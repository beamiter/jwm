#!/usr/bin/env bash
# Same-run cold source/receipt construction. Call with env -i as documented.
set -euo pipefail
root=$(realpath "$1")
out=$(realpath -m "$2")
role=$3
sha=$4
base=9b3974b31566a2ae1bb6fe33cfb9642762434bf6
candidate=c2fec065c4a891712e7a4f34eb5dfbb8a6893cb6
tree=3e3a3ccd47b62de19702dcc5774cfca2fe1391ea
[[ "$sha" =~ ^[0-9a-f]{40}$ ]]
mkdir -m 700 "$out"
cd "$root"
test "$(git -c core.fsmonitor=false rev-parse HEAD)" = "$sha"
test -z "$(git -c core.fsmonitor=false status --porcelain --untracked-files=all)"
actual_tree=$(git rev-parse 'HEAD^{tree}')
case "$role" in
  baseline) test "$sha" = "$base";;
  candidate) test "$sha" = "$candidate"; test "$actual_tree" = "$tree";;
  *) exit 2;;
esac
test ! -e target
cargo --version
rustc --version
cargo build --locked --release --no-default-features --features backend-x11rb --bin jwm
test "$(git rev-parse HEAD)" = "$sha"
test -z "$(git -c core.fsmonitor=false status --porcelain --untracked-files=all)"
binary=$root/target/release/jwm
test -x "$binary"
test "$(realpath "$binary")" = "$binary"
/usr/bin/python3 -I - "$binary" "$sha" "$actual_tree" "$out" <<'PY'
import hashlib,json,pathlib,sys
b,sha,tree,out=sys.argv[1:]
with open(b,'rb') as f:
    digest=hashlib.file_digest(f,'sha256').hexdigest()
(pathlib.Path(out)/'build.json').write_text(json.dumps(
    {'source':sha,'tree':tree,'binary_sha256':digest,'build_exit_code':0},indent=2)+'\n')
PY
