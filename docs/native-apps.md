# Native notes and image viewer

JWM includes two Rust/GTK4 applications. Both run on X11 and Wayland and appear
in the application launcher. The source installer builds and installs both,
then registers their defaults in `$XDG_CONFIG_HOME/jwm-mimeapps.list` (normally
`~/.config/jwm-mimeapps.list`). These defaults apply when
`XDG_CURRENT_DESKTOP=jwm`, as set by JWM's session entries. Shared desktop
preferences are preserved. Release bundles register Open With entries; as with
the player, they do not modify user MIME preferences.

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
files browses only those files. File and directory operations and image
decoding happen on a worker thread. The viewer applies EXIF orientation and
limits images to 64 million pixels with a 256 MiB decoder allocation budget.
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
bars, to keep GTK dependencies out of the compositor workspace. Building these
apps requires Rust 1.92+ and GTK 4.10+.

```bash
sudo apt-get install libgtk-4-dev pkg-config  # GTK >= 4.10, Ubuntu 24.04+
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
