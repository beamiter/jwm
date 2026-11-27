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

For a separate first-paint measurement, run from `crates/jwm_files`
with a private X11 display:

```sh
GDK_BACKEND=x11 GSK_RENDERER=cairo JWM_FILE_FRESH_WINDOW_SAMPLES=20 \
  dbus-run-session -- xvfb-run -a cargo test --locked bench_ui::fresh_window_benchmark -- --exact --ignored --nocapture
```

Each sample starts a fresh GTK test client using the same window code,
waits until the first known row is mapped and allocated, and observes a
completed paint. It reports P50, P95 and maximum for 100 synthetic
entries. It reuses a fixture with warm OS caches and includes test
harness startup. The CI run uses five samples for coverage, not a
release latency gate. This is not a production cold-disk or hardware result.

A separate keyboard measurement in private X11 starts a fresh Files test
window, waits for a confirmed ready listing, then sends one **Ctrl+L** and
**Return** to a fixture directory with spaces and Chinese characters. It
does not retry input. It reports the time from issuing Return to a
confirmed paint of the exact target listing and its distinctive row.
The runner time includes the key injector and test-process IPC overhead; this
is not a full startup-to-input or Finder comparison.

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
This process tries to offer `ShowFolders`, `ShowItems` and `ShowItemProperties` on the existing session bus
using the [freedesktop FileManager1 interface](https://wiki.freedesktop.org/Specifications/file-manager-interface/).
It never replaces an existing owner, queues for the name, changes desktop
settings, or installs a bus-activation service. Ordinary JWM Files launches
do not register this name.

`ShowFolders` opens only local `file://` folders, with up to eight
browser windows per process. `ShowItems` accepts one distinct local item
and reveals it in its parent without activating the item. It returns
success only after the exact selected row is visible in the list; a
missing item returns an error. A request is rejected in full if any URI
is invalid or remote. The startup ID is not used for focus negotiation
yet. Remote GVFS locations, automatic activation and default-handler
migration remain outside this slice.

The native Linux integration test starts the packaged XDG Desktop Portal
on a private X11 session bus and sends a real `OpenDirectory` descriptor
for both a regular and a hidden file. It verifies that JWM Files selects
the exact item rather than falling back to opening only its folder.
This does not configure the system’s default file manager or test a
particular browser build.

### Read-only file properties

Select a main-pane item and choose **属性** or **Alt+Enter**. The native
panel queries the exact item's available type, size, modification time,
permissions, and numeric owner and group off the UI thread. Symbolic
links show their recorded target without following it. A replaced
selection is rejected. This does not calculate recursive directory size,
change permissions, or execute/open the item.

The optional FileManager1 `ShowItemProperties` call also opens this
panel for one distinct native item. Up to four properties panels can be
open per process.

### Already-mounted locations

Expand **已挂载设备** to see mounted locations that GIO exposes as native
paths. A click opens the exact local mount folder. The list updates when
mounts are added, changed, or removed while expanded. It excludes hidden
shadowed mount aliases and remote URI-only locations. It does not mount,
unmount, eject, or request credentials.

### Copy full path

**Ctrl+Shift+C** or **复制完整路径** copies the selected item’s absolute
path as text. If the filename is not valid UTF-8 or includes a control
character, the clipboard gets a single-line local file URI that preserves
the original bytes. This does not open the item, change the directory,
or replace the separate file-copy selection. The shortcut does not
intercept text editing or act on the read-only comparison pane.

Paste a local file URI into **Ctrl+L** to revisit the exact directory,
including native names that cannot be shown as UTF-8. Explicit local URIs
decode percent escapes; ordinary path text keeps percent signs literally.
Remote file URIs are rejected without changing the current view.

### Find in the current directory

**Ctrl+F** searches the current loaded directory by a literal, case-insensitive
filename substring. Press Enter or **下一个** to select the next match, and
**上一个** to go back. The search never opens a file or walks other folders.
It does no filesystem I/O or indexing and stores no search history on disk.

A result from a previous directory or an older snapshot is never applied.
If a search hasn't finished when you press Enter, Files remembers one
explicit step for that query. Editing the query, changing directories, or
closing search discards that request.

### Copy multiple regular files

Choose **操作 → 选择多个文件复制** and use Ctrl or Shift in the picker to
select up to 128 regular files from the current local directory. Confirm
with **选定复制**, then navigate to a different directory and use **Ctrl+V**.
The same action works with the comparison pane's paste button. It does not
change the system clipboard. Same-name destinations are skipped without
overwriting and sources are never removed. The batch does not copy whole
folders or symlinks, and **Ctrl+Shift+V** is still for a single file.

Use **操作 → 查看批量复制结果** in the destination window for completed,
skipped, failed, cancelled-before-publication and unstarted results. Cancelling
does not roll back already copied files. This is a window-local report, not
persistent operation history or proof of crash durability.

### Copy a folder

The local provider can build a bounded metadata-only plan for an explicitly
selected real directory. It records exact native names, regular files and
directories, including hidden entries and empty subdirectories. It rejects
symlinks, special files, nested mounts, incomplete scans and exceeded limits
rather than returning a plan for only part of the requested tree.

This plan reads no file contents and writes nothing. It is an observation,
not a filesystem snapshot or authority to copy.

The separate Linux provider operation revalidates the plan and selected
destination, then copies at most 1 GiB of logical data into private staging.
The destination must be owned by the effective user and not group- or
world-writable. If observations still match, one no-replace rename publishes
the completed folder. An existing destination is never replaced, and the
source is never removed. A pre-publication cancellation does not publish
the final name. It does not promise metadata fidelity, a frozen source,
or power-loss durability. Unexpected or interrupted staging may remain;
it is not automatically replayed.

To use it, select a real folder and press **Ctrl+C**, navigate to the
destination, then press **Ctrl+V**. It uses the application-local clipboard,
leaves the source untouched and rejects the same folder, its descendants or
an existing same-name item. Use **操作 → 取消正在进行的复制或移动** before
publication to cancel. A folder cannot use the single-file **Ctrl+Shift+V**
automatic sibling name. Unsupported links or mounts fail the entire folder
rather than silently omitting content.

### Move one regular file

The Linux provider can move one observed regular file to an explicitly held
directory on the same mount, keeping its exact native name. Both parents must
be owned by the effective user and not writable by the group or others.
The provider rechecks the parents and source before one no-replace rename.
An existing item, including a symlink, is left intact. Cross-mount requests
are refused; there is no copy-then-delete fallback.

Cancellation is honored before the rename. If the rename succeeds but the
destination can no longer be verified, the provider reports that distinction
and does not retry or roll back. The source inode check and the rename are not
one indivisible check, and this operation does not promise power-loss
durability.

Select a regular file in the main file list, press **Ctrl+X**, open
the destination in the main file list, then press **Ctrl+V**. Cutting
does not change the system text clipboard or move the file until paste.
The Cut shortcut and a paste that would move a cut file only act when
the primary file list or grid has keyboard focus.
Use **操作 → 取消正在进行的复制或移动** to request cancellation before
publication. A move attempt consumes the selection, so if it conflicts or
is cancelled before the rename, you must cut again to retry. **Ctrl+Shift+V** and the comparison pane's
copy button do not move a cut file. This does not move directories,
batches, or files across mounts, and does not offer undo.

### Drag-copy a single regular file between Files windows (X11)

In an X11 session, select a regular file and use **拖拽复制所选文件**.
Release it on **拖到这里复制到当前目录** in a second window of the same
Files instance. No destination is changed by hovering, and only a
released drop queues a copy. Check the destination window's status for
the actual result; an accepted gesture does not mean the copy finished.
An existing name is never replaced. The source and clipboard remain
unchanged. The target is the committed directory at drop time.

This first slice does not move or link, import external drops, or accept
directories or multiple files. Wayland keeps keyboard and menu copy;
the drag controls are not exposed there in this slice.

### Copy interruption review (core only)

The local provider has a bounded record for one copy intent or an observed
publication. The record retains native path bytes. Review reopens the
destination directory without following symlinks and checks metadata only.
A file present with an intent but no publication record is indeterminate.
Matching metadata is a point-in-time observation, not proof of identity or
permission to delete, overwrite, or roll back a file. The application does not
yet write these records or offer copy history or recovery in the UI.

### Find filenames in subdirectories

**操作 → 查找子目录文件名** is separate from Ctrl+F. It starts only for the
currently loaded local directory and builds a temporary filename index in
memory. It does not read file contents, include hidden names, follow links or
cross mount boundaries. Enter searches literal text, including Chinese, and
shows up to 50 read-only relative paths. Select a result and use
**复制完整路径** to copy it as text, or as a lossless local file URI if its
native filename cannot be represented as safe single-line UTF-8. Editing or
resubmitting the query clears the preceding selection. The panel does not open
results or change the current directory. Closing it discards the index; if the bounded
scan cannot complete, Files shows an error instead of partial results.
If Enter is pressed before the index is ready, Files keeps one explicit
request for the unchanged query.

### Save and restore tabs (opt-in)

Use **操作 → 保存此窗口标签页** (**Ctrl+Alt+S**) to save the current
window's open directory tabs and active tab. In a later Files window, use
**操作 → 恢复已保存标签页** (**Ctrl+Alt+R**) to open them in a separate
window. A normal launch and exit neither read nor update the saved state.
This does not save selection, closed tabs, clipboard, navigation history,
or other windows. If a saved directory is unavailable, its tab remains
listed with the usual directory error and you can navigate to another.

Files stores one private, atomically replaced record under
`$XDG_STATE_HOME/jwm-files`, or `$HOME/.local/state/jwm-files`. A
configured absolute state root must already exist and be privately owned
and must not pass through a symlink. No default applications are changed.

### Open the current directory in a terminal

Choose **在此处打开终端** or press **Ctrl+Shift+Enter**. On Linux this
requests the configured `xdg-terminal-exec` (or Debian's
`x-terminal-emulator` when the former is missing). Files passes a held
local directory to the terminal's shell helper, not a path to reinterpret
or a shell command. If the identity or the launch acknowledgement is
unavailable, it does not report success. No desktop defaults are changed.

CI exercises the shortcut in private X11 with xterm and, separately, in
JWM's nested native Wayland compositor with foot. Both tests require a
real terminal PTY and verify the held directory after a synthetic rename
and replacement. These fixtures do not cover every configured terminal
or physical Linux desktop.

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

**Ctrl+Shift+T** or **恢复关闭的标签页** reopens the most recently closed
tab in that window, keeping both Back and Forward history. The last 16
closed tabs are retained in memory only; another window or application
restart does not inherit them. Reopening reloads the directory rather than
restoring stale rows or open file handles.

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
To keep both copies explicitly, choose **粘贴为副本** or use
**Ctrl+Shift+V** in the main pane. It works in the same or another folder
and picks a sibling name such as `report (copy).txt` or
`report (copy 2).txt`. Existing files and links are never replaced. Data
is staged only once and each of the up to 32 candidate names is published
without replacement. The status shows the selected name. Ordinary
**Ctrl+V** still refuses a conflict.

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
symlinked parent. The application does not enumerate other mounts or
offer Trash undo.

**恢复原件** is a separate action in the same window. It moves the selected
ordinary file out of Home Trash to its recorded original location after
explicit confirmation. Cancel is the default. It requires an existing,
user-controlled original directory on the same mount, refuses ambiguous
metadata, and does not replace existing files, create parents or use a
copy-delete fallback. After a confirmed move, **刷新** lists the remaining
items. If the receipt says the file or metadata could not be verified,
check both locations and do not replay the action. The copy action stays
available for records that do not qualify for moving the original, if
their recorded parent already exists.

To move one ordinary file, select it in the primary pane and choose
**操作 → 移到主目录回收站**. Review the exact path shown and confirm the
move. Cancel is the default; this action has no global Delete shortcut.
A successful move preserves the file in Home Trash. Use **恢复副本** to
copy it back without replacing an existing file or deleting the Trash
entry. If the Trash window was already open, use **刷新** to list the
new entry. This action does not support directories, symlinks or batches.

The Linux provider implements the explicit move for one observed
regular file into Home Trash. It initializes private Trash children
under an existing, user-controlled XDG data directory only on that explicit
operation. Opening the listing remains read-only. The selected parent and
Trash directories must be on the same mount; the operation refuses unsafe
ownership or permissions, changed paths and symlinks. It reserves the
original-path sidecar before a no-replace move, preserves the native file
name in that metadata, and never falls back to permanently deleting the
file. A failed sidecar cleanup or a completed move whose result cannot be
verified is reported separately; neither is permission to replay or undo.
This does not promise power-loss durability.
