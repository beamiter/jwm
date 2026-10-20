#!/usr/bin/env bash
# Offline, destructive-to-temporary-directory-only wallpaper installer checks.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-wallpaper-install.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPO="$TEST_ROOT/project space"
mkdir -p "$REPO/scripts" "$REPO/assets/wallpapers"
cp -- "$ROOT/scripts/install-wallpapers.sh" "$REPO/scripts/"
INSTALLER="$REPO/scripts/install-wallpapers.sh"
printf 'v1\n' > "$REPO/assets/wallpapers/01.png"
printf 'beach\n' > "$REPO/assets/wallpapers/02.JPG"
printf 'ignored\n' > "$REPO/assets/wallpapers/README.txt"
fail() { printf 'test-wallpaper-install: FAIL: %s\n' "$*" >&2; exit 1; }
export HOME="$TEST_ROOT/home with spaces" XDG_CONFIG_HOME="$TEST_ROOT/config with spaces"
mkdir -p -- "$HOME" "$XDG_CONFIG_HOME/jwm/wallpapers"
TARGET="$XDG_CONFIG_HOME/jwm/wallpapers"
printf 'private\n' > "$TARGET/01.png"
bash "$INSTALLER" >/dev/null
[[ $(<"$TARGET/01.png") == private ]] || fail 'custom same-name image was overwritten'
[[ $(<"$TARGET/02.JPG") == beach ]] || fail 'bundled image was not installed'
[[ ! -e $TARGET/README.txt ]] || fail 'nonimage sidecar was installed'
[[ $(wc -l < "$TARGET/.jwm-bundled-sha256") == 1 ]] || fail 'managed hashes include user files'

printf 'beach v2\n' > "$REPO/assets/wallpapers/02.JPG"
bash "$INSTALLER" >/dev/null
[[ $(<"$TARGET/02.JPG") == 'beach v2' ]] || fail 'untouched managed image failed to update'
printf 'my edits\n' > "$TARGET/02.JPG"
printf 'beach v3\n' > "$REPO/assets/wallpapers/02.JPG"
bash "$INSTALLER" >/dev/null
[[ $(<"$TARGET/02.JPG") == 'my edits' ]] || fail 'edited bundled image was overwritten'
[[ ! -s $TARGET/.jwm-bundled-sha256 ]] || fail 'modified image remained managed'
rm -- "$TARGET/02.JPG"
bash "$INSTALLER" >/dev/null
[[ $(<"$TARGET/02.JPG") == 'beach v3' ]] || fail 'removed image was not restored'

printf 'protected\n' > "$TEST_ROOT/outside.png"
rm -- "$TARGET/02.JPG"
ln -s -- "$TEST_ROOT/outside.png" "$TARGET/02.JPG"
bash "$INSTALLER" >/dev/null
[[ $(<"$TEST_ROOT/outside.png") == protected ]] || fail 'symlink target was overwritten'
rm -- "$TARGET/02.JPG"

# Relative XDG_CONFIG_HOME is not a legal XDG location and must be ignored.
XDG_CONFIG_HOME=relative bash "$INSTALLER" >/dev/null
[[ -f $HOME/.config/jwm/wallpapers/01.png ]] || fail 'relative XDG config home was used'

ln -s "$TARGET" "$TEST_ROOT/destination link"
if bash "$INSTALLER" --dest "$TEST_ROOT/destination link" > /dev/null 2>&1; then
    fail 'symlink directory was accepted'
fi
if bash "$INSTALLER" --dest relative > /dev/null 2>&1; then
    fail 'relative explicit destination was accepted'
fi
# An old user-managed collection is never touched.
mkdir -p "$HOME/.local/share/jwm/wallpapers/jwm-14"
printf 'legacy\n' > "$HOME/.local/share/jwm/wallpapers/jwm-14/old.png"
bash "$INSTALLER" >/dev/null
[[ $(<"$HOME/.local/share/jwm/wallpapers/jwm-14/old.png") == legacy ]] || fail 'legacy user wallpaper was removed'
printf 'test-wallpaper-install: PASS\n'
