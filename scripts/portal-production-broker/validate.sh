#!/usr/bin/env bash
# Disposable Ubuntu runner only. This deliberately tests the package-owned
# system metadata path; the production user installer never does sudo.
set -euo pipefail

source_dir=$(realpath -- "$1")
evidence=$(realpath -- "$2")
product_commit=$3
package=jwm-portal-metadata
deb_version=0.2.0-1
metadata=/usr/share/xdg-desktop-portal/portals/jwm.portal
gtk=/usr/share/xdg-desktop-portal/portals/gtk.portal
descriptor="$source_dir/portal/dist/jwm.portal"
[[ $(id -u) != 0 ]]
actual=$(dpkg-query -W -f='${db:Status-Status} ${Version}' xdg-desktop-portal)
printf '%s\n' "$actual" > "$evidence/required-broker-version.txt"
[[ $actual == 'installed 1.18.4-1ubuntu2.24.04.3' ]] ||
    { echo "Expected Ubuntu broker package 1.18.4-1ubuntu2.24.04.3, got: $actual" >&2; exit 1; }
[[ ! -e $metadata && ! -L $metadata && -f $gtk && -f $descriptor ]]
if dpkg-query -W "$package" 2>/dev/null; then
    echo "Unexpected pre-existing package: $package" >&2
    exit 1
fi

cleanup() {
    local rc=$?
    trap - EXIT
    if dpkg-query -W "$package" >/dev/null 2>&1; then
        sudo -n dpkg --purge "$package" >> "$evidence/final-cleanup.log" 2>&1 || rc=1
    fi
    if [[ -e $metadata || -L $metadata ]]; then
        echo "Production package metadata survived cleanup" >> "$evidence/final-cleanup.log"
        rc=1
    fi
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
sha256sum -- "$gtk" > "$evidence/gtk-descriptor.sha256"

probe() {
    local phase=$1 state=$2
    env -i PATH=/usr/bin:/bin LANG=C.UTF-8 PYTHONDONTWRITEBYTECODE=1 \
        timeout --kill-after=3 45 /usr/bin/python3 -I -B \
        "$source_dir/scripts/portal-production-broker/portal_probe.py" \
        --repo "$source_dir" --output "$evidence/$phase" --package-state "$state" \
        2>&1 | tee "$evidence/$phase.log"
    sha256sum -c "$evidence/gtk-descriptor.sha256"
}

# These are the actual tagged Release builder and strict verifier from the
# product parent, not a CI-generated substitute control or descriptor file.
export SOURCE_DATE_EPOCH="$(git -C "$source_dir" show -s --format=%ct "$product_commit")"
artifact=$(bash "$source_dir/scripts/package-portal-metadata.sh" "$evidence")
/usr/bin/python3 "$source_dir/scripts/test-package-portal-metadata.py" \
    "$artifact" --version "$deb_version" | tee "$evidence/artifact-verification.log"
(
    cd -- "$evidence"
    sha256sum -- "${artifact##*/}" > production-deb.sha256
    sha256sum -c production-deb.sha256
)
dpkg-deb --info "$artifact" > "$evidence/production-deb-control.txt"
dpkg-deb --contents "$artifact" > "$evidence/production-deb-contents.txt"

probe before absent
sudo -n dpkg -i "$artifact" 2>&1 | tee "$evidence/package-install.log"
[[ $(dpkg-query -W -f='${db:Status-Status} ${Version}' "$package") == "installed $deb_version" ]]
dpkg-query -S "$metadata" > "$evidence/descriptor-ownership.txt"
[[ $(cat "$evidence/descriptor-ownership.txt") == "$package: $metadata" ]]
[[ ! -L $metadata && $(stat -c '%u:%g:%a' "$metadata") == 0:0:644 ]]
cmp -- "$descriptor" "$metadata"
sha256sum -- "$descriptor" "$metadata" > "$evidence/jwm-descriptor.sha256"
probe after present

sudo -n dpkg --purge "$package" 2>&1 | tee "$evidence/package-remove.log"
[[ ! -e $metadata && ! -L $metadata ]]
if dpkg-query -W "$package" 2>/dev/null; then
    echo "Production package still registered after purge" >&2
    exit 1
fi
probe uninstalled absent
printf 'package absent; descriptor absent; stock GTK descriptor unchanged\n' > "$evidence/purge-proof.txt"
/usr/bin/python3 - "$evidence" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
reports = [json.loads((root / phase / "result.json").read_text())
           for phase in ("before", "after", "uninstalled")]
assert len({r["broker_sha256"] for r in reports}) == 1
assert len({r["descriptor_sha256"] for r in reports}) == 1
for phase in ("before", "after", "uninstalled"):
    row = json.loads((root / phase / "discover-wayland" / "broker-identity.json").read_text())
    assert row["portal_metadata_override_absent"] is True
print("Ubuntu broker package discovery before/after/uninstall: PASS")
PY
