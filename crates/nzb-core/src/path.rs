//! Path validation shared by download and post-processing boundaries.

use std::path::{Path, PathBuf};

/// Join an untrusted archive or API supplied relative name beneath `root`.
/// Backslashes are treated as separators on every platform.
pub fn safe_join(root: &Path, name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.as_bytes().get(1) == Some(&b':')
        || normalized.chars().any(char::is_control)
    {
        return None;
    }

    let mut output = root.to_path_buf();
    for component in normalized.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            component => output.push(component),
        }
    }
    output.starts_with(root).then_some(output)
}

/// Validate a user supplied directory or category component.
/// These values are intentionally a single path component.
pub fn safe_component(value: &str) -> Option<&str> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value)
}

/// Resolve a category `output_dir` override to the root its jobs land in.
///
/// A relative override is joined beneath `complete_dir` with [`safe_join`].
/// An absolute override is used as is, provided it has no `..` component
/// and no control characters. Returns `None` for anything else, including
/// an empty override.
pub fn category_output_root(complete_dir: &Path, output_dir: &Path) -> Option<PathBuf> {
    if output_dir.is_absolute() {
        let clean = !output_dir
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
            && !output_dir.to_string_lossy().chars().any(char::is_control);
        return clean.then(|| output_dir.to_path_buf());
    }
    safe_join(complete_dir, &output_dir.to_string_lossy())
}

/// Maximum length, in bytes, of a sanitized job name. Leaves headroom under
/// the common 255-byte `NAME_MAX` for suffixes such as `.1` or `_UNPACK_`.
pub const MAX_JOB_NAME_BYTES: usize = 240;

/// Job name used when sanitization leaves nothing usable.
pub const UNNAMED_JOB: &str = "unnamed";

/// Turn an untrusted job name (usually an NZB filename stem) into a single
/// directory name that is valid on Linux, macOS, Windows and SMB shares.
///
/// Rules, applied in order:
/// - Unicode NFC normalization.
/// - `:` becomes `-`, or ` -` when it is followed by whitespace, so
///   `Star Trek: Discovery` reads `Star Trek - Discovery`.
/// - The other Windows-illegal characters `< > " / \ | ? *` and control
///   characters become `_`.
/// - Leading and trailing whitespace and dots are trimmed.
/// - Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
///   `LPT1`-`LPT9`, case-insensitive, with or without an extension) get a
///   `_` prefix.
/// - The result is truncated to [`MAX_JOB_NAME_BYTES`] on a char boundary.
/// - An empty result becomes [`UNNAMED_JOB`].
///
/// The output always passes [`safe_component`] and [`safe_join`], and
/// sanitizing an already sanitized name returns it unchanged.
pub fn sanitize_job_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let normalized: String = name.nfc().collect();
    let mut replaced = String::with_capacity(normalized.len());
    let mut chars = normalized.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ':' => {
                let spaced = chars.peek().is_some_and(|next| next.is_whitespace())
                    && !replaced.ends_with(char::is_whitespace);
                replaced.push_str(if spaced { " -" } else { "-" });
            }
            '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*' => replaced.push('_'),
            c if c.is_control() => replaced.push('_'),
            c => replaced.push(c),
        }
    }

    let trim = |s: &str| -> String {
        s.trim_matches(|c: char| c.is_whitespace() || c == '.')
            .to_string()
    };
    let mut out = trim(&replaced);
    if out.is_empty() {
        return UNNAMED_JOB.to_string();
    }
    if is_windows_reserved_name(&out) {
        out.insert(0, '_');
    }
    if out.len() > MAX_JOB_NAME_BYTES {
        let mut end = MAX_JOB_NAME_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        // Truncation may expose a trailing dot or space; the first char is
        // neither, so this can never empty the name.
        out = trim(&out);
    }
    out
}

