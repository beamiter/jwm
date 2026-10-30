# Transparent WebKitGTK backing-store fix

WebKitGTK 2.50.4 with Skia does not honor `CompositeOperator::Copy` in
`ShareableBitmap::paint`. The CPU backing store blends partial updates over
previous pixels, leaving old glyphs and increasingly opaque rectangles. Clearing
GTK draw contexts or changing the DOM cannot repair pixels already accumulated
inside that backing store.

Upstream fix: https://commits.webkit.org/317608@main
Bug: https://bugs.webkit.org/show_bug.cgi?id=319864
Patch: `shareable-bitmap-copy.patch` (unmodified upstream commit).

The backport retains transparency and changes bitmap copy semantics only. Build
it into an application-specific prefix; do not overwrite system WebKit libraries.
Use the matching JavaScriptCore library and WebKit helper executables from the
same prefix. Existing applications can continue using the distribution library.

Regression test (requires GTK3/WebKitGTK 4.1 development packages and Xvfb):

```sh
cc scripts/test_webkit_transparent_repaint.c -o /tmp/webkit-repaint \
  $(pkg-config --cflags --libs webkit2gtk-4.1)
xvfb-run -a /tmp/webkit-repaint
LD_LIBRARY_PATH="$prefix/lib" xvfb-run -a /tmp/webkit-repaint
```

The test forces the affected CPU backing-store path, alternates a long and short
label twelve times, then compares native RGBA pixels with a fresh page snapshot.
It checks alpha as well as premultiplied color. A page snapshot alone cannot
catch this bug. The unpatched 2.50.4 library fails with 1982 differing pixels.

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
scripts/run-with-webkit-transparent.sh tauri_leptos_bar /dev/shm/jwm_bar_...
```

For supervised bars, keep `status_bar.name = "tauri_leptos_bar"` and the
executable basename unchanged (JWM uses these to recognize bar windows). Install
the real binary in `$prefix/libexec/jwm/tauri_leptos_bar`, and put a launcher in
the original PATH location that invokes this helper with the real binary and
forwards all arguments. Copy the helper into `$prefix/bin` so the launcher does
not depend on the source checkout. Keep a backup of the original executable;
restoring it rolls back the private library selection. Reinstalling the bar
binary may replace this launcher, so reapply it when updating that executable.
Do not export this library path globally to the desktop session.

Validation on the affected machine: the patched library produces zero differing
pixels with the native regression test. The background retains alpha 148/255.
The test allows up to five channel levels only at the antialiased rounded
corners, where native and snapshot rasterization have slightly different
rounding; the text/update region retains the stricter two-level threshold.

Build the bar with its embedded production frontend before installing it:

```sh
(cd bars/tauri_leptos_bar && cargo tauri build --no-bundle -- --locked)
```

A bare `cargo build --release` is insufficient here: the Tauri CLI also builds
the Leptos bundle and selects the production asset protocol.
