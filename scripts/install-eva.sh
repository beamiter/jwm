#!/usr/bin/env bash
# Build/install Eva into a user prefix; preserve an existing configuration.
set -euo pipefail

eva_repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
eva_prefix="${JWM_EVA_PREFIX:-$HOME/.local}"
eva_config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/jwm"
eva_unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
eva_build=true
eva_enable=true
eva_profile=release
eva_binary=

usage() {
    cat <<'HELP'
Usage: scripts/install-eva.sh [options]
  --prefix DIR       User installation prefix (default ~/.local)
  --config-dir DIR   Eva config directory (default ~/.config/jwm)
  --unit-dir DIR     systemd user unit directory
  --binary FILE      Install an existing binary; implies --no-build
  --no-build         Use target/release/jwm-eva (or target/debug with --debug)
  --debug            Build/use the debug profile
  --no-enable        Install files without contacting the user service manager
  -h, --help         Show this help
Existing eva.toml is preserved. No API request or microphone capture is started
by installation. Enabling starts the daemon in Idle.
HELP
}
need_value() {
    if [[ $# -lt 2 || -z $2 ]]; then
        printf 'install-eva: missing value for %s\n' "$1" >&2
        exit 2
    fi
}
while (($#)); do
    case "$1" in
        --prefix) need_value "$@"; eva_prefix=$2; shift 2 ;;
        --config-dir) need_value "$@"; eva_config_dir=$2; shift 2 ;;
        --unit-dir) need_value "$@"; eva_unit_dir=$2; shift 2 ;;
        --binary) need_value "$@"; eva_binary=$2; eva_build=false; shift 2 ;;
        --no-build) eva_build=false; shift ;;
        --no-enable) eva_enable=false; shift ;;
        --debug) eva_profile=debug; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'install-eva: unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done
for eva_directory in "$eva_prefix" "$eva_config_dir" "$eva_unit_dir"; do
    if [[ $eva_directory != /* || $eva_directory == *$'\n'* || $eva_directory == *$'\r'* ]]; then
        printf 'install-eva: installation directories must be absolute, single-line paths\n' >&2
        exit 2
    fi
done

eva_target_dir=${CARGO_TARGET_DIR:-$eva_repo/target}
if [[ $eva_target_dir != /* ]]; then eva_target_dir="$eva_repo/$eva_target_dir"; fi
if [[ $eva_build == true ]]; then
    if ! command -v pkg-config >/dev/null || ! pkg-config --exists libpulse; then
        printf 'install-eva: native PulseAudio headers are required (Debian/Ubuntu: libpulse-dev)\n' >&2
        exit 1
    fi
    eva_pipewire_environment=$(bash "$eva_repo/scripts/ensure_pipewire.sh" --env)
    while IFS= read -r eva_setting; do
        case "$eva_setting" in
            PKG_CONFIG_PATH=*|JWM_PIPEWIRE_PREFIX=*) export "${eva_setting?}" ;;
            '') ;;
            *) printf 'install-eva: unexpected PipeWire build environment entry\n' >&2; exit 1 ;;
        esac
    done <<< "$eva_pipewire_environment"
    eva_cargo_args=(build --locked -p jwm-eva --target-dir "$eva_target_dir")
    if [[ $eva_profile == release ]]; then eva_cargo_args+=(--release); fi
    (cd -- "$eva_repo" && cargo "${eva_cargo_args[@]}")
fi
if [[ -z $eva_binary ]]; then eva_binary="$eva_target_dir/$eva_profile/jwm-eva"; fi
if [[ ! -f $eva_binary || ! -x $eva_binary ]]; then
    printf 'install-eva: executable not found: %s\n' "$eva_binary" >&2
    exit 1
fi

install -d -m755 -- "$eva_prefix/bin" "$eva_config_dir" "$eva_unit_dir"
install -m755 -- "$eva_binary" "$eva_prefix/bin/jwm-eva"
if [[ ! -e $eva_config_dir/eva.toml ]]; then
    install -m600 -- "$eva_repo/eva/dist/eva.toml" "$eva_config_dir/eva.toml"
fi
# systemd ExecStart has its own quoting and expands percent specifiers.
eva_exec="$eva_prefix/bin/jwm-eva"
eva_exec=${eva_exec//\\/\\\\}
eva_exec=${eva_exec//\"/\\\"}
eva_exec=${eva_exec//%/%%}
eva_config_exec="$eva_config_dir/eva.toml"
eva_config_exec=${eva_config_exec//\\/\\\\}
eva_config_exec=${eva_config_exec//\"/\\\"}
eva_config_exec=${eva_config_exec//%/%%}
eva_unit_tmp=$(mktemp "${TMPDIR:-/tmp}/jwm-eva-unit.XXXXXX")
trap 'rm -f -- "$eva_unit_tmp"' EXIT
eva_replaced=false
while IFS= read -r eva_line || [[ -n $eva_line ]]; do
    if [[ $eva_line == 'ExecStart=@JWM_EVA_EXECUTABLE@ --config @JWM_EVA_CONFIG@ daemon' ]]; then
        printf 'ExecStart="%s" --config "%s" daemon\n' "$eva_exec" "$eva_config_exec"
        eva_replaced=true
    else
        printf '%s\n' "$eva_line"
    fi
done < "$eva_repo/eva/dist/jwm-eva.service" > "$eva_unit_tmp"
if [[ $eva_replaced != true ]]; then
    printf 'install-eva: service template lacks the executable placeholder\n' >&2
    exit 1
fi
install -m644 -- "$eva_unit_tmp" "$eva_unit_dir/jwm-eva.service"

if [[ $eva_enable == true ]]; then
    eva_environment=()
    for eva_name in DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR; do
        if [[ -v $eva_name ]]; then eva_environment+=("$eva_name"); fi
    done
    if ((${#eva_environment[@]})); then systemctl --user import-environment "${eva_environment[@]}"; fi
    systemctl --user daemon-reload
    systemctl --user enable "$eva_unit_dir/jwm-eva.service"
    systemctl --user restart --no-block jwm-eva.service
fi
printf 'Eva installed: %s\nConfig preserved/created: %s\nUser unit: %s\n' \
    "$eva_prefix/bin/jwm-eva" "$eva_config_dir/eva.toml" "$eva_unit_dir/jwm-eva.service"
printf 'Next: register the API credential, then run jwm-eva doctor. See eva/README.md.\n'
