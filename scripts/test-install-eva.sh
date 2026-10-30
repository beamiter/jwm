#!/usr/bin/env bash
# Isolated install/upgrade checks: no real user service or credentials are used.
set -euo pipefail

eva_repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
eva_test_dir=$(mktemp -d "${TMPDIR:-/tmp}/jwm-eva-install.XXXXXX")
trap 'rm -rf -- "$eva_test_dir"' EXIT
mkdir -p "$eva_test_dir/fake-bin"
cat > "$eva_test_dir/eva-binary" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${JWM_EVA_BINARY_LOG:?}"
STUB
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
export JWM_EVA_BINARY_LOG="$eva_test_dir/binary.log"

PATH="$eva_test_dir/fake-bin:$PATH" bash "$eva_repo/scripts/install-eva.sh" \
    --binary "$eva_test_dir/eva-binary" --prefix "$eva_prefix" \
    --config-dir "$eva_config_dir" --unit-dir "$eva_unit_dir" >"$eva_test_dir/first-install.log"
[[ -x $eva_prefix/bin/jwm-eva ]]
[[ $(stat -c '%a' "$eva_config_dir/eva.toml") == 600 ]]
rg -q '^\[openai\]$' "$eva_config_dir/eva.toml"
rg -Fq "ExecStart=\"$eva_test_dir/user prefix with %% character/bin/jwm-eva\" --config \"$eva_config_dir/eva.toml\" daemon" "$eva_unit_dir/jwm-eva.service"
rg -Fq -- '--user daemon-reload' "$JWM_EVA_SYSTEMCTL_LOG"
rg -Fq -- "--user enable $eva_unit_dir/jwm-eva.service" "$JWM_EVA_SYSTEMCTL_LOG"
rg -Fq -- '--user restart --no-block jwm-eva.service' "$JWM_EVA_SYSTEMCTL_LOG"

# An old voice profile is deliberately left for an explicit migration. Keep
# unusual spacing and a missing final newline to detect any automatic rewrite.
cat > "$eva_config_dir/eva.toml" <<'LEGACY'
# Existing pre-migration Eva profile
[interaction]
half_duplex = true
follow_up_ms = 5000
[openai]
model = "gpt-realtime-2.1-mini"
[future_wake_word]
enabled = false
phrase = "hi eva"
LEGACY
printf '# existing-user-setting without final newline' >> "$eva_config_dir/eva.toml"
cp -- "$eva_config_dir/eva.toml" "$eva_test_dir/old-config.toml"
eva_before=$(sha256sum "$eva_config_dir/eva.toml")
rm -- "$JWM_EVA_SYSTEMCTL_LOG"
PATH="$eva_test_dir/fake-bin:$PATH" bash "$eva_repo/scripts/install-eva.sh" \
    --no-enable --binary "$eva_test_dir/eva-binary" --prefix "$eva_prefix" \
    --config-dir "$eva_config_dir" --unit-dir "$eva_unit_dir" >"$eva_test_dir/upgrade.log"
[[ $eva_before == "$(sha256sum "$eva_config_dir/eva.toml")" ]]
cmp -- "$eva_test_dir/old-config.toml" "$eva_config_dir/eva.toml"
[[ ! -e $JWM_EVA_SYSTEMCTL_LOG ]]
[[ ! -e $JWM_EVA_BINARY_LOG ]]
eva_migrate_check=$(printf '%q --config %q config migrate --check' "$eva_prefix/bin/jwm-eva" "$eva_config_dir/eva.toml")
eva_migrate_write=$(printf '%q --config %q config migrate --write' "$eva_prefix/bin/jwm-eva" "$eva_config_dir/eva.toml")
rg -Fq -- "$eva_migrate_check" "$eva_test_dir/upgrade.log"
rg -Fq -- "$eva_migrate_write" "$eva_test_dir/upgrade.log"
rg -Fq -- 'Existing Eva configuration was kept unchanged.' "$eva_test_dir/upgrade.log"
if rg -Fq -- 'config migrate' "$eva_test_dir/first-install.log"; then
    printf 'installer suggested migrating a newly created configuration\n' >&2; exit 1
fi
# Symlinks are existing configuration entries even when their targets are
# missing. Installation must neither replace the link nor create its target.
rm -- "$eva_config_dir/eva.toml"
ln -s -- "$eva_test_dir/old-config.toml" "$eva_config_dir/eva.toml"
eva_symlink_before=$(readlink -- "$eva_config_dir/eva.toml")
eva_target_before=$(sha256sum "$eva_test_dir/old-config.toml")
PATH="$eva_test_dir/fake-bin:$PATH" bash "$eva_repo/scripts/install-eva.sh" \
    --no-enable --binary "$eva_test_dir/eva-binary" --prefix "$eva_prefix" \
    --config-dir "$eva_config_dir" --unit-dir "$eva_unit_dir" >"$eva_test_dir/symlink-upgrade.log"
[[ -L $eva_config_dir/eva.toml ]]
[[ $eva_symlink_before == "$(readlink -- "$eva_config_dir/eva.toml")" ]]
[[ $eva_target_before == "$(sha256sum "$eva_test_dir/old-config.toml")" ]]
rg -Fq -- "$eva_migrate_check" "$eva_test_dir/symlink-upgrade.log"
rm -- "$eva_config_dir/eva.toml"
ln -s -- "$eva_test_dir/missing-target.toml" "$eva_config_dir/eva.toml"
eva_symlink_before=$(readlink -- "$eva_config_dir/eva.toml")
PATH="$eva_test_dir/fake-bin:$PATH" bash "$eva_repo/scripts/install-eva.sh" \
    --no-enable --binary "$eva_test_dir/eva-binary" --prefix "$eva_prefix" \
    --config-dir "$eva_config_dir" --unit-dir "$eva_unit_dir" >"$eva_test_dir/dangling-upgrade.log"
[[ -L $eva_config_dir/eva.toml ]]
[[ $eva_symlink_before == "$(readlink -- "$eva_config_dir/eva.toml")" ]]
[[ ! -e $eva_test_dir/missing-target.toml ]]
rg -Fq -- "$eva_migrate_check" "$eva_test_dir/dangling-upgrade.log"
rg -Fq -- "$eva_migrate_write" "$eva_test_dir/dangling-upgrade.log"
[[ ! -e $JWM_EVA_BINARY_LOG ]]
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
printf 'Eva installer: install, quoting, byte/symlink preservation, migration hints, service lifecycle and failure checks passed.\n'
