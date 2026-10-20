//! A stand-in `unrar` for post-processing tests (WI-1757).
//!
//! Real RAR sets can't be built in tests (`rar` is proprietary) and `unrar`
//! may not be installed, so tests register what an archive should look like
//! and put this script first on `PATH`. The script behaves like unrar 7 for
//! the arguments rustnzb passes (`x -o+ -y -p… -ai -idp <archive> <out>`):
//!
//! - With no arguments it prints an unrar 7 banner, so the password goes on
//!   stdin.
//! - It checks every registered volume against its MD5 and fails with a
//!   checksum error when one differs. A run that succeeds therefore shows
//!   that PAR2 repair happened before extraction.
//! - When a password is registered, a missing or wrong one fails with
//!   unrar's `Incorrect password` message and exit code 11.
//! - On success it writes `<stem>.mkv` into the output directory, holding
//!   the volumes concatenated in name order.
//!
//! Archives are keyed by stem: `name.part01.rar` and `name.rar` both have
//! the stem `name`. Unix only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use md5::{Digest, Md5};

/// The banner unrar 7 prints when run with no arguments.
const BANNER: &str = "UNRAR 7.01 freeware      Copyright (c) 1993-2024 Alexander Roshal";

/// The installed fake. Get it with [`install`].
pub struct FakeUnrar {
    dir: PathBuf,
}

/// Install the fake `unrar` first on this process's `PATH`, once, and return
/// it. Call this at the start of every test that unpacks a RAR, before any
/// extraction runs.
pub fn install() -> &'static FakeUnrar {
    static FAKE: OnceLock<FakeUnrar> = OnceLock::new();
    FAKE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("rustnzb-fake-unrar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("create fake unrar dir");
        std::fs::create_dir_all(dir.join("archives")).expect("create fake unrar archives dir");
        write_script(&dir.join("bin/unrar"), &script(&dir.join("archives")));

        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let joined = std::env::join_paths(paths).expect("join PATH");
        // SAFETY: this runs once, inside `OnceLock::get_or_init`. std's
        // `set_var`, `var` and `Command::spawn` share one environment lock,
        // and nothing in these tests reads the environment through libc
        // directly, so no other thread can see a torn environment.
        unsafe { std::env::set_var("PATH", joined) };
        FakeUnrar { dir }
    })
}

impl FakeUnrar {
    /// Register the archive `stem`: the exact bytes each volume must hold
    /// (name, contents), and the password it needs, if any.
    pub fn register(&self, stem: &str, volumes: &[(&str, &[u8])], password: Option<&str>) {
        let archive = self.dir.join("archives").join(stem);
        let _ = std::fs::remove_dir_all(&archive);
        std::fs::create_dir_all(&archive).expect("create archive entry");
        let mut sums = String::new();
        for (name, bytes) in volumes {
            let digest = Md5::digest(bytes);
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            sums.push_str(&format!("{hex}  {name}\n"));
        }
        std::fs::write(archive.join("volumes"), sums).expect("write volume sums");
        if let Some(password) = password {
            std::fs::write(archive.join("password"), password).expect("write password");
        }
    }

    /// The file the fake writes for `stem` on success: the volumes joined.
    pub fn expected_output(volumes: &[(&str, &[u8])]) -> Vec<u8> {
        let mut sorted = volumes.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        sorted
            .iter()
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect()
    }
}

fn script(archives: &Path) -> String {
    format!(
        r#"#!/bin/sh
if [ $# -eq 0 ]; then printf '%s\n' '{BANNER}'; exit 0; fi
archive=""; out=""; pflag=""
for a; do
  case "$a" in
    -p*) pflag="$a" ;;
    *.rar) archive="$a" ;;
  esac
  out="$a"
done
dir=$(dirname "$archive")
stem=$(basename "$archive" | sed -E 's/\.part[0-9]+\.rar$//; s/\.rar$//')
entry='{archives}'/"$stem"
[ -f "$entry/volumes" ] || {{ echo "Cannot open $archive" >&2; exit 10; }}
if [ -f "$entry/password" ]; then
  case "$pflag" in
    -p-) pw="" ;;
    -p) printf 'Enter password (will not be echoed) for %s.mkv: ' "$stem" >&2; IFS= read -r pw ;;
    *) pw="${{pflag#-p}}" ;;
  esac
  if [ "$pw" != "$(cat "$entry/password")" ]; then
    echo "Incorrect password for $stem.mkv" >&2; exit 11
  fi
fi
(cd "$dir" && md5sum -c --quiet "$entry/volumes") >/dev/null 2>&1 || {{
  echo "$stem.mkv - checksum error" >&2; exit 3
}}
mkdir -p "$out"
(cd "$dir" && cat $(awk '{{print $2}}' "$entry/volumes" | sort)) > "$out/$stem.mkv"
echo "All OK"
"#,
        archives = archives.display()
    )
}

/// Write an executable script from a child `sh`, so the open write handle
/// never lives in this process. Otherwise another test thread can fork while
/// it is open, the child inherits it, and running the script fails with
/// "Text file busy" (ETXTBSY).
fn write_script(path: &Path, contents: &str) {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh to write the fake unrar");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(contents.as_bytes())
        .expect("write fake unrar");
    assert!(child.wait().expect("wait for sh").success());
}
