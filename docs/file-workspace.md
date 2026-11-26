# JWM File Workspace

The goal is a native daily file manager for JWM, with a standalone Rust
provider and operation layer and a GTK4 desktop client. Replace neither
the current directory handler nor the system file chooser by default.

## Milestones and ship gates

| Stage | Work | Acceptance before claiming completion |
| --- | --- | --- |
| P0 | Local provider, GTK4 navigation, places and breadcrumb, virtual list and grid, keyboard/menu, watched refresh, basic operations | Local daily browsing and basic file operations on X11 and Wayland; rapid navigation ignores stale responses; 100 to 100,000 entry fixtures; no filesystem calls on the UI thread |
| P1 | Tabs, tree, split panes, previews, copy/move queue, Trash, conflict decisions, journal, recovery, DnD | No silent overwrite; interrupted copy never claims success; cross-device move removes the source only after verification; real drop and preview tests |
| P2 | FileManager1 D-Bus, MIME opt-in/rollback, terminal and Vim9 actions, session restore, GIO/GVFS and volumes | Start/reveal from external applications; existing directory handler still works; opt-in can be undone; disconnect/remount cases tested |
| P3 | Local SQLite/FTS5 index, search and filters, incremental updates, preview metadata | Index roots are opt-in; updates after rename, eviction and dropped watcher events; real large-directory and search timings |
| P4 | Scoped LifeAI tools, local semantic search, cross-device workspace | Read-only by default; explicit, auditable file-changing requests; no shell dispatch; symlink/user boundary and permissions verified |

P0 release targets from the design: P95 cold launch to interactive 500 ms,
warm directory display 200 ms, 10,000-entry first display 500 ms. These are
targets, not measurements. Record desktop/backend, filesystem, fixture size,
warm/cold definition and sample count with any reported timings.

## First slice: isolated local provider

`crates/jwm_file_core` is a standalone Cargo workspace with no GTK or
compositor dependency. It exposes read-only flat directory snapshots,
directory-first stable ordering, byte-preserving names, distinct symlink
entries, optional Unix device/inode observations, and cooperative
cancellation. Non-fatal entry errors are counted; directory errors preserve
their OS cause. A device/inode observation is neither a permanent key nor an
authorization token. Snapshots are not atomic and a blocking OS operation
cannot be interrupted by the cancellation token.

Run from the repository root:

```sh
cargo test --locked --manifest-path crates/jwm_file_core/Cargo.toml
```

The GTK4 shell keeps the selected path as bytes, separates display labels
from path identity, keys results by request generation, and reports partial
snapshots. Default-handler migration is gated on P2 rather than performed at
install time.

## GTK4 browser

On a system with GTK4 development libraries, open a directory with:

```sh
cargo run --locked --manifest-path crates/jwm_files/Cargo.toml -- /path/to/directory
```

The sidebar and path entry navigate local directories. Ctrl+L focuses the
address field; the refresh and hidden-items controls rescan. GTK virtualizes
row widgets, and the snapshot-backed model constructs labels on demand. A single scan
worker cancels and replaces obsolete requests, and does not publish stale
results even if the old provider call ignores cancellation. See the document activation section below.


### Optional application menu launcher

The repository ships `crates/jwm_files/data/org.jwm.Files.desktop`. If
`jwm-files` is already installed on the desktop session's PATH, you can
add the launcher for your account explicitly:

```sh
install -Dm644 crates/jwm_files/data/org.jwm.Files.desktop \
  "${XDG_DATA_HOME:-$HOME/.local/share}/applications/org.jwm.Files.desktop"
```

This adds a menu launcher only. It does not advertise a folder MIME
association or change an existing or inferred default handler.
Remove that desktop entry to remove the launcher. The launcher does not start the optional FileManager1 D-Bus service.

### Opt-in FileManager1 folder requests

