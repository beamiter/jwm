#!/usr/bin/env bash
# Isolated install/upgrade checks: no real user service or credentials are used.
set -euo pipefail

eva_repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
eva_test_dir=$(mktemp -d "${TMPDIR:-/tmp}/jwm-eva-install.XXXXXX")
trap 'rm -rf -- "$eva_test_dir"' EXIT
mkdir -p "$eva_test_dir/fake-bin"
printf '#!/usr/bin/env bash\nexit 0\n' > "$eva_test_dir/eva-binary"
chmod +x "$eva_test_dir/eva-binary"
cat > "$eva_test_dir/fake-bin/systemctl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${JWM_EVA_SYSTEMCTL_LOG:?}"
STUB
chmod +x "$eva_test_dir/fake-bin/systemctl"
eva_prefix="$eva_test_dir/user prefix with % character"
eva_config_dir="$eva_test_dir/config/jwm"
eva_unit_dir="$eva_test_dir/config/systemd/user"
export JWM_EVA_SYSTEMCTL_LOG="$eva_test_dir/systemctl.log"

PATH="$eva_test_dir/fake-bin:$PATH" bash "$eva_repo/scripts/install-eva.sh" \
    --binary "$eva_test_dir/eva-binary" --prefix "$eva_prefix" \
    --config-dir "$eva_config_dir" --unit-dir "$eva_unit_dir" >/dev/null
[[ -x $eva_prefix/bin/jwm-eva ]]
[[ $(stat -c '%a' "$eva_config_dir/eva.toml") == 600 ]]
rg -q '^\[openai\]$' "$eva_config_dir/eva.toml"
rg -Fq "ExecStart=\"$eva_test_dir/user prefix with %% character/bin/jwm-eva\" --config \"$eva_config_dir/eva.toml\" daemon" "$eva_unit_dir/jwm-eva.service"
rg -Fq -- '--user daemon-reload' "$JWM_EVA_SYSTEMCTL_LOG"
rg -Fq -- "--user enable $eva_unit_dir/jwm-eva.service" "$JWM_EVA_SYSTEMCTL_LOG"
rg -Fq -- '--user restart --no-block jwm-eva.service' "$JWM_EVA_SYSTEMCTL_LOG"

printf '\n# existing-user-setting\n' >> "$eva_config_dir/eva.toml"
eva_before=$(sha256sum "$eva_config_dir/eva.toml")
rm -- "$JWM_EVA_SYSTEMCTL_LOG"
PATH="$eva_test_dir/fake-bin:$PATH" bash "$eva_repo/scripts/install-eva.sh" \
    --no-enable --binary "$eva_test_dir/eva-binary" --prefix "$eva_prefix" \
    --config-dir "$eva_config_dir" --unit-dir "$eva_unit_dir" >/dev/null
[[ $eva_before == "$(sha256sum "$eva_config_dir/eva.toml")" ]]
[[ ! -e $JWM_EVA_SYSTEMCTL_LOG ]]
if bash "$eva_repo/scripts/install-eva.sh" --prefix relative --no-enable --no-build >/dev/null 2>&1; then
    printf 'installer accepted a relative prefix\n' >&2; exit 1
fi
if bash "$eva_repo/scripts/install-eva.sh" --prefix >/dev/null 2>&1; then
    printf 'installer accepted a missing option value\n' >&2; exit 1
fi
if bash "$eva_repo/scripts/install-eva.sh" --binary "$eva_test_dir/missing" --no-enable >/dev/null 2>&1; then
    printf 'installer accepted a missing binary\n' >&2; exit 1
fi
printf 'Eva installer: install, quoting, preservation, service lifecycle and failure checks passed.\n'
