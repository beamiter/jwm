#!/usr/bin/env bash
# Invoked by env -i from the workflow; source checkout is fixed and unmodified.
set -euo pipefail
fixture=$(cd "$(dirname "$0")" && pwd)
root=$(realpath "$1")
evidence=$(realpath "$2")
expected=c994017ba60687c7209e2fc6d451be8745cfdea2
cd "$root"
test "$(git -c core.fsmonitor=false rev-parse HEAD)" = "$expected"
test -z "$(git -c core.fsmonitor=false status --porcelain --untracked-files=all)"
test ! -e target
cargo --version --verbose
rustc --version --verbose
cargo build --locked --release --no-default-features --features backend-wayland-nested --bin jwm --bin jwm-tool
CARGO_TARGET_DIR="$root/target/native" cargo build --locked --manifest-path crates/jwm_shell/Cargo.toml --example native_shell_client
test "$(git -c core.fsmonitor=false rev-parse HEAD)" = "$expected"
test -z "$(git -c core.fsmonitor=false status --porcelain --untracked-files=all)"
/usr/bin/python3 -I - "$root" "$evidence" "$expected" "$fixture" <<'PY'
import hashlib,json,pathlib,sys
root,out,sha=pathlib.Path(sys.argv[1]),pathlib.Path(sys.argv[2]),sys.argv[3]
paths={'jwm':root/'target/release/jwm','jwm-tool':root/'target/release/jwm-tool',
       'native_shell_client':root/'target/native/debug/examples/native_shell_client'}
for p in paths.values():
    assert p.is_file() and p.resolve()==p,p
(out/'build.json').write_text(json.dumps({'source':sha,'build_exit_code':0,
    'host_identity_sha256':hashlib.sha256((pathlib.Path(sys.argv[4])/'host_identity.py').read_bytes()).hexdigest(),
    'binaries':{n:hashlib.sha256(p.read_bytes()).hexdigest() for n,p in paths.items()}},indent=2))
PY
