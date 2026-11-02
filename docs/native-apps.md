# Native notes and image viewer

JWM includes two native Rust applications with Cairo/Pango rendering. Both run
on X11 and Wayland and appear in the application launcher. The source installer builds and installs both,
then registers their defaults in `$XDG_CONFIG_HOME/jwm-mimeapps.list` (normally
`~/.config/jwm-mimeapps.list`). These defaults apply when
`XDG_CURRENT_DESKTOP=jwm`, as set by JWM's session entries. Shared desktop
preferences are preserved. Release bundles register Open With entries; as with
the player, they do not modify user MIME preferences.

Both apps follow the sidebars' self-drawn GUI route: Cairo paints surfaces and
line icons, Pango shapes and lays out text, and shared scene/hit-region code
handles interaction. Ordinary application windows use winit for native X11 and
Wayland xdg-shell events and softbuffer for software presentation. They do not
link GTK, GDK, WebKit, Lua or mpv. GIO is used for atomic file saves, etags and
the system trash; it does not create GUI widgets.

The dark header contains the app name, document title and window controls; the
toolbar below contains file/view actions. Drag empty header space to move the
window, double-click it to maximize/restore, or drag a window edge to resize.
Right-click the header for the window menu. Close follows the same
save/prompt path as Ctrl+W. Window requests go through the native window system.
Rendering and pointer coordinates account for the window's display scale.

Text editing supports selection, clipboard shortcuts, Unicode grapheme deletion,
undo/redo, wrapping, scrolling and platform IME composition/candidate placement.
The shared self-drawn file picker supports directory browsing and a path field
(with `~/` expansion); Enter opens the path or directory, and Escape cancels.
Tab moves through dialog buttons; Enter activates the focused button.
Saving over an existing file requires confirmation. It uses the same rendering
stack as the apps. Clipboard access on Wayland requires compositor data-control
support. File drops use the window backend's drop events.

## Notes

Run `jwm-notes` for the notebook, or `jwm-notes -- FILE…` to edit text files.
The notebook stores individual UTF-8 files under
`$XDG_DATA_HOME/jwm/notes` (normally `~/.local/share/jwm/notes`), with private
file permissions. Titles come from the first nonempty line. Changes save after
700 ms of inactivity, and before switching notes or closing. Search matches the
full text. Delete moves a note to the system trash after confirmation.

Externally opened files use explicit save and prompt before closing unsaved
changes. Saving checks the file's etag to detect external modifications;
conflicts keep the editor open so the user can save a separate copy. UTF-8 BOM
and Windows CRLF line endings are preserved. Invalid UTF-8, directories and
files over 8 MiB are rejected. Automatic note saves use atomic replacement and
retain a previous version as a `~` backup beside the file.

| Shortcut | Action |
| --- | --- |
| Ctrl+N | New note |
| Ctrl+O | Open text file in another window |
| Ctrl+S | Save |
| Ctrl+Shift+S | Export note / save text file as |
| Ctrl+F | Search the notebook |
| Ctrl+Z / Ctrl+Shift+Z | Undo / redo |
| Ctrl+W | Close, checking unsaved changes |

## Image viewer

Run `jwm-image-viewer -- FILE…`, use Ctrl+O, or drop files onto its window.
Opening one file also lists supported images in its directory; opening several
files browses only those files. Directory scanning and image decoding happen on
a worker thread. Rapid navigation keeps one active decoder and coalesces pending
requests. The viewer applies EXIF orientation and limits images to 64 million pixels with a 256 MiB decoder allocation budget.
PNG, JPEG, WebP, GIF, BMP, TIFF, ICO and PNM/PPM/PGM/PBM/PAM are supported.
Animated formats display their first frame. Rotation changes only the view.

| Shortcut | Action |
| --- | --- |
| Ctrl+O | Open images |
| Left / Right, Page Up / Page Down | Previous / next (wraps) |
| Space | Next image |
| + / - or Ctrl+wheel | Zoom (5%–1600%) |
| 1 | Original size |
| F / 0 | Fit to window |
| R | Rotate clockwise |
| F11 / Escape | Toggle / leave fullscreen |
| Drag image | Pan a zoomed image |
| Ctrl+W | Close |

## Build separately

The apps use a separate Cargo package and lockfile, like JWM's shared shell and
bars, to keep application dependencies out of the compositor workspace. Building
these apps requires Rust 1.92+ and the same Cairo/Pango/GLib development libraries
as the shared sidebars, plus X11/Wayland and xkbcommon runtime libraries. GTK4
and Ubuntu 24.04 are not required.

```bash
sudo apt-get install libcairo2-dev libpango1.0-dev libglib2.0-dev libxkbcommon-dev pkg-config
cargo build --locked --release --manifest-path crates/jwm_apps/Cargo.toml \
  --target-dir target/native-apps
target/native-apps/release/jwm-notes
target/native-apps/release/jwm-image-viewer -- /path/to/photo.jpg
```

The full installer is `bash scripts/install-jwm.sh`. Its `--skip-jwm` option
also skips these apps. The desktop IDs are `org.jwm.Notes.desktop` and
`org.jwm.ImageViewer.desktop`, matching their Wayland application identities.

To install only these two apps for the current user, without rebuilding JWM or
requiring sudo, run `bash scripts/install-native-apps.sh`. This installs binaries
to `~/.local/bin`, desktop entries to the user's applications directory, and
sets JWM-specific text/image defaults. Use `--debug` for a development build.

## Verification

Run the data-model/editor tests and isolated native-window regressions:

```bash
cargo test --locked --manifest-path crates/jwm_apps/Cargo.toml --target-dir target/native-apps
python3 scripts/test-native-apps.py
python3 scripts/test-native-apps-wayland.py --scale 1
python3 scripts/test-native-apps-wayland.py --scale 2
```

The X11 test needs Xvfb, xdotool, ImageMagick and Pillow; the Wayland test needs
Sway, wtype, grim and Pillow. Each creates a private display, session bus and
application-data directory, and never controls an existing desktop.
