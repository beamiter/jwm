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

## Read-only GTK4 browser

On a system with GTK4 development libraries, open a directory with:

```sh
cargo run --locked --manifest-path crates/jwm_files/Cargo.toml -- /path/to/directory
```

The sidebar and path entry navigate local directories. Ctrl+L focuses the
address field; the refresh and hidden-items controls rescan. GTK virtualizes
row widgets, and the model is populated in bounded batches. A single scan
worker cancels and replaces obsolete requests, and does not publish stale
results even if the old provider call ignores cancellation. File activation
currently shows a path; opening documents and all file changes remain out of
scope for this slice.
