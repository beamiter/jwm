#!/usr/bin/env bash
# Offline portal installer path checks; never restart or signal host processes.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-portal-install-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPO="$TEST_ROOT/project"
mkdir -p "$REPO/scripts" "$REPO/portal/dist" "$TEST_ROOT/bin"
cp "$ROOT/scripts/install-portal.sh" "$REPO/scripts/"
cp "$ROOT/portal/dist/"{jwm-portal.service,jwm.portal,jwm-portals.conf} "$REPO/portal/dist/"
cat > "$TEST_ROOT/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
while (($#)); do
    if [[ $1 == --target-dir ]]; then
        target=$2
        shift 2
    else
        shift
    fi
done
mkdir -p "$target/release"
printf '#!/bin/sh\nexit 0\n' > "$target/release/jwm-portal"
chmod +x "$target/release/jwm-portal"
STUB
for command in systemctl pkill; do
    printf '#!/bin/sh\nexit 0\n' > "$TEST_ROOT/bin/$command"
done
chmod +x "$TEST_ROOT/bin/"*
fail() { printf 'test-install-portal: FAIL: %s\n' "$*" >&2; exit 1; }
for mode in default custom relative; do
    case_dir="$TEST_ROOT/$mode"
    mkdir -p "$case_dir/home"
    data="$case_dir/home/.local/share"
    config="$case_dir/home/.config"
    data_env='' config_env=''
    if [[ $mode == custom ]]; then
        data="$case_dir/data space"
        config="$case_dir/config space"
        data_env=$data config_env=$config
    elif [[ $mode == relative ]]; then
        data_env=relative-data config_env=relative-config
    fi
    PATH="$TEST_ROOT/bin:$PATH" HOME="$case_dir/home" XDG_DATA_HOME="$data_env" XDG_CONFIG_HOME="$config_env" \
        CARGO_TARGET_DIR="$case_dir/target" bash "$REPO/scripts/install-portal.sh" > "$case_dir/output.log" 2>&1
    [[ -x $case_dir/home/.local/libexec/jwm-portal ]] || fail "$mode binary missing"
    [[ -f $data/dbus-1/services/org.freedesktop.impl.portal.desktop.jwm.service ]] || fail "$mode D-Bus service missing"
    [[ -f $data/xdg-desktop-portal/portals/jwm.portal ]] || fail "$mode portal definition missing"
    for name in jwm JWM; do
        cmp "$ROOT/portal/dist/jwm-portals.conf" "$config/xdg-desktop-portal/$name-portals.conf" || fail "$mode config differs"
    done
    [[ $mode != custom || ! -e $case_dir/home/.config && ! -e $case_dir/home/.local/share ]] || fail 'custom XDG paths ignored'
done
printf 'test-install-portal: PASS\n'
