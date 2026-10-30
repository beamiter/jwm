# Transparent WebKitGTK backing-store fix

WebKitGTK 2.50.4 and 2.52.3 with Skia do not honor `CompositeOperator::Copy` in
`ShareableBitmap::paint`. The CPU backing store blends partial updates over
previous pixels, leaving old glyphs and increasingly opaque rectangles. Clearing
GTK draw contexts or changing the DOM cannot repair pixels already accumulated
inside that backing store.

Upstream fix: https://commits.webkit.org/317608@main
Bug: https://bugs.webkit.org/show_bug.cgi?id=319864
Patch: `shareable-bitmap-copy.patch` (unmodified upstream commit).

The [2.52.6 source](https://github.com/WebKit/WebKit/blob/webkitgtk-2.52.6/Source/WebCore/platform/graphics/skia/ShareableBitmapSkia.cpp)
already includes this fix. On Ubuntu 24.04, matching
`libwebkit2gtk-4.1-0` and `libjavascriptcoregtk-4.1-0` packages at
`2.52.6-0ubuntu0.24.04.1` can supply a private runtime without compiling WebKit.
Extract both packages into the same prefix with `dpkg-deb --extract` and create
`$prefix/lib` as a symlink to `usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH)`.
`apt-get download` verifies the packages against the configured APT repository
metadata. Do not mix packages built for different Ubuntu releases or versions.

The backport retains transparency and changes bitmap copy semantics only. Build
it into an application-specific prefix; do not overwrite system WebKit libraries.
Use the matching JavaScriptCore library and WebKit helper executables from the
same prefix. Existing applications can continue using the distribution library.

Regression test (requires GTK3/WebKitGTK 4.1 development packages and Xvfb):

```sh
cc scripts/test_webkit_transparent_repaint.c -o /tmp/webkit-repaint \
  $(pkg-config --cflags --libs webkit2gtk-4.1)
xvfb-run -a /tmp/webkit-repaint
WEBKIT_INSTALL_PREFIX="$prefix" xvfb-run -a \
  scripts/run-with-webkit-transparent.sh /tmp/webkit-repaint
```

The test forces the affected CPU backing-store path, alternates a long and short
label twelve times, then compares native RGBA pixels with a freshly rendered
native view of the final page. It checks alpha as well as premultiplied color,
with a two-level tolerance throughout. The translucent background is rectangular
to keep antialiased rounded-clip rasterization out of this bitmap-copy test.
A page snapshot alone cannot catch this bug. On Ubuntu 24.04, the unpatched
2.52.3 library fails with 2427 differing pixels; private 2.52.6 passes with zero.

Build on Ubuntu 22.04 (in addition to the repository's Tauri prerequisites):

```sh
sudo apt-get install g++-12 cmake ninja-build gperf ruby unifdef \
  libgcrypt20-dev libtasn1-6-dev libharfbuzz-dev libxslt1-dev libwebp-dev \
  libhyphen-dev libsecret-1-dev libwoff-dev libsystemd-dev libseccomp-dev \
  liblcms2-dev libenchant-2-dev flite1-dev bubblewrap xdg-dbus-proxy \
  libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev
scripts/build-webkit-transparent.sh
```

`WEBKIT_BUILD_DIR`, `WEBKIT_INSTALL_PREFIX`, and `WEBKIT_BUILD_JOBS` override
build storage, installation prefix, and concurrency. The default installation
prefix is `$HOME/.local/opt/jwm-webkitgtk-2.50.4`. The source archive is pinned by
SHA-256 and the script validates an already-applied patch before resuming.

Launch one program with the private library and matching subprocess dependencies:

```sh
WEBKIT_INSTALL_PREFIX="$prefix" scripts/run-with-webkit-transparent.sh \
  tauri_leptos_bar /dev/shm/jwm_bar_...
```

The helper defaults to `$HOME/.local/opt/jwm-webkitgtk` (which may be a symlink
to a versioned prefix), falling back to the original `jwm-webkitgtk-2.50.4`
source-build prefix. For extracted distribution packages, it uses bubblewrap
to mount the private helper directory read-only at its compiled absolute path
only for this program. This selects matching WebKit subprocess executables and
the injected bundle without replacing system files or disabling WebKit's
sandbox. The supervisor terminates the bar if its launcher exits.

For supervised bars, keep `status_bar.name = "tauri_leptos_bar"` and the
executable basename unchanged (JWM uses these to recognize bar windows). Install
the launcher with:

```sh
WEBKIT_INSTALL_PREFIX="$prefix" scripts/install-webkit-bar-launcher.sh \
  /path/to/real/tauri_leptos_bar "$HOME/.cargo/bin/tauri_leptos_bar"
```

This stores the real binary in `$prefix/libexec/jwm`, copies the runtime helper
into `$prefix/bin`, and atomically installs a launcher in the original PATH
location. An existing executable is backed up as `tauri_leptos_bar.before-webkit`;
restoring it rolls back the private library selection. `install-jwm.sh` now
retains the launcher when upgrading any Tauri bar if a complete private runtime
is present at the default prefix or `WEBKIT_INSTALL_PREFIX`. Do not export this
library path globally to the desktop session.

Validation on the affected machine: the repaired library produces zero differing
pixels with the native regression test. The background retains alpha 148/255.
`scripts/test-install-jwm-build.sh` also checks that two consecutive bar
installations preserve runtime selection and the original executable backup.

Build the bar with its embedded production frontend before installing it:

```sh
(cd bars/tauri_leptos_bar && cargo tauri build --no-bundle -- --locked)
```

A bare `cargo build --release` is insufficient here: the Tauri CLI also builds
the Leptos bundle and selects the production asset protocol.
