# Shared by ci/tasks/sync-crate and ci/tasks/crate-drift; source it, do not
# run it. POSIX sh only: the runners' /bin/sh is dash.
#
# crate_export <crate> <dest> builds the tree a standalone repo should carry
# for crates/<crate> at HEAD: the tracked files (git archive, so no target/),
# with every Cargo.toml rewritten so it builds outside this workspace, and a
# [workspace] table added when the crate has nested member crates. Both tasks
# compare against or ship exactly this tree, so the sync and the drift gate
# cannot disagree about what "in sync" means.
#
# Expects to run from the monorepo root.

# crate_export_manifest <file> <crate> — rewrite one exported Cargo.toml in
# place:
#   <crate> = { workspace = true }   ->  <crate> = { path = ".." }
#       (a nested member such as nzb-decode's ffi/ depending on its parent)
#   <dep>.workspace = true           ->  <dep> = <[workspace.dependencies] value>
#   { version = "x", path = "../y" } ->  { version = "x" }
#       (sibling nzb-* crates resolve from crates.io in the standalone repo)
# Any other workspace inheritance, a sibling path without a version, or a
# sibling requirement below that sibling's current version is a hard error rather than a silently unbuildable manifest.
crate_export_manifest() {
    awk -v crate="$2" -v file="$1" '
        function fail(msg) {
            printf "crate-export: %s:%d: %s\n", file, FNR, msg > "/dev/stderr"
            exit 1
        }
        FNR == NR {
            if ($0 ~ /^\[/) { inws = ($0 ~ /^\[workspace\.dependencies\][ \t]*$/); next }
            if (inws && $0 ~ /^[A-Za-z0-9_-]+[ \t]*=/) {
                name = $0; sub(/[ \t]*=.*/, "", name)
                val = $0; sub(/^[^=]*=[ \t]*/, "", val)
                ws[name] = val
            }
            next
        }
        {
            line = $0
            self = "^" crate "[ \t]*=[ \t]*\\{[ \t]*workspace[ \t]*=[ \t]*true[ \t]*\\}"
            if (line ~ self) {
                print crate " = { path = \"..\" }"
                next
            }
            if (line ~ /^[A-Za-z0-9_-]+\.workspace[ \t]*=[ \t]*true[ \t]*$/) {
                name = line; sub(/\..*/, "", name)
                if (!(name in ws)) fail(name " inherits from the workspace but [workspace.dependencies] has no " name)
                val = ws[name]
                if (val ~ /path[ \t]*=/) fail(name ": the workspace entry is a path dependency")
                if (val ~ /^\{/ && val !~ /\}/) fail(name ": multi-line workspace entries are not supported")
                print name " = " val
                next
            }
            if (line ~ /workspace[ \t]*=[ \t]*true/) fail("unsupported workspace inheritance: " line)
            if (line ~ /path[ \t]*=[ \t]*"\.\.\//) {
                if (line !~ /version[ \t]*=/) fail("sibling path dependency without a version: " line)
                # Here the path wins, so a stale requirement still builds; in
                # the standalone repo it resolves from crates.io and may pick
                # a sibling that lacks what this crate uses.
                match(line, /path[ \t]*=[ \t]*"\.\.\/[^"]*"/)
                sib = substr(line, RSTART, RLENGTH)
                sub(/^[^"]*"\.\.\//, "", sib); sub(/"$/, "", sib)
                mf = "git show HEAD:crates/" sib "/Cargo.toml"
                want = ""
                while ((mf | getline l) > 0)
                    if (want == "" && l ~ /^version[ \t]*=/) { want = l; sub(/^[^"]*"/, "", want); sub(/".*/, "", want) }
                close(mf)
                have = line; sub(/.*version[ \t]*=[ \t]*"/, "", have); sub(/".*/, "", have)
                if (want != "" && have != want) fail(sib " requirement " have " is not its current version " want)
                gsub(/,[ \t]*path[ \t]*=[ \t]*"\.\.\/[^"]*"/, "", line)
                gsub(/path[ \t]*=[ \t]*"\.\.\/[^"]*"[ \t]*,[ \t]*/, "", line)
            }
            print line
        }
    ' Cargo.toml "$1" >"$1.export" || { rm -f "$1.export"; return 1; }
    mv "$1.export" "$1"
}

crate_export() {
    _crate=$1
    _dest=$2
    mkdir -p "$_dest"
    git archive "HEAD:crates/$_crate" | tar -x -C "$_dest"
    for _manifest in $(find "$_dest" -name Cargo.toml | LC_ALL=C sort); do
        crate_export_manifest "$_manifest" "$_crate" || return 1
    done
    # Nested crates (crates/<crate>/<dir>/Cargo.toml) are workspace members in
    # the standalone repo. A [workspace] table cannot live in the crate's own
    # manifest here — this repo's root workspace owns it — so it is added on
    # export.
    _members=$(cd "$_dest" && find . -mindepth 2 -maxdepth 2 -name Cargo.toml \
        | sed 's#^\./##; s#/Cargo\.toml$##; s#.*#"&"#' | LC_ALL=C sort | paste -sd, - | sed 's/,/, /g')
    if [ -n "$_members" ] && ! grep -q '^\[workspace\]' "$_dest/Cargo.toml"; then
        printf '\n[workspace]\nmembers = [%s]\n' "$_members" >>"$_dest/Cargo.toml"
    fi
}

# preserved_path <path> — true when ci/sync-preserve.txt keeps <path> in the
# standalone repo (an exact match or anything under a listed prefix).
preserved_path() {
    [ -f ci/sync-preserve.txt ] || return 1
    while IFS= read -r _rule; do
        case "$_rule" in ''|\#*) continue ;; esac
        case "$1" in "$_rule"|"$_rule"/*) return 0 ;; esac
    done <ci/sync-preserve.txt
    return 1
}

# sync_accepted <crate> <full-sha> — print the reason ci/sync-accept.txt gives
# for letting a sync overwrite that standalone commit; false when unlisted.
sync_accepted() {
    [ -f ci/sync-accept.txt ] || return 1
    while read -r _c _sha _reason; do
        case "$_c" in ''|\#*) continue ;; esac
        [ "$_c" = "$1" ] || continue
        [ "${#_sha}" -ge 7 ] || continue
        case "$2" in "$_sha"*) printf '%s\n' "$_reason"; return 0 ;; esac
    done <ci/sync-accept.txt
    return 1
}
