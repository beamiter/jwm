# Adopting the 0.8 shell surface

Version 0.8 keeps every 0.7 surface and adds one capability: a bar can ask its
window manager to open the window manager's *own* shell surface — the launcher,
notification center, clipboard history, calendar, and wallpaper picker that
DMS, Noctalia, Caelestia, and end-4 all reach from a single bar entry.

`xbar_core` implements none of those pages and holds no state for them. A bar
names the page; the window manager owns everything the page does. That is the
whole feature, and it is why the addition is small.

## New model surface

- `ShellRoute`: `Hub`, `Applications`, `Notifications`, `Clipboard`,
  `Calendar`, `Wallpaper`. `Hub` is the home page that lists the rest.
  - `code()` / `from_code()` are the wire contract with the window manager.
    The codes are written out rather than derived from declaration order, so
    reordering the enum for readability can never repoint a running bar at a
    different page. A test pins them.
  - `next()` / `previous()` walk the pages in one cycle and are exact
    inverses, so scroll bindings on a single bar cell reach every page and
    scrolling back lands where it started.
  - `key()` / `from_key()` for configuration files. `from_key` trims, folds
    case, and accepts the ecosystem's other names (`launcher`,
    `control-center`, `background`, …) so a config copied from another shell
    keeps working.
- `UserAction::OpenShellHub(ShellRoute)` and the matching
  `WmCommand::OpenShellHub { route, monitor }`. The reducer emits the command
  and changes no local state — the bar must not start rendering its own idea
  of whether the shell is open.
- `ActionRequest::OpenShellHub { route }` for webview bridges. The wire form is
  `{ "action": "open_shell_hub", "route": "notifications" }`. `route` is
  required: "open the shell" without naming a page is ambiguous, and the home
  page is better spelled `"hub"` than left implicit.

## Presentation

`NodeId::ShellHub(ShellRoute)` carries its route, so a bar showing several
entries keeps distinct hover, damage, and hit-testing per page.

Shell entries are appended *after* `STATUS_ORDER_RIGHT_TO_LEFT`, which is
unchanged at 11 fixed controls. Because the status cluster lays out
right-to-left, that puts them at the cluster's inner (left) edge, and makes
them the first cells dropped on a narrow bar — the right trade, since a clock
or a battery reading is worth more width than a launcher.

New configuration:

```toml
[presentation.visibility]
shell_hub = true          # default: true

[presentation]
shell_routes = ["hub"]    # default; any ShellRoute keys, in left-to-right order
```

`PresentationLabels` gains `shell_routes: [String; 6]`, indexed by
`ShellRoute::code()`, with `PresentationLabels::shell_route(route)` falling
back to the built-in glyph when a host supplies a blank override. The Nerd Font
preset uses the same glyphs JWM's own shell rows use, so the bar entry and the
page it opens read as one surface.

`PresentationLabels` and `PresentationVisibility` are now `#[serde(default)]`.
Existing configuration files keep loading: adding an icon or a toggle must
never turn a running bar into a startup failure.

### Default-on, and why that is safe

`visibility.shell_hub` defaults to `true`. A window manager that does not
answer the shell command also does not hold the transport open, and the entry
is projected with `available` and `enabled` tied to `wm_available` — so it
grays out instead of swallowing clicks. There is no dead-button case to guard
against, and a bar that ships the entry off by default just hides a working
feature.

Set it to `false` to reclaim the width.

### Default bindings

Projected by `PresentationProjector` for every configured route:

| input | action |
| --- | --- |
| primary | open this route |
| secondary | the hub returns to `Applications`; any page returns to `Hub` |
| scroll up / down | open the previous / next page |

## Transport

`CommandType::ShellHub = 4` in `shared_structures`, with the route in
`parameter`. `SharedCommand::shell_hub(route, monitor)` builds one and
`SharedCommand::shell_hub_route()` reads it back, returning `None` for every
other command type.

The struct layout is untouched, so the change is wire-compatible in both
directions:

- An **older window manager** receiving a shell command sees an unknown
  `cmd_type` and ignores it, exactly as before.
- A **newer bar** naming a route an older window manager does not know gets
  `ShellHubRoute::from_raw_or_hub`, which degrades to the hub rather than
  dropping the request.

`shared_structures::ShellHubRoute` and `xbar_core::ShellRoute` stay separate
types on purpose: the model must compile without the shared-memory feature.
`transport.rs` matches exhaustively on both, so a page added to either crate is
a compile error rather than a silently misrouted command.

## Migrating

Repin and rebuild. Bars built on `CairoBar` / `LayoutEngine` get the entry with
no code change. Toolkit and webview frontends that build their own widgets add
a control that dispatches `UserAction::OpenShellHub` (or posts the
`open_shell_hub` action request).

The one thing to check: `UserAction`, `WmCommand`, `NodeId`, and
`ActionRequest` each grew a variant, so exhaustive `match` expressions over
them need a new arm.