/// Whether `name` is a Windows reserved device name. Windows ignores the
/// extension and trailing spaces, so `con.txt` and `NUL .x` count too.
fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let bytes = upper.as_bytes();
            bytes.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && (b'1'..=b'9').contains(&bytes[3])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_cross_platform_escape_paths() {
        let root = Path::new("/tmp/job");
        assert!(safe_join(root, "folder/file.txt").is_some());
        for path in ["../outside", r"..\outside", "/etc/passwd", r"C:\temp"] {
            assert!(safe_join(root, path).is_none(), "{path}");
        }
        assert!(safe_component("movies").is_some());
        assert!(safe_component("../outside").is_none());
        assert!(safe_component(r"movies\tv").is_none());
    }

    #[test]
    fn category_output_root_accepts_subdirs_and_clean_absolute_roots() {
        let complete = Path::new("/data/complete");
        assert_eq!(
            category_output_root(complete, Path::new("shows/tv")),
            Some(PathBuf::from("/data/complete/shows/tv"))
        );
        assert_eq!(
            category_output_root(complete, Path::new("/srv/media")),
            Some(PathBuf::from("/srv/media"))
        );
        for unsafe_dir in [
            "",
            "../../../etc",
            "shows/../../etc",
            "/srv/../etc",
            "/srv/a\nb",
        ] {
            assert!(
                category_output_root(complete, Path::new(unsafe_dir)).is_none(),
                "{unsafe_dir:?}"
            );
        }
    }

    #[test]
    fn sanitize_job_name_replaces_windows_illegal_characters() {
        assert_eq!(sanitize_job_name("a:b*c?d"), "a-b_c_d");
        assert_eq!(
            sanitize_job_name("Star Trek: Discovery"),
            "Star Trek - Discovery"
        );
        assert_eq!(sanitize_job_name(r#"x<y>z"q|r"#), "x_y_z_q_r");
        assert_eq!(sanitize_job_name("tab\there"), "tab_here");
        assert_eq!(sanitize_job_name("C:"), "C-");
    }

    #[test]
    fn sanitize_job_name_neutralizes_traversal_and_separators() {
        assert_eq!(sanitize_job_name("."), UNNAMED_JOB);
        assert_eq!(sanitize_job_name(".."), UNNAMED_JOB);
        assert_eq!(sanitize_job_name(""), UNNAMED_JOB);
        assert_eq!(sanitize_job_name(" . "), UNNAMED_JOB);
        assert_eq!(sanitize_job_name("../etc/passwd"), "_etc_passwd");
        assert_eq!(sanitize_job_name(r"..\outside"), "_outside");
        assert_eq!(sanitize_job_name("/abs"), "_abs");
    }

    #[test]
    fn sanitize_job_name_trims_dots_and_whitespace() {
        assert_eq!(sanitize_job_name("Trailing.Dot."), "Trailing.Dot");
        assert_eq!(
            sanitize_job_name(" lead and trail space "),
            "lead and trail space"
        );
        assert_eq!(sanitize_job_name(".hidden"), "hidden");
        assert_eq!(sanitize_job_name("Show.S01E01.1080p"), "Show.S01E01.1080p");
    }

    #[test]
    fn sanitize_job_name_prefixes_windows_reserved_names() {
        for (input, expected) in [
            ("CON", "_CON"),
            ("con", "_con"),
            ("Nul.txt", "_Nul.txt"),
            ("aux.tar.gz", "_aux.tar.gz"),
            ("COM1", "_COM1"),
            ("lpt9.log", "_lpt9.log"),
            ("NUL .x", "_NUL .x"),
        ] {
            assert_eq!(sanitize_job_name(input), expected, "{input}");
        }
        for kept in ["CONSOLE", "COM0", "COM10", "LPT", "Conan", "NULL.x"] {
            assert_eq!(sanitize_job_name(kept), kept);
        }
    }

    #[test]
    fn sanitize_job_name_truncates_on_a_char_boundary() {
        let long = "x".repeat(300);
        assert_eq!(sanitize_job_name(&long), "x".repeat(MAX_JOB_NAME_BYTES));

        // 'é' is two bytes; 239 ASCII bytes leave a split char at 240.
        let multibyte = format!("{}{}", "a".repeat(239), "é".repeat(10));
        let out = sanitize_job_name(&multibyte);
        assert!(out.len() <= MAX_JOB_NAME_BYTES);
        assert_eq!(out, "a".repeat(239));

        // A dot exposed by truncation is trimmed again.
        let dotted = format!("{}. tail", "b".repeat(MAX_JOB_NAME_BYTES - 1));
        assert_eq!(
            sanitize_job_name(&dotted),
            "b".repeat(MAX_JOB_NAME_BYTES - 1)
        );
    }

    #[test]
    fn sanitize_job_name_output_is_safe_and_idempotent() {
        let root = Path::new("/tmp/complete");
        for input in [
            "a:b*c?d",
            "..",
            ".",
            "../x",
            "C:\\temp",
            "CON",
            " x. ",
            "\u{0}nul",
            "café",
            "Star Trek: Discovery",
        ] {
            let once = sanitize_job_name(input);
            assert!(safe_component(&once).is_some(), "{input:?} -> {once:?}");
            assert!(safe_join(root, &once).is_some(), "{input:?} -> {once:?}");
            assert_eq!(sanitize_job_name(&once), once, "{input:?}");
        }
        // NFD input is normalized to NFC.
        assert_eq!(sanitize_job_name("cafe\u{0301}"), "caf\u{00E9}");
    }
}
