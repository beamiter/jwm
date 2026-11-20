#!/usr/bin/env bash
# Offline portal installer path checks; never restart or signal host processes.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-portal-install-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPO="$TEST_ROOT/project"
mkdir -p "$REPO/scripts" "$REPO/portal/dist" "$TEST_ROOT/bin"
# Redirect only the hard-coded distro metadata path in this private copy.
sed "s|^NOBLE_PORTAL_METADATA=.*|NOBLE_PORTAL_METADATA=\"$TEST_ROOT/system/jwm.portal\"|" \
    "$ROOT/scripts/install-portal.sh" > "$REPO/scripts/install-portal.sh"
cp "$ROOT/scripts/ensure_pipewire.sh" "$REPO/scripts/"
cp "$ROOT/portal/dist/"{jwm-portal.service,jwm.portal,jwm-portals.conf} "$REPO/portal/dist/"
cat > "$TEST_ROOT/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
touch "$HOME/cargo-called"
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
    printf '#!/bin/sh\ntouch "$HOME/service-called"\n' > "$TEST_ROOT/bin/$command"
done
cat > "$TEST_ROOT/bin/dpkg-query" <<'STUB'
#!/usr/bin/env bash
[[ $# == 3 && $1 == -W && $2 == '-f=${db:Status-Status} ${Version}' &&
   $3 == xdg-desktop-portal && -n ${TEST_PORTAL_PACKAGE:-} ]] || exit 1
printf '%s' "$TEST_PORTAL_PACKAGE"
STUB
cat > "$TEST_ROOT/bin/pkg-config" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
    --modversion) printf '1.6.10\n' ;;
    --atleast-version=*|--exists) exit 0 ;;
    *) exit 1 ;;
esac
STUB
chmod +x "$TEST_ROOT/bin/"*
fail() { printf 'test-install-portal: FAIL: %s\n' "$*" >&2; exit 1; }
export TMPDIR="$TEST_ROOT"
for mode in default custom relative noble-missing noble-wrong noble-fifo noble-packaged modern removed unknown-vendor initial-noble older-noble; do
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
    pkg=''
    case "$mode" in
        noble-*) pkg='installed 1.18.4-1ubuntu2.24.04.3' ;;
        modern) pkg='installed 1.20.1-1ubuntu2.24.04.3' ;;
        removed) pkg='config-files 1.18.4-1ubuntu2.24.04.3' ;;
        unknown-vendor) pkg='installed 1.18.4-1debian1' ;;
        initial-noble) pkg='installed 1.18.4-1ubuntu2' ;;
        older-noble) pkg='installed 1.18.3-1ubuntu1' ;;
    esac
    mkdir -p "$TEST_ROOT/system"
    case "$mode" in
        noble-wrong) printf 'wrong metadata\n' > "$TEST_ROOT/system/jwm.portal" ;;
        noble-fifo) mkfifo "$TEST_ROOT/system/jwm.portal" ;;
        noble-packaged) cp "$ROOT/portal/dist/jwm.portal" "$TEST_ROOT/system/" ;;
    esac
    rc=0
    PATH="$TEST_ROOT/bin:$PATH" HOME="$case_dir/home" XDG_DATA_HOME="$data_env" XDG_CONFIG_HOME="$config_env" \
        TEST_PORTAL_PACKAGE="$pkg" CARGO_TARGET_DIR="$case_dir/target" \
        timeout 5s bash "$REPO/scripts/install-portal.sh" > "$case_dir/output.log" 2>&1 || rc=$?
    rm -f -- "$TEST_ROOT/system/jwm.portal"
    if [[ $mode == noble-missing || $mode == noble-wrong || $mode == noble-fifo ]]; then
        [[ $rc == 1 ]] || fail "$mode must fail promptly before installation (rc=$rc)"
        [[ -z $(ls -A "$case_dir/home") && ! -e $case_dir/target ]] || fail "$mode caused side effects"
        grep -Fq 'does not discover the per-user jwm.portal' "$case_dir/output.log" || fail "$mode missing diagnosis"
        grep -Fq 'portal/dist/jwm.portal' "$case_dir/output.log" || fail "$mode missing packaging guidance"
        continue
    fi
    [[ $rc == 0 && -f $case_dir/home/cargo-called && -f $case_dir/home/service-called ]] ||
        fail "$mode installation or restart stub failed"
    [[ -x $case_dir/home/.local/libexec/jwm-portal ]] || fail "$mode binary missing"
    [[ -f $data/dbus-1/services/org.freedesktop.impl.portal.desktop.jwm.service ]] || fail "$mode D-Bus service missing"
    [[ -f $data/xdg-desktop-portal/portals/jwm.portal ]] || fail "$mode portal definition missing"
    for name in jwm JWM; do
        cmp "$ROOT/portal/dist/jwm-portals.conf" "$config/xdg-desktop-portal/$name-portals.conf" || fail "$mode config differs"
    done
    [[ $mode != custom || ! -e $case_dir/home/.config && ! -e $case_dir/home/.local/share ]] || fail 'custom XDG paths ignored'
done
printf 'test-install-portal: PASS (12 cases)\n'
