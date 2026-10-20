#!/usr/bin/env bash
# Install bundled static wallpapers without replacing the user's own pictures.
set -euo pipefail

SOURCE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)/assets/wallpapers"

usage() {
    printf 'Usage: %s [--dest ABSOLUTE_WALLPAPER_DIRECTORY]\n' "${0##*/}"
    printf 'Default: ~/.config/jwm/wallpapers (honors absolute XDG_CONFIG_HOME)\n'
}

if [[ $# == 1 && ( $1 == -h || $1 == --help ) ]]; then
    usage
    exit 0
elif [[ $# == 0 ]]; then
    if [[ ${XDG_CONFIG_HOME:-} == /* ]]; then
        DEST_DIR="$XDG_CONFIG_HOME/jwm/wallpapers"
    else
        DEST_DIR="$HOME/.config/jwm/wallpapers"
    fi
elif [[ $# == 2 && $1 == --dest ]]; then
    DEST_DIR="$2"
else
    usage >&2
    exit 2
fi

fail() { printf 'install-wallpapers: %s\n' "$*" >&2; exit 1; }
[[ $DEST_DIR == /* ]] || fail 'destination must be an absolute path'
[[ -d $SOURCE_DIR ]] || fail "missing wallpaper assets: $SOURCE_DIR"
command -v sha256sum >/dev/null || fail 'sha256sum is required'
[[ ! -L $DEST_DIR ]] || fail "refusing symlink wallpaper directory: $DEST_DIR"
mkdir -p -- "$DEST_DIR"
[[ -d $DEST_DIR && ! -L $DEST_DIR ]] || fail "not a regular wallpaper directory: $DEST_DIR"

MANIFEST="$DEST_DIR/.jwm-bundled-sha256"
[[ ! -L $MANIFEST ]] || fail "refusing symlink manifest: $MANIFEST"
[[ ! -d $MANIFEST ]] || fail "manifest path is a directory: $MANIFEST"
declare -A previous=()
if [[ -f $MANIFEST ]]; then
    while read -r hash name; do
        if [[ $hash =~ ^[[:xdigit:]]{64}$ && -n ${name:-} && $name != */* && $name != . && $name != .. ]]; then
            previous["$name"]="$hash"
        fi
    done < "$MANIFEST"
fi

TEMP_IMAGE=''
TEMP_MANIFEST=''
cleanup() {
    [[ -z $TEMP_IMAGE ]] || rm -f -- "$TEMP_IMAGE"
    [[ -z $TEMP_MANIFEST ]] || rm -f -- "$TEMP_MANIFEST"
}
trap cleanup EXIT
TEMP_MANIFEST=$(mktemp "$DEST_DIR/.jwm-bundled-sha256.XXXXXXXX")

copied=0 updated=0 unchanged=0 preserved=0 images=0
# Do not recurse or include sidecars; no repository-specific subfolder is needed.
shopt -s nullglob
for src in "$SOURCE_DIR"/*; do
    [[ -f $src && ! -L $src ]] || continue
    name=${src##*/}
    ext=${name##*.}
    case ${ext,,} in png|jpg|jpeg|webp|bmp|gif) ;; *) continue ;; esac
    ((images += 1))
    new_hash=$(sha256sum -- "$src")
    new_hash=${new_hash%% *}
    dest="$DEST_DIR/$name"
    if [[ -L $dest || ( -e $dest && ! -f $dest ) ]]; then
        printf 'install-wallpapers: preserve non-regular file: %s\n' "$dest" >&2
        ((preserved += 1))
        continue
    fi
    if [[ -f $dest ]]; then
        old_hash=$(sha256sum -- "$dest")
        old_hash=${old_hash%% *}
        if [[ $old_hash == "$new_hash" ]]; then
            ((unchanged += 1))
            printf '%s %s\n' "$new_hash" "$name" >> "$TEMP_MANIFEST"
            continue
        fi
        if [[ ${previous[$name]:-} != "$old_hash" ]]; then
            printf 'install-wallpapers: preserve user-modified wallpaper: %s\n' "$dest" >&2
            ((preserved += 1))
            continue
        fi
        ((updated += 1))
    else
        ((copied += 1))
    fi
    TEMP_IMAGE=$(mktemp "$DEST_DIR/.jwm-image.XXXXXXXX")
    install -m 0644 -- "$src" "$TEMP_IMAGE"
    mv -f -- "$TEMP_IMAGE" "$dest"
    TEMP_IMAGE=''
    printf '%s %s\n' "$new_hash" "$name" >> "$TEMP_MANIFEST"
done
((images > 0)) || fail "no static wallpaper images found in $SOURCE_DIR"
chmod 0644 -- "$TEMP_MANIFEST"
mv -f -- "$TEMP_MANIFEST" "$MANIFEST"
TEMP_MANIFEST=''
printf 'install-wallpapers: %d copied, %d updated, %d unchanged, %d preserved -> %s\n' \
    "$copied" "$updated" "$unchanged" "$preserved" "$DEST_DIR"
