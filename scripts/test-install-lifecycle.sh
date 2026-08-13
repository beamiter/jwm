#!/usr/bin/env bash
# End-to-end DESTDIR test for legacy replacement and release lifecycle state.
set -euo pipefail
umask 022

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
PROJECT_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd -P)
MANIFEST="$PROJECT_ROOT/packaging/release-manifest.tsv"
INSTALLER="$PROJECT_ROOT/scripts/install-release.sh"
PACKAGER="$PROJECT_ROOT/scripts/package-release.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-install-test.XXXXXXXX")
DESTDIR="$TMP_ROOT/root"

cleanup() {
    case "$TMP_ROOT" in
        "${TMPDIR:-/tmp}"/jwm-install-test.*) rm -rf -- "$TMP_ROOT" ;;
        *) printf 'Refusing to clean unexpected test path: %s\n' "$TMP_ROOT" >&2 ;;
    esac
}
trap cleanup EXIT HUP INT TERM

fail() {
    printf 'test-install-lifecycle: FAIL: %s\n' "$*" >&2
    exit 1
}

assert_file_contains() {
    local path=$1 expected=$2
    [[ -f $path ]] || fail "expected a regular file: $path"
    grep -Fq -- "$expected" "$path" || fail "$path does not contain: $expected"
}

