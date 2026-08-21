#!/usr/bin/env bash
# Build a complete, reproducible JWM binary release archive.
set -euo pipefail
umask 022

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
PROJECT_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd -P)
MANIFEST="$PROJECT_ROOT/packaging/release-manifest.tsv"

OUTPUT_DIR="$PROJECT_ROOT/dist"
TARGET_DIR="$PROJECT_ROOT/target"
VERSION=""
SOURCE_EPOCH="${SOURCE_DATE_EPOCH:-}"
SKIP_BUILD=0

usage() {
    printf '%s\n' \
        "Usage: $(basename -- "$0") [OPTIONS]" \
        "" \
        "Build and package the complete JWM release bundle." \
        "" \
        "Options:" \
        "  --output-dir DIR        Archive output directory (default: dist)" \
        "  --target-dir DIR        Cargo target directory (default: target)" \
        "  --version VERSION       Release version (default: root Cargo.toml)" \
        "  --source-date-epoch N   Archive timestamp (default: SOURCE_DATE_EPOCH," \
        "                          then the current git commit timestamp, then 0)" \
        "  --no-build              Package already-built binaries" \
        "  -h, --help              Show this help"
}

die() {
    printf 'package-release: %s\n' "$*" >&2
    exit 1
}

need_value() {
    [[ $# -ge 2 && -n $2 ]] || die "$1 requires a value"
}

while (($#)); do
    case "$1" in
        --output-dir)
            need_value "$@"
            OUTPUT_DIR=$2
            shift 2
            ;;
        --target-dir)
            need_value "$@"
            TARGET_DIR=$2
            shift 2
            ;;
        --version)
            need_value "$@"
            VERSION=$2
            shift 2
            ;;
        --source-date-epoch)
            need_value "$@"
            SOURCE_EPOCH=$2
            shift 2
            ;;
        --no-build)
            SKIP_BUILD=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[[ -f $MANIFEST ]] || die "missing manifest: $MANIFEST"

