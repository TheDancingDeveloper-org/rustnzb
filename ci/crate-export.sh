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
#   <dep> = { workspace = true, k = v, ... }
#                                    ->  the workspace value merged with the
#       local keys: features are added to the workspace features, any other
#       key (optional, default-features) is set or overridden
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
        # inline_split <inner> <keys> <vals> — split the inside of an inline
        # table into key/value pairs on the commas outside strings and arrays.
        function inline_split(s, keys, vals,    n, i, c, depth, q, cur) {
            n = 0; depth = 0; q = 0; cur = ""
            s = s ","
            for (i = 1; i <= length(s); i++) {
                c = substr(s, i, 1)
                if (c == "\"") q = !q
                else if (!q && c == "[") depth++
                else if (!q && c == "]") depth--
                if (!q && depth == 0 && c == ",") {
                    if (cur ~ /=/) {
                        n++
                        keys[n] = cur; sub(/[ \t]*=.*/, "", keys[n]); sub(/^[ \t]*/, "", keys[n])
                        vals[n] = cur; sub(/^[^=]*=[ \t]*/, "", vals[n]); sub(/[ \t]*$/, "", vals[n])
                    } else if (cur !~ /^[ \t]*$/) fail("cannot parse inline table entry: " cur)
                    cur = ""
                    continue
                }
                cur = cur c
            }
            return n
        }
        # feature_union <a> <b> — the array <a> with the items of <b> it lacks.
        function feature_union(a, b,    na, nb, ia, ib, i, out, seen) {
            sub(/^\[[ \t]*/, "", a); sub(/[ \t,]*\][ \t]*$/, "", a)
            sub(/^\[[ \t]*/, "", b); sub(/[ \t,]*\][ \t]*$/, "", b)
            na = (a == "") ? 0 : split(a, ia, /[ \t]*,[ \t]*/)
            nb = (b == "") ? 0 : split(b, ib, /[ \t]*,[ \t]*/)
            out = ""
            for (i = 1; i <= na; i++) if (!(ia[i] in seen)) { seen[ia[i]] = 1; out = out (out == "" ? "" : ", ") ia[i] }
            for (i = 1; i <= nb; i++) if (!(ib[i] in seen)) { seen[ib[i]] = 1; out = out (out == "" ? "" : ", ") ib[i] }
            return "[" out "]"
        }
        # ws_value <name> — the [workspace.dependencies] value for <name>,
        # checked to be a single-line registry dependency.
        function ws_value(name,    val) {
            if (!(name in ws)) fail(name " inherits from the workspace but [workspace.dependencies] has no " name)
            val = ws[name]
            if (val ~ /path[ \t]*=/) fail(name ": the workspace entry is a path dependency")
            if (val ~ /^\{/ && val !~ /\}/) fail(name ": multi-line workspace entries are not supported")
            return val
        }
        # inline_inherit <name> <inner> — <name> = { workspace = true, ... }
        # resolved against the workspace entry.
        function inline_inherit(name, inner,    val, wn, wk, wv, ln, lk, lv, i, j, found, out) {
            val = ws_value(name)
            ln = inline_split(inner, lk, lv)
            if (ln == 1) return name " = " val
            if (val ~ /^"/) { wn = 1; wk[1] = "version"; wv[1] = val }
            else {
                sub(/^\{[ \t]*/, "", val); sub(/[ \t]*\}[ \t]*$/, "", val)
                wn = inline_split(val, wk, wv)
            }
            for (i = 1; i <= ln; i++) {
                if (lk[i] == "workspace") continue
                found = 0
                for (j = 1; j <= wn; j++) if (wk[j] == lk[i]) {
                    found = 1
                    wv[j] = (lk[i] == "features") ? feature_union(wv[j], lv[i]) : lv[i]
                }
                if (!found) { wn++; wk[wn] = lk[i]; wv[wn] = lv[i] }
            }
            out = ""
            for (j = 1; j <= wn; j++) out = out (j > 1 ? ", " : "") wk[j] " = " wv[j]
            return name " = { " out " }"
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
            if ($0 ~ /^\[/) indev = ($0 ~ /^\[dev-dependencies\]/)
            line = $0
            self = "^" crate "[ \t]*=[ \t]*\\{[ \t]*workspace[ \t]*=[ \t]*true[ \t]*\\}"
            if (line ~ self) {
                print crate " = { path = \"..\" }"
                next
            }
            if (line ~ /^[A-Za-z0-9_-]+\.workspace[ \t]*=[ \t]*true[ \t]*$/) {
                name = line; sub(/\..*/, "", name)
                print name " = " ws_value(name)
                next
            }
            if (line ~ /^[A-Za-z0-9_-]+[ \t]*=[ \t]*\{(.*,)?[ \t]*workspace[ \t]*=[ \t]*true[ \t]*(,.*)?\}[ \t]*$/) {
                name = line; sub(/[ \t]*=.*/, "", name)
                if (indev && (name in ws) && ws[name] ~ /path[ \t]*=/) next
                inner = line; sub(/^[^{]*\{[ \t]*/, "", inner); sub(/[ \t]*\}[ \t]*$/, "", inner)
                print inline_inherit(name, inner)
                next
            }
            if (line ~ /workspace[ \t]*=[ \t]*true/) fail("unsupported workspace inheritance: " line)
            if (line ~ /path[ \t]*=[ \t]*"\.\.\//) {
                # A dev-only sibling with no version (the mock server) is not
                # part of the published crate. Drop it rather than invent a
                # crates.io requirement.
                if (indev && line !~ /version[ \t]*=/) next
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