assert_relative_link() {
    local path=$1 target
    [[ -L $path ]] || fail "expected a symlink: $path"
    target=$(readlink -- "$path")
    [[ -n $target && $target != /* ]] || fail "expected a relative symlink: $path -> $target"
}

for session in \
    jwm-x11rb.desktop \
    jwm-x11rb-debug.desktop \
    jwm-xcb.desktop \
    jwm-xcb-debug.desktop; do
    if grep -q 'JWM_WATERLILY_' "$PROJECT_ROOT/$session"; then
        fail "release session forces opt-in WaterLily settings: $session"
    fi
done

make_fake_bundle() {
    local version=$1 marker=$2 bundle root
    bundle="$TMP_ROOT/bundle-$version"
    root="$bundle/payload/usr/local/lib/jwm/versions/$version"
    local kind source version_path stable_path mode extra destination
    mkdir -p -- "$root"
    cp -- "$INSTALLER" "$bundle/install-release.sh"
    cp -- "$MANIFEST" "$bundle/release-manifest.tsv"
    printf '%s\n' "$version" > "$bundle/VERSION"

    while IFS=$'\t' read -r kind source version_path stable_path mode extra || [[ -n ${kind:-} ]]; do
        [[ -n ${kind:-} && $kind != \#* ]] || continue
        [[ -z ${extra:-} ]] || fail "test manifest has too many columns"
        destination="$root/$version_path"
        case "$kind" in
            built)
                mkdir -p -- "$(dirname -- "$destination")"
                printf '#!/usr/bin/env sh\nprintf "%%s\\n" "%s:%s"\n' "$marker" "$source" > "$destination"
                chmod 0755 -- "$destination"
                ;;
            repo-file)
                mkdir -p -- "$(dirname -- "$destination")"
                printf '%s:%s\n' "$marker" "$source" > "$destination"
                chmod "$mode" -- "$destination"
                ;;
            repo-tree)
                mkdir -p -- "$destination"
                printf '%s:documentation\n' "$marker" > "$destination/lifecycle.md"
                ;;
            stable-link)
                mkdir -p -- "$destination"
                ;;
            *) fail "unknown test manifest kind: $kind" ;;
        esac
    done < "$MANIFEST"
    printf '%s\n' "$bundle"
}

mkdir -p -- "$DESTDIR/usr/local/bin"
printf 'legacy-jwm\n' > "$DESTDIR/usr/local/bin/jwm"
chmod 0751 -- "$DESTDIR/usr/local/bin/jwm"

BUNDLE_V1=$(make_fake_bundle 1.0.0 release-v1)
BUNDLE_V2=$(make_fake_bundle 2.0.0 release-v2)

# Bundle extraction metadata is untrusted input. A privileged installation
# must not preserve a user's writable or set-id modes in the immutable tree.
chmod 04755 -- "$BUNDLE_V1/payload/usr/local/lib/jwm/versions/1.0.0/bin/jwm"
chmod 0666 -- "$BUNDLE_V1/payload/usr/local/lib/jwm/versions/1.0.0/share/xsessions/jwm-x11rb.desktop"

# DESTDIR is a security boundary and must be absolute.
if bash "$BUNDLE_V1/install-release.sh" install --destdir relative-root >"$TMP_ROOT/relative.log" 2>&1; then
    fail "relative --destdir was accepted"
fi

# A bundle manifest may never escape its immutable version directory, even
# when replacement was explicitly requested.
BAD_BUNDLE="$TMP_ROOT/bundle-unsafe"
cp -a -- "$BUNDLE_V1" "$BAD_BUNDLE"
printf 'built\tjwm\t../escape\tusr/local/bin/jwm\t0755\n' >> "$BAD_BUNDLE/release-manifest.tsv"
if bash "$BAD_BUNDLE/install-release.sh" install --destdir "$DESTDIR" --replace >"$TMP_ROOT/unsafe.log" 2>&1; then
    fail "unsafe manifest version path was accepted"
fi
assert_file_contains "$DESTDIR/usr/local/bin/jwm" legacy-jwm
[[ ! -e $DESTDIR/usr/local/lib/jwm/.release-state ]] || fail "unsafe bundle left installation state behind"

BAD_PAYLOAD="$TMP_ROOT/bundle-symlink"
cp -a -- "$BUNDLE_V1" "$BAD_PAYLOAD"
rm -- "$BAD_PAYLOAD/payload/usr/local/lib/jwm/versions/1.0.0/bin/jwm"
ln -s -- /etc/passwd "$BAD_PAYLOAD/payload/usr/local/lib/jwm/versions/1.0.0/bin/jwm"
if bash "$BAD_PAYLOAD/install-release.sh" install --destdir "$DESTDIR" --replace >"$TMP_ROOT/symlink.log" 2>&1; then
    fail "symlink payload was accepted"
fi
assert_file_contains "$DESTDIR/usr/local/bin/jwm" legacy-jwm
[[ ! -e $DESTDIR/usr/local/lib/jwm/.release-state ]] || fail "symlink payload left installation state behind"

# The legacy binary is an intentional collision. No mutation is allowed until
# replacement is explicit.
if bash "$BUNDLE_V1/install-release.sh" install --destdir "$DESTDIR" >"$TMP_ROOT/no-replace.log" 2>&1; then
    fail "legacy destination was replaced without --replace"
fi
assert_file_contains "$DESTDIR/usr/local/bin/jwm" legacy-jwm
[[ ! -e $DESTDIR/usr/local/lib/jwm/.release-state ]] || fail "failed preflight left installation state behind"

bash "$BUNDLE_V1/install-release.sh" install --destdir "$DESTDIR" --replace
[[ $(readlink -- "$DESTDIR/usr/local/lib/jwm/current") == versions/1.0.0 ]] || fail "v1 did not become current"
assert_relative_link "$DESTDIR/usr/local/bin/jwm"
assert_file_contains "$DESTDIR/usr/local/bin/jwm" release-v1:jwm
assert_file_contains "$DESTDIR/usr/local/lib/jwm/.release-state/backups/usr/local/bin/jwm" legacy-jwm
[[ $(stat -c '%a' "$DESTDIR/usr/local/lib/jwm/versions/1.0.0/bin/jwm") == 755 ]] ||
    fail "installed executable mode was not normalized"
[[ $(stat -c '%a' "$DESTDIR/usr/local/lib/jwm/versions/1.0.0/share/xsessions/jwm-x11rb.desktop") == 644 ]] ||
    fail "installed data-file mode was not normalized"

while IFS=$'\t' read -r kind source version_path stable_path mode extra || [[ -n ${kind:-} ]]; do
    [[ -n ${kind:-} && $kind != \#* && $stable_path != - ]] || continue
    assert_relative_link "$DESTDIR/$stable_path"
done < "$MANIFEST"

bash "$BUNDLE_V2/install-release.sh" install --destdir "$DESTDIR"
[[ $(readlink -- "$DESTDIR/usr/local/lib/jwm/current") == versions/2.0.0 ]] || fail "v2 did not become current"
assert_file_contains "$DESTDIR/usr/local/bin/jwm" release-v2:jwm

bash "$BUNDLE_V2/install-release.sh" rollback --destdir "$DESTDIR"
[[ $(readlink -- "$DESTDIR/usr/local/lib/jwm/current") == versions/1.0.0 ]] || fail "rollback did not reactivate v1"
assert_file_contains "$DESTDIR/usr/local/bin/jwm" release-v1:jwm
[[ -d $DESTDIR/usr/local/lib/jwm/versions/2.0.0 ]] || fail "rollback unexpectedly deleted v2"

# Exercise both version-selective branches: removing an inactive version must
# leave current untouched, while removing the active version must reactivate
# the latest remaining history entry.
bash "$BUNDLE_V2/install-release.sh" uninstall --destdir "$DESTDIR" --version 2.0.0
[[ $(readlink -- "$DESTDIR/usr/local/lib/jwm/current") == versions/1.0.0 ]] || fail "removing inactive v2 changed current"
[[ ! -e $DESTDIR/usr/local/lib/jwm/versions/2.0.0 ]] || fail "inactive v2 was not removed"

bash "$BUNDLE_V2/install-release.sh" install --destdir "$DESTDIR"
[[ $(readlink -- "$DESTDIR/usr/local/lib/jwm/current") == versions/2.0.0 ]] || fail "reinstalled v2 did not become current"
bash "$BUNDLE_V2/install-release.sh" uninstall --destdir "$DESTDIR" --version 2.0.0
[[ $(readlink -- "$DESTDIR/usr/local/lib/jwm/current") == versions/1.0.0 ]] || fail "removing active v2 did not reactivate v1"
assert_file_contains "$DESTDIR/usr/local/bin/jwm" release-v1:jwm

bash "$BUNDLE_V2/install-release.sh" uninstall --destdir "$DESTDIR"
[[ -f $DESTDIR/usr/local/bin/jwm && ! -L $DESTDIR/usr/local/bin/jwm ]] || fail "legacy jwm was not restored"
assert_file_contains "$DESTDIR/usr/local/bin/jwm" legacy-jwm
[[ $(stat -c '%a' "$DESTDIR/usr/local/bin/jwm") == 751 ]] || fail "legacy jwm mode was not restored"
[[ ! -e $DESTDIR/usr/local/lib/jwm && ! -L $DESTDIR/usr/local/lib/jwm ]] || fail "managed library tree remains after uninstall"

while IFS=$'\t' read -r kind source version_path stable_path mode extra || [[ -n ${kind:-} ]]; do
    [[ -n ${kind:-} && $kind != \#* && $stable_path != - ]] || continue
    [[ $stable_path == usr/local/bin/jwm ]] && continue
    [[ ! -e $DESTDIR/$stable_path && ! -L $DESTDIR/$stable_path ]] || fail "managed destination remains after uninstall: $stable_path"
done < "$MANIFEST"

# A fixed source epoch and identical inputs must produce byte-identical release
# archives, and each adjacent checksum must verify from its output directory.
PACKAGE_VERSION=9.9.9-test
FAKE_TARGET="$TMP_ROOT/fake-target"
OUTPUT_ONE="$TMP_ROOT/package-one"
OUTPUT_TWO="$TMP_ROOT/package-two"
mkdir -p -- "$FAKE_TARGET/release" "$OUTPUT_ONE" "$OUTPUT_TWO"
while IFS=$'\t' read -r kind source version_path stable_path mode extra || [[ -n ${kind:-} ]]; do
    [[ $kind == built ]] || continue
    printf '#!/usr/bin/env sh\nprintf "fake release binary: %%s\\n" "%s"\n' "$source" > "$FAKE_TARGET/release/$source"
    chmod 0755 -- "$FAKE_TARGET/release/$source"
done < "$MANIFEST"

bash "$PACKAGER" \
    --no-build \
    --version "$PACKAGE_VERSION" \
    --source-date-epoch 1700000000 \
    --target-dir "$FAKE_TARGET" \
    --output-dir "$OUTPUT_ONE"
bash "$PACKAGER" \
    --no-build \
    --version "$PACKAGE_VERSION" \
    --source-date-epoch 1700000000 \
    --target-dir "$FAKE_TARGET" \
    --output-dir "$OUTPUT_TWO"

ARCH=$(uname -m)
ARCHIVE_NAME="jwm-$PACKAGE_VERSION-linux-$ARCH.tar.gz"
ARCHIVE_ONE="$OUTPUT_ONE/$ARCHIVE_NAME"
ARCHIVE_TWO="$OUTPUT_TWO/$ARCHIVE_NAME"
[[ -f $ARCHIVE_ONE && -f $ARCHIVE_TWO ]] || fail "reproducibility archives are missing"
HASH_ONE=$(sha256sum -- "$ARCHIVE_ONE" | awk '{print $1}')
HASH_TWO=$(sha256sum -- "$ARCHIVE_TWO" | awk '{print $1}')
[[ $HASH_ONE == "$HASH_TWO" ]] || fail "fixed-epoch release archives are not reproducible"
BUNDLE_NAME=${ARCHIVE_NAME%.tar.gz}
tar -tzf "$ARCHIVE_ONE" > "$TMP_ROOT/archive-contents.txt"
for required_path in \
    release-manifest.tsv \
    install-release.sh \
    VERSION \
    ARCH \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/bin/jwm \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/bin/jwm-tool \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/bin/jwm-support \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/bin/jwm-bridge \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/bin/tao_pixels_bar \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/xsessions/jwm-x11rb.desktop \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/wayland-sessions/jwm-wayland.desktop \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/dbus-1/services/org.freedesktop.Notifications.service \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/doc/jwm/README.md \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/doc/jwm/CHANGELOG.md \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/doc/jwm/LICENSE \
    payload/usr/local/lib/jwm/versions/$PACKAGE_VERSION/share/doc/jwm/docs/architecture.md; do
    grep -Fxq -- "$BUNDLE_NAME/$required_path" "$TMP_ROOT/archive-contents.txt" ||
        fail "release archive omits $required_path"
done
(
    cd -- "$OUTPUT_ONE"
    sha256sum -c -- "$ARCHIVE_NAME.sha256"
)
(
    cd -- "$OUTPUT_TWO"
    sha256sum -c -- "$ARCHIVE_NAME.sha256"
)

printf 'test-install-lifecycle: PASS (legacy -> upgrade -> rollback -> selective/final uninstall; reproducible package %s)\n' "$HASH_ONE"
