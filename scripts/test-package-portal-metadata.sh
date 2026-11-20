#!/usr/bin/env bash
# Test package version mapping, offline contents, reproducibility and collisions.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/jwm-portal-metadata-tests.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
repo="$work/fixture"
mkdir -p "$repo/scripts" "$repo/portal/dist"
cp -- "$ROOT"/scripts/{package-portal-metadata.sh,test-package-portal-metadata.py} "$repo/scripts/"
cp -- "$ROOT/LICENSE" "$repo/"
cp -- "$ROOT/portal/dist/jwm.portal" "$repo/portal/dist/"

export SOURCE_DATE_EPOCH=1700000000
build="$repo/scripts/package-portal-metadata.sh"
verify="$repo/scripts/test-package-portal-metadata.py"
write_version() {
    printf '[workspace.package]\nversion = "9.8.7"\n[package]\nname = "jwm"\nversion = "%s"\n' "$1" > "$repo/Cargo.toml"
}
write_version 0.2.0
stable=$(bash "$build" "$work/out")
python3 "$verify" "$stable" --version 0.2.0-1
stable_copy=$(bash "$build" "$work/repro")
cmp -- "$stable" "$stable_copy"
if bash "$build" "$work/out" > /dev/null 2>&1; then
    printf 'builder must refuse to overwrite an existing package\n' >&2
    exit 1
fi
cmp -- "$stable" "$stable_copy"

write_version 0.2.0-rc.1+g123
pre=$(bash "$build" "$work/out")
python3 "$verify" "$pre" --version 0.2.0~rc.1+g123-1
dpkg --compare-versions 0.2.0~rc.1+g123-1 lt 0.2.0-1
write_version 0.2.0+git-456
meta=$(bash "$build" "$work/out")
python3 "$verify" "$meta" --version 0.2.0+git-456-1

# Corrupt a private copy to prove the artifact verifier rejects scripts and
# changed descriptor bytes, not just malformed arguments. Nothing is installed.
dpkg-deb --raw-extract "$stable" "$work/tampered"
printf '#!/bin/sh\nexit 0\n' > "$work/tampered/DEBIAN/postinst"
chmod 755 "$work/tampered/DEBIAN/postinst"
tampered="$work/jwm-portal-metadata_0.2.0-1_all.deb"
dpkg-deb --root-owner-group --build "$work/tampered" "$tampered" >/dev/null
if python3 "$verify" "$tampered" --version 0.2.0-1 >/dev/null 2>&1; then
    printf 'verifier accepted a maintainer script\n' >&2
    exit 1
fi
rm -- "$work/tampered/DEBIAN/postinst"
printf 'wrong descriptor\n' > "$work/tampered/usr/share/xdg-desktop-portal/portals/jwm.portal"
dpkg-deb --root-owner-group --build "$work/tampered" "$tampered" >/dev/null
if python3 "$verify" "$tampered" --version 0.2.0-1 >/dev/null 2>&1; then
    printf 'verifier accepted changed descriptor bytes\n' >&2
    exit 1
fi

for version in 'v0.2.0' '0.02.0' '0.2.0-rc.01' '0.2.0-rc..1' '0.2.0+' '0.2.0/../3'; do
    write_version "$version"
    if bash "$build" "$work/rejected" > /dev/null 2>&1; then
        printf 'invalid JWM version was accepted: %s\n' "$version" >&2
        exit 1
    fi
done
write_version 0.2.0
if SOURCE_DATE_EPOCH=invalid bash "$build" "$work/rejected" > /dev/null 2>&1; then
    printf 'invalid SOURCE_DATE_EPOCH was accepted\n' >&2
    exit 1
fi
[[ ! -e $work/rejected ]]
printf 'test-package-portal-metadata: PASS (3 versions, reproducibility, collision, 7 invalid inputs, 2 tampered packages)\n'
