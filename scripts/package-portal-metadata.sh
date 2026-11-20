#!/usr/bin/env bash
# Build the optional, metadata-only Debian package without installing it.
set -euo pipefail

if [[ $# != 1 ]]; then
    printf 'Usage: %s OUTPUT_DIR\n' "${0##*/}" >&2
    exit 2
fi
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
version=$(awk '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ {
        if ($0 !~ /^version[[:space:]]*=[[:space:]]*"[^"]+"[[:space:]]*$/) exit 1
        split($0, parts, "\"")
        print parts[2]
    }
' "$ROOT/Cargo.toml")
semver='^((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))(-([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?(\+([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?$'
if [[ ! $version =~ $semver ]]; then
    printf 'Invalid JWM package version: %s\n' "$version" >&2
    exit 1
fi
upstream=${BASH_REMATCH[1]}
pre=${BASH_REMATCH[6]}
build=${BASH_REMATCH[9]}
if [[ -n $pre ]]; then
    IFS=. read -r -a identifiers <<< "$pre"
    for identifier in "${identifiers[@]}"; do
        if [[ $identifier =~ ^0[0-9]+$ ]]; then
            printf 'Invalid numeric prerelease identifier: %s\n' "$identifier" >&2
            exit 1
        fi
    done
    upstream+="~$pre"
fi
if [[ -n $build ]]; then
    upstream+="+$build"
fi
deb_version="$upstream-1"
dpkg --validate-version "$deb_version"
if [[ -n ${SOURCE_DATE_EPOCH:-} && ! $SOURCE_DATE_EPOCH =~ ^[0-9]+$ ]]; then
    printf 'SOURCE_DATE_EPOCH must be an unsigned integer\n' >&2
    exit 1
fi

# Only a control file, the existing descriptor, and its copyright notice.
# Fixed staging permissions and root-owner-group avoid host-user metadata.
umask 022
mkdir -p -- "$1"
out=$(cd -- "$1" && pwd -P)
work=$(mktemp -d "$out/.jwm-portal-metadata.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
pkg="$work/package"
name=jwm-portal-metadata
install -d -m755 -- "$pkg/DEBIAN"
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: $name
Version: $deb_version
Architecture: all
Section: x11
Priority: optional
Maintainer: JWM maintainers <beamiter@users.noreply.github.com>
Depends: xdg-desktop-portal
Homepage: https://github.com/beamiter/jwm
Description: system Portal descriptor for the JWM Wayland ScreenCast backend
 Installs only the discovery descriptor for brokers that cannot read
 per-user Portal descriptors. Install the JWM Portal backend separately
 in the target user session.
CONTROL
install -Dm644 -- "$ROOT/portal/dist/jwm.portal" "$pkg/usr/share/xdg-desktop-portal/portals/jwm.portal"
install -Dm644 -- "$ROOT/LICENSE" "$pkg/usr/share/doc/$name/copyright"
artifact="${name}_${deb_version}_all.deb"
dpkg-deb --root-owner-group --build "$pkg" "$work/$artifact" >&2
# Work and output share a filesystem. Publish atomically, never overwrite
# an existing artifact, even if another builder finished concurrently.
ln -T -- "$work/$artifact" "$out/$artifact"
printf '%s\n' "$out/$artifact"