if [[ -z $VERSION ]]; then
    VERSION=$(awk '
        /^\[package\][[:space:]]*$/ { in_package = 1; next }
        /^\[/ { if (in_package) exit }
        in_package && /^[[:space:]]*version[[:space:]]*=/ {
            line = $0
            sub(/^[^=]*=[[:space:]]*"/, "", line)
            sub(/"[[:space:]]*$/, "", line)
            print line
            exit
        }
    ' "$PROJECT_ROOT/Cargo.toml")
fi

valid_version() {
    [[ $1 =~ ^[0-9A-Za-z][0-9A-Za-z._+-]{0,127}$ && $1 != *..* ]]
}

valid_relative_path() {
    local value=$1 component
    [[ -n $value && $value != /* && $value != *$'\n'* && $value != *$'\r'* && $value != *$'\t'* ]] || return 1
    [[ $value =~ ^[0-9A-Za-z._+/@-]+$ ]] || return 1
    IFS=/ read -r -a components <<< "$value"
    for component in "${components[@]}"; do
        [[ -n $component && $component != . && $component != .. ]] || return 1
    done
}

valid_stable_path() {
    local value=$1
    valid_relative_path "$value" || return 1
    case "$value" in
        usr/local/bin/jwm|\
        usr/local/bin/jwm-tool|\
        usr/local/bin/jwm-support|\
        usr/local/bin/jwm-bridge|\
        usr/local/bin/tao_glow_bar|\
        usr/local/bin/tao_pixels_bar|\
        usr/share/xsessions/jwm-x11rb.desktop|\
        usr/share/xsessions/jwm-x11rb-debug.desktop|\
        usr/share/xsessions/jwm-xcb.desktop|\
        usr/share/xsessions/jwm-xcb-debug.desktop|\
        usr/share/wayland-sessions/jwm-wayland.desktop|\
        usr/share/wayland-sessions/jwm-wayland-debug.desktop|\
        usr/share/dbus-1/services/org.freedesktop.Notifications.service|\
        usr/local/share/doc/jwm)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

valid_version "$VERSION" || die "unsafe version: $VERSION"
[[ $SOURCE_EPOCH =~ ^[0-9]+$ ]] || {
    if [[ -z $SOURCE_EPOCH ]]; then
        SOURCE_EPOCH=$(git -C "$PROJECT_ROOT" log -1 --format=%ct 2>/dev/null || true)
        SOURCE_EPOCH=${SOURCE_EPOCH:-0}
    else
        die "--source-date-epoch must be a non-negative integer"
    fi
}

if [[ $OUTPUT_DIR != /* ]]; then
    OUTPUT_DIR="$PROJECT_ROOT/$OUTPUT_DIR"
fi
if [[ $TARGET_DIR != /* ]]; then
    TARGET_DIR="$PROJECT_ROOT/$TARGET_DIR"
fi
mkdir -p -- "$OUTPUT_DIR" "$TARGET_DIR"
OUTPUT_DIR=$(cd -- "$OUTPUT_DIR" && pwd -P)
TARGET_DIR=$(cd -- "$TARGET_DIR" && pwd -P)

for tool in awk find grep gzip install mkdir mktemp mv sha256sum tar uname; do
    command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

if ((SKIP_BUILD == 0)); then
    command -v cargo >/dev/null 2>&1 || die "cargo is required unless --no-build is used"
    cargo build --locked --release --bins --target-dir "$TARGET_DIR" --manifest-path "$PROJECT_ROOT/Cargo.toml"
    cargo build --locked --release --bin jwm-bridge --target-dir "$TARGET_DIR" --manifest-path "$PROJECT_ROOT/bridge/Cargo.toml"
    cargo build --locked --release --bin tao_glow_bar --target-dir "$TARGET_DIR" --manifest-path "$PROJECT_ROOT/bars/tao_glow_bar/Cargo.toml"
    cargo build --locked --release --bin tao_pixels_bar --target-dir "$TARGET_DIR" --manifest-path "$PROJECT_ROOT/bars/tao_pixels_bar/Cargo.toml"
fi

ARCH=$(uname -m)
[[ $ARCH =~ ^[0-9A-Za-z_+-]+$ ]] || die "unsafe architecture name: $ARCH"
BUNDLE_NAME="jwm-$VERSION-linux-$ARCH"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-release.XXXXXXXX")
STAGING="$TMP_ROOT/$BUNDLE_NAME"
VERSION_ROOT="$STAGING/payload/usr/local/lib/jwm/versions/$VERSION"

cleanup() {
    rm -rf -- "$TMP_ROOT"
}
trap cleanup EXIT HUP INT TERM

mkdir -p -- "$VERSION_ROOT"
install -m 0644 -- "$MANIFEST" "$STAGING/release-manifest.tsv"
install -m 0755 -- "$PROJECT_ROOT/scripts/install-release.sh" "$STAGING/install-release.sh"
printf '%s\n' "$VERSION" > "$STAGING/VERSION"
printf '%s\n' "$ARCH" > "$STAGING/ARCH"
chmod 0644 -- "$STAGING/VERSION" "$STAGING/ARCH"

declare -A SEEN_VERSION_PATHS=()
declare -A SEEN_STABLE_PATHS=()
line_number=0
entry_count=0

while IFS=$'\t' read -r kind source version_path stable_path mode extra || [[ -n ${kind:-} ]]; do
    ((line_number += 1))
    [[ -n ${kind:-} ]] || continue
    [[ $kind == \#* ]] && continue
    [[ -z ${extra:-} ]] || die "manifest line $line_number has too many columns"
    case "$kind" in
        built|repo-file|repo-tree|stable-link) ;;
        *) die "manifest line $line_number has unknown kind: $kind" ;;
    esac
    valid_relative_path "$version_path" || die "manifest line $line_number has unsafe version path: $version_path"
    [[ -z ${SEEN_VERSION_PATHS[$version_path]+set} ]] || die "duplicate version path in manifest: $version_path"
    SEEN_VERSION_PATHS[$version_path]=1

    if [[ $stable_path != - ]]; then
        valid_stable_path "$stable_path" || die "manifest line $line_number has unsafe stable path: $stable_path"
        [[ -z ${SEEN_STABLE_PATHS[$stable_path]+set} ]] || die "duplicate stable path in manifest: $stable_path"
        SEEN_STABLE_PATHS[$stable_path]=1
    fi

    destination="$VERSION_ROOT/$version_path"
    case "$kind" in
        built)
            valid_relative_path "$source" || die "manifest line $line_number has unsafe binary name: $source"
            [[ $source != */* && $mode == 0755 ]] || die "manifest line $line_number has an invalid built entry"
            [[ -f $TARGET_DIR/release/$source && ! -L $TARGET_DIR/release/$source ]] || die "missing built binary: $TARGET_DIR/release/$source"
            mkdir -p -- "$(dirname -- "$destination")"
            install -m 0755 -- "$TARGET_DIR/release/$source" "$destination"
            ;;
        repo-file)
            valid_relative_path "$source" || die "manifest line $line_number has unsafe source path: $source"
            [[ $mode == 0644 || $mode == 0755 ]] || die "manifest line $line_number has invalid mode: $mode"
            [[ -f $PROJECT_ROOT/$source && ! -L $PROJECT_ROOT/$source ]] || die "missing regular source file: $source"
            mkdir -p -- "$(dirname -- "$destination")"
            install -m "$mode" -- "$PROJECT_ROOT/$source" "$destination"
            ;;
        repo-tree)
            valid_relative_path "$source" || die "manifest line $line_number has unsafe source tree: $source"
            [[ $mode == 0644 ]] || die "manifest line $line_number has invalid tree mode: $mode"
            [[ -d $PROJECT_ROOT/$source && ! -L $PROJECT_ROOT/$source ]] || die "missing source directory: $source"
            if find "$PROJECT_ROOT/$source" \( -type l -o ! -type d ! -type f \) -print -quit | grep -q .; then
                die "source tree contains a symlink or special file: $source"
            fi
            mkdir -p -- "$destination"
            while IFS= read -r -d '' directory; do
                relative=${directory#"$PROJECT_ROOT/$source"/}
                [[ $directory == "$PROJECT_ROOT/$source" ]] && relative=""
                [[ -z $relative ]] || mkdir -p -- "$destination/$relative"
            done < <(find "$PROJECT_ROOT/$source" -type d -print0)
            while IFS= read -r -d '' file; do
                relative=${file#"$PROJECT_ROOT/$source"/}
                mkdir -p -- "$(dirname -- "$destination/$relative")"
                install -m 0644 -- "$file" "$destination/$relative"
            done < <(find "$PROJECT_ROOT/$source" -type f -print0)
            ;;
        stable-link)
            [[ $source == - && $mode == - && $stable_path != - ]] || die "manifest line $line_number has an invalid stable-link entry"
            ;;
    esac
    ((entry_count += 1))
done < "$MANIFEST"

((entry_count > 0)) || die "release manifest has no entries"

# Every payload object must be a normal directory or regular file. The archive
# deliberately carries no symlink that could be followed during installation.
if find "$STAGING/payload" \( -type l -o ! -type d ! -type f \) -print -quit | grep -q .; then
    die "staged payload contains a symlink or special file"
fi

# Normalize directory modes; file mtimes, uid/gid, and ordering are normalized
# by tar below. gzip -n suppresses its timestamp and original-name fields.
find "$STAGING" -type d -exec chmod 0755 {} +

ARCHIVE="$OUTPUT_DIR/$BUNDLE_NAME.tar.gz"
ARCHIVE_TMP="$TMP_ROOT/$BUNDLE_NAME.tar.gz"
LC_ALL=C tar \
    --sort=name \
    --format=posix \
    --pax-option=delete=atime,delete=ctime \
    --mtime="@$SOURCE_EPOCH" \
    --owner=0 --group=0 --numeric-owner \
    -C "$TMP_ROOT" -cf - "$BUNDLE_NAME" | gzip -n -9 > "$ARCHIVE_TMP"
mv -f -- "$ARCHIVE_TMP" "$ARCHIVE"
CHECKSUM_TMP="$TMP_ROOT/$BUNDLE_NAME.tar.gz.sha256"
(
    cd -- "$OUTPUT_DIR"
    sha256sum -- "$BUNDLE_NAME.tar.gz"
) > "$CHECKSUM_TMP"
mv -f -- "$CHECKSUM_TMP" "$ARCHIVE.sha256"

printf 'Created %s\n' "$ARCHIVE"
printf 'Created %s.sha256\n' "$ARCHIVE"
