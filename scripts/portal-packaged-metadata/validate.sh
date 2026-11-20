#!/usr/bin/env bash
# Disposable Ubuntu runner only. This deliberately tests the package-owned
# system metadata path; the production user installer never does sudo.
set -euo pipefail

source_dir=$(realpath -- "$1")
evidence=$(realpath -- "$2")
package=jwm-portal-metadata-fixture
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

stage=$(mktemp -d)
cleanup() {
    local rc=$?
    trap - EXIT
    if dpkg-query -W "$package" >/dev/null 2>&1; then
        sudo -n dpkg --purge "$package" >> "$evidence/final-cleanup.log" 2>&1 || rc=1
    fi
    if [[ -e $metadata || -L $metadata ]]; then
        echo "Fixture metadata survived cleanup" >> "$evidence/final-cleanup.log"
        rc=1
    fi
    rm -rf -- "$stage"
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
        "$source_dir/scripts/portal-packaged-metadata/portal_probe.py" \
        --repo "$source_dir" --output "$evidence/$phase" --package-state "$state" \
        2>&1 | tee "$evidence/$phase.log"
    sha256sum -c "$evidence/gtk-descriptor.sha256"
}

probe before absent
mkdir -p "$stage/image/DEBIAN"
cat > "$stage/image/DEBIAN/control" <<CONTROL
Package: $package
Version: 1.0-1
Architecture: all
Maintainer: JWM CI <ci@example.invalid>
Description: Disposable JWM portal metadata acceptance fixture
CONTROL
install -D -m 0644 "$descriptor" "$stage/image$metadata"
dpkg-deb --root-owner-group --build "$stage/image" "$stage/metadata.deb"
dpkg-deb -c "$stage/metadata.deb" > "$evidence/fixture-package-contents.txt"
sudo -n dpkg -i "$stage/metadata.deb" 2>&1 | tee "$evidence/package-install.log"
[[ $(dpkg-query -S "$metadata") == "$package: $metadata" ]]
[[ ! -L $metadata && $(stat -c '%u:%g:%a' "$metadata") == 0:0:644 ]]
cmp -- "$descriptor" "$metadata"
sha256sum -- "$descriptor" "$metadata" > "$evidence/jwm-descriptor.sha256"
probe after present

sudo -n dpkg --purge "$package" 2>&1 | tee "$evidence/package-remove.log"
[[ ! -e $metadata && ! -L $metadata ]]
if dpkg-query -W "$package" 2>/dev/null; then
    echo "Fixture package still registered after purge" >&2
    exit 1
fi
probe uninstalled absent
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