For manual integration testing, run `jwm-files --file-manager-service`.
This process tries to offer `ShowFolders` on the existing session bus
using the [freedesktop FileManager1 interface](https://wiki.freedesktop.org/Specifications/file-manager-interface/).
It never replaces an existing owner, queues for the name, changes desktop
settings, or installs a bus-activation service. Ordinary JWM Files launches
do not register this name.

Only local `file://` folders are accepted, with up to eight browser
windows per process. The whole request is rejected if any URI is invalid
or remote. The startup ID is not used for focus negotiation yet. Item
selection and item properties methods, GVFS volumes, and default handler
migration are outside this slice.

### Already-mounted locations

Expand **已挂载设备** to see mounted locations that GIO exposes as native
paths. A click opens the exact local mount folder. The list updates when
mounts are added, changed, or removed while expanded. It excludes hidden
shadowed mount aliases and remote URI-only locations. It does not mount,
unmount, eject, or request credentials.

### New Folder

The toolbar action or Ctrl+Shift+N creates one named folder in the currently
opened Linux directory. A worker uses the same directory handle as the scan,
and rejects a path that no longer points to that directory. It also rejects
multiple path components and never replaces an existing item. The UI refreshes
the directory on success when it is still current. Copy, move, rename,
Trash, and crash recovery are not included in this operation slice.

### Directory history

The Back and Forward toolbar controls and Alt+Left/Alt+Right revisit
successfully loaded directories. Failed or cancelled loads leave the current
directory and history intact, and refresh preserves the forward branch.
History is in memory and bounded to 128 entries.

### Open a local document

Double-click or press Enter on a selected row. Directory links are resolved
asynchronously and open in the current browser. An ordinary document goes
to the existing system MIME handler, using a URI constructed from its exact
native path rather than the visible label. Broken links report an error;
special files and desktop launchers are not run. The app does not change MIME
associations. Moving to another directory cancels an unresolved activation.

### Rename

F2 renames the selected item in place on Linux. The worker validates the
directory and the selected item's observed identity and uses atomic
no-replace rename. An existing destination is never overwritten. Selection
and filesystem changes are not one atomic action across processes, so the
source check rejects changes it observes rather than promising exclusive
access to a directory.

### Undo the last rename

**Ctrl+Z** from the file view, or **撤销重命名** in the operation menu,
reverses the most recent successful F2 rename in this window. Return to
the original directory first. If the original name is occupied or the
renamed item no longer matches, nothing is overwritten. The undo is
in-memory only; it is not a journal, does not add redo, and does not
change text-entry undo.

### Tabs

Open a tab at the current directory with **Ctrl+T** or the operation
menu. Switch with the tab strip or **Ctrl+Page Up/Down**, and close the
current tab with **Ctrl+W** without closing the last tab. Each tab keeps
its own back/forward history. Switching rescans the target and clears old
selection before allowing file actions. List/grid and hidden-item settings
are shared across the window. Up to 16 tabs can be open at once.

### Directory comparison and explicit copy

**F3** or **操作 → 目录对照栏** opens a side-by-side view at the current
folder. Use the right pane's directory rows and Back, Forward, Up, or
Refresh buttons to compare another directory without changing the
left pane. The right pane has its own worker and watcher; hidden files
stay hidden. To copy one regular file, select it in the main pane, press
**Ctrl+C**, and click **粘贴副本到此** in the right pane. The button uses
the confirmed folder, is disabled during navigation, and never replaces
a same-name file. The right pane does not redirect the main pane's file
shortcuts, rename, delete, or support drag-and-drop. F3 closes it.

### Separate windows

**Ctrl+N** or **操作 → 新建窗口** opens a second browser at the last
successfully displayed directory. Its tabs, history, selection and scanning
are independent. Use the window close control to close it; **Ctrl+W** still
closes a tab. Up to eight browser windows per process are allowed. This is
not the in-window split-pane view.

### Live directory refresh

Linux watches only the displayed local directory, non-recursively. The scan
worker coalesces notifications and rescans after a short delay; GTK does no
directory I/O. The main browser waits for a bounded wake from its workers
instead of polling at 16 ms, and uses a one-shot timer to debounce refresh.
The separate watcher and tree workers keep their existing behavior. A
same-directory refresh restores the selected item only if its path and
physical identity still match; deletion and replacement leave selection empty. Moving or unmounting the current directory invalidates old
actions until the directory is reopened. Manual refresh is still available
when a filesystem cannot supply notifications.

### Folder tree

On Linux, the sidebar's folder tree starts at the launch directory, with
**以当前目录为树根** to make the already-open folder its root. Expand an
arrow to load just that branch; double-click a folder or activate it with
the keyboard to open it in the main list. The separate bounded worker
allows other branches and the main list to remain independent. Only real
directories appear, and symlink children and repeated directory identities
are not expanded. Collapsing and reopening refreshes a branch. The hidden
items toggle rebuilds the tree from its selected root. File lists and their
watchers remain independent; expanded tree branches are not live-watched.

### Breadcrumb and desktop places

The confirmed directory is shown as clickable path segments. The ellipsis
opens older ancestors when the path is deep. Labels may be shortened, but
clicks use the underlying native path. Ctrl+L opens the address editor and
Escape leaves it without navigating. A failed address load leaves the
breadcrumb and the existing file list intact. Documents and Downloads use
the desktop's XDG user directories rather than assumed English names;
missing or duplicate configuration entries are omitted.

### Display-backend acceptance

The File Workspace CI includes private X11 and native Wayland checks. The
Wayland test uses a headless Weston compositor with software rendering and
no XWayland fall-back. It checks surface configure/buffer-attach protocol
events, GTK mapping and a frame tick, and navigation in the same window.
This is not a hardware performance or JWM compositor acceptance test.

### Large-directory row model

A directory snapshot publishes its complete row count in one GTK model
change. Label objects are created when the view requests them, rather than
prebuilding one object per item. The model retains only weak references to
live row objects and does not obtain any filesystem metadata on the UI
thread. Ordering and action paths still come from the provider snapshot.
The large-fixture test covers 100,000 rows and the last item. It does not
measure filesystem latency or the time to present a frame.

### List and icon views

Switch with the toolbar button or Ctrl+1 (list) and Ctrl+2 (icons). Both
views share the same selected item and exact action paths. Folders,
documents, symbolic links and other file kinds have distinct generic
icons. Icon mode does not request thumbnails; previews are a later
milestone.

### Staged regular-file copy primitive

The Linux core can copy one observed regular file from a pinned source
directory into a pinned destination without replacing any existing name.
It stages bytes in an anonymous inode. When the source filesystem reports
sparse extents, it writes only the reported data ranges and keeps the
logical length, including any trailing hole. Filesystems without extent
support use an ordinary byte copy. It checks cancellation and revalidates
the observed source before publication. The destination needs Linux
anonymous staging (`O_TMPFILE`). Some removable-volume filesystem drivers
do not provide it, including [VFAT](https://github.com/torvalds/linux/blob/v6.18/fs/fat/namei_vfat.c#L1105-L1115)
and [exFAT](https://github.com/torvalds/linux/blob/v6.18/fs/exfat/namei.c#L1233-L1242)
in Linux 6.18. The copy reports an unsupported destination instead of
falling back to a visible partial file. This primitive
does not delete the source, preserve file metadata, copy directories, or
provide a crash-durable completion receipt.

The **操作** menu also exposes this primitive in the file browser.
Select a regular file and press Ctrl+C, then browse to a different
directory and press Ctrl+V. The same source is available to sibling
windows in this JWM Files process, not to other applications or processes.
The browser does not modify the system clipboard or intercept normal
text-field copy/paste. At most one filesystem mutation runs per
window. Use the menu to request cancellation before publication.
The operation status remains visible while you browse elsewhere.

### Measuring local provider latency

Run this separately from CI acceptance, using a disposable filesystem with
room for 100,000 empty files (override `TMPDIR` to choose the filesystem):

```sh
cargo run --locked --release --manifest-path crates/jwm_file_core/Cargo.toml \
  --example directory_latency -- 20
```

The benchmark constructs 100, 1,000, 10,000, and 100,000-entry directories;
it validates complete reads and reports nearest-rank P50/P95 and maximum.
It fully scans once before recording samples to exercise the warm metadata
case. Fixture creation, cleanup, GTK layout, and cold filesystem cache are
not part of these numbers. Record the CPU, filesystem, OS, and build
profile before comparing results across environments.

For the GUI slice, run the opt-in release benchmark on a private X11
display (or set `GDK_BACKEND=wayland` under an appropriate Wayland fixture):

```sh
GDK_BACKEND=x11 GSK_RENDERER=cairo JWM_FILE_BENCH_SAMPLES=20 \
  xvfb-run -a -s '-screen 0 1280x800x24 -nolisten tcp' \
  cargo test --locked --release --manifest-path crates/jwm_files/Cargo.toml \
    native_listing_benchmark -- --ignored --nocapture
```

It switches a warm window between an empty directory and the test
directory. It requires a real mapped row and a client paint afterward.
The timer includes navigation, directory scan, layout, and the confirmed
paint. It is an upper bound on first client paint because it requests a
new paint for confirmation; it does not measure scanout or cold process
startup. Explicitly record the renderer and whether the display was
physical or virtual. No timings are asserted by these benchmarks.

### Text and Markdown Quick Preview

Choose **快速预览** from the operation menu, or press Space with the
main list or icon view focused, to preview the first 128 KiB of a
selected regular file. Escape or Space closes the preview.
The dialog shows the full source path. When it is already open, another
explicit preview request switches it to the newly selected file. An older
read finishing later cannot overwrite the new view.

Reading happens off the UI thread through the selected directory and
file identities, without opening links or special files. UTF-8 files
ending in `.md` or `.markdown` use the same inert Markdown formatting
as JWM Notes, and **查看源码** switches to the exact source prefix.
HTML is escaped; links and images never load external resources. Other
UTF-8 files, including TSX, remain plain text. The preview does not
execute any file and reports when only a truncated prefix is shown.
Image previews use a separate format-specific path; audio and video are not yet supported.

### PNG and JPEG Quick Preview

Select a regular `.png`, `.jpg` or `.jpeg` file and use the same Quick
Preview action. Image decoding runs off the UI thread and accepts up to 8 MiB of input, 8,388,608
pixels, and 8,192 pixels per axis. The window fits the image without
changing the file. It rejects animated or invalid PNG data, never uses an
external viewer and does not process embedded text or ICC profiles.
JPEG uses the primary EXIF orientation tag when valid, without reading
thumbnail directories or loading external resources. Progressive JPEGs
are limited to 36 scans. Other image formats and icon-grid thumbnails
are not included.

### Home Trash

**操作 → 主目录回收站** displays the home Trash in your XDG data
directory. Entries include their original path and deletion time when
metadata is available, plus the unique storage name so repeated deletes
at the same path and second remain distinguishable. Missing metadata is
shown explicitly. Non-UTF-8 bytes are displayed as distinct `\xNN`
escapes, also in the recovery confirmation. **下一批** continues the same directory scan without
retaining previous lists in memory. Each batch has up to 4,096 entries
and is sorted on its own. **刷新** starts again at the first batch, and
is required if the directory changes during a scan. Select an ordinary
file with an intact original path and choose **恢复副本** to preview the destination and confirm.
The action creates a no-overwrite copy at the original path, leaving
the Trash entry and metadata in place. It refuses a missing or
symlinked parent. It does not enumerate other mounts, move files to
Trash, remove an original Trash entry, or offer Trash undo.
