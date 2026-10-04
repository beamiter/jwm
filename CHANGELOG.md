# Changelog

Notable user-visible changes to JWM are recorded here. Components in this
monorepo use independent Semantic Versions.

## [Unreleased]

### Added

- `get_monitors` / `get_tree` monitor rows report `bar_visible`: whether the
  status-bar window currently occupies that output. Distinct from `show_bar`,
  which remains the per-tag preference and stays true during F11 fullscreen.
- `get_show_bar` includes `bar_visible` beside the per-tag `show_bar`
  preference, matching monitor rows.
- `get_monitors` / `get_tree` monitor rows report `has_visible_fullscreen`
  for a non-hidden fullscreen client on the current view, the same predicate
  that hides the status bar.
- `get_show_bar` also reports `has_visible_fullscreen` beside `bar_visible`.
- `get_bar_visible` aliases `get_show_bar`.
- `get_workspaces` reports `has_visible_fullscreen` per tag: true only when
  that tag is on view and a live fullscreen client owns the output.
- `get_windows` / `get_tree` / `window/state` report `owns_output` when a
  visible fullscreen client currently covers the monitor.
- Monitor, workspace and tree rows report `owns_output_count`.
- `get_show_bar` also reports `owns_output_count`.
- `get_monitors` `owns_output_count` uses the same hide-bar visibility
  predicate as `get_show_bar`.
- `get_workspaces` `owns_output_count` is the monitor hide-bar count on the
  active tag and `0` off-view.
- `get_tree` monitor nodes use the same hide-bar `owns_output_count`.
- `WindowInfo.owns_output` is false for swallowed terminals.
- Tag client counts exclude swallowed terminals from `owns_output`.
- `get_owns_output` aliases `get_show_bar`.
- Show-bar IPC snapshots can be built for any monitor, not only the focused
  one.
- F11 fullscreen flips emit `monitor/bar` with the same occupancy snapshot.
- `togglebar` emits `monitor/bar` after the layout and bar window settle.
- F11 and `togglebar` share one `broadcast_monitor_bar_ipc` helper.
- Tag `view` emits `monitor/bar` because per-tag `show_bar` and visible
  fullscreen occupancy can both change.
- `toggleview` emits `monitor/bar` after arrange.
- Fullscreen layout enter/leave emits `monitor/bar` after the bar window
  is parked or restored.
- Minimizing or restoring a fullscreen client emits `monitor/bar`.
- Unmanaging a fullscreen client emits `monitor/bar` after `window/close`.
- Swallowing or unswallowing a terminal emits `monitor/bar` for its
  monitor.
- Moving a fullscreen client between outputs emits `monitor/bar` on both.
- `focusmon` emits `monitor/bar` for the newly focused output.
- Pointer monitor switches emit `monitor/focus` and `monitor/bar`.
- Session restore emits `monitor/bar` for every output.
- External strut apply/remove emits `monitor/bar` for every output.
- Output hotplug emits `monitor/bar` for every remaining output.
- Subscribe topic `bar` aliases `monitor/bar` (occupancy only, not
  `monitor/focus`).
- `jwm-tool msg --subscribe bar` is documented as occupancy-only.
- README subscription example includes `bar`; the ack stores it as
  `monitor/bar`.
- `tools/README.md` subscribe example includes `bar`.
- `WorkspaceInfo.owns_output_count` rustdoc names the hide-bar occupancy
  rule (zero off-view).
- `TreeNode.owns_output_count` rustdoc names hide-bar occupancy.
- `MonitorInfoIpc.owns_output_count` rustdoc excludes swallowed terminals.
- `query_show_bar_for_monitor` rustdoc lists the occupancy JSON keys.
- `broadcast_monitor_bar_ipc` rustdoc names the `bar` subscribe alias.
- `get_status.show_bar` rustdoc twins the occupancy snapshot aliases.
- `toggletag` on a fullscreen client emits `monitor/bar`.
- Moving a fullscreen client to another tag emits `monitor/bar`.
- Toggling sticky on a fullscreen client emits `monitor/bar`.
- Window-tabs docs name the `bar` occupancy subscription.
- `get_visible_fullscreen` aliases `get_show_bar`.
- `get_status.show_bar` rustdoc includes `get_visible_fullscreen`.
- `get_vf` aliases `get_visible_fullscreen`.
- `get_status.show_bar` rustdoc includes `get_vf`.
- `jwm-tool msg` help lists `get_show_bar` and `get_vf`.
- README control examples include `get_show_bar`.
- `tools/README.md` control examples include `get_show_bar`.
- `jwm-tool msg` help lists `get_owns_output`.
- `jwm-tool msg` help lists `get_visible_fullscreen`.
- `jwm-tool msg` help lists `get_bar_visible`.
- `jwm-tool msg` help lists `get_bar`.
- README control examples include `get_vf`.
- `tools/README.md` control examples include `get_vf`.
- `jwm-tool msg` after-help examples include `get_vf`.
- `jwm-tool msg` after-help examples include `get_show_bar`.
- `jwm-tool capabilities` text lists `bar->monitor/bar` and
  `workspace->tag` aliases.
- `jwm-tool capabilities` text lists occupancy query aliases of
  `get_show_bar`.
- `jwm-tool health` prints focused-bar preference, visibility and
  `owns_output` count when `get_status.show_bar` is present.
- `jwm-tool health` occupancy line includes `has_visible_fullscreen`.
- `jwm-tool health` occupancy line appends `connector` when known.
- `jwm-tool health` occupancy line includes the monitor number.
- `get_show_bar` occupancy snapshots include `tag` (`Pertag.cur_tag`).
- `jwm-tool health` occupancy line includes the current tag.
- `get_show_bar` occupancy snapshots include `layout` (`lt_symbol`).
- Layout changes emit `monitor/bar` after `layout/set`.
- `jwm-tool health` occupancy line includes the current layout.
- `get_show_bar` occupancy snapshots include `gap`.
- `jwm-tool health` occupancy line includes the current gap.
- `setgaps` emits `monitor/bar` after arrange.
- `get_show_bar` occupancy snapshots include `mfact`.
- `jwm-tool health` occupancy line includes the current mfact.
- `setmfact` emits `monitor/bar` after arrange.
- `get_show_bar` occupancy snapshots include `nmaster`.
- `jwm-tool health` occupancy line includes the current nmaster.
- `setnmaster` emits `monitor/bar` after arrange.
- Scrolling column-width `setmfact` emits `monitor/bar` after arrange.
- `get_show_bar` occupancy snapshots include `prev_tag`.
- `jwm-tool health` occupancy line includes prev_tag.
- `get_status.show_bar` rustdoc names tag / prev_tag / layout / gap / mfact / nmaster.
- README names occupancy `tag` / `prev_tag` / `layout` / `gap` / `mfact` / `nmaster`.
- `tools/README.md` names the same occupancy keys.
- Window-tabs docs name occupancy `tag` / `layout` / `gap` / `mfact` / `nmaster`.
- `get_show_bar` occupancy snapshots include `selected_id`.
- `jwm-tool health` occupancy line includes selected_id.
- `focusstack` emits `monitor/bar` after the selection changes.
- Scrolling in-column focus emits `monitor/bar` after arrange.
- `focus_none` emits `monitor/bar` after dropping selection.
- `focus_window` emits `monitor/bar` after a successful reveal.
- `get_status.show_bar` rustdoc names `selected_id`.
- README occupancy JSON names `selected_id`.
- `tools/README.md` occupancy JSON names `selected_id`.
- Window-tabs docs name occupancy `selected_id`.
- `get_show_bar` occupancy snapshots include `sel_tags`.
- `get_show_bar` occupancy snapshots include `previous_tags`.
- `jwm-tool health` occupancy line includes sel_tags.
- `jwm-tool health` occupancy line includes previous_tags.
- `get_show_bar` occupancy snapshots include `active_tags`.
- `jwm-tool health` occupancy line includes active_tags.
- `get_status.show_bar` rustdoc names sel_tags / previous_tags / active_tags.
- README occupancy JSON names sel_tags / previous_tags / active_tags.
- `tools/README.md` occupancy JSON names sel_tags / previous_tags / active_tags.
- Window-tabs docs name occupancy sel_tags / previous_tags / active_tags.
- `zoom` emits `monitor/bar` after promoting a client.
- `get_show_bar` occupancy snapshots include `window_count`.
- `jwm-tool health` occupancy line includes window_count.
- Managing a client emits `monitor/bar` after `window/new`.
- Unmanaging a client emits `monitor/bar` even when it was not fullscreen.
- `get_status.show_bar` rustdoc names `window_count`.
- README occupancy JSON names `window_count`.
- `tools/README.md` occupancy JSON names `window_count`.
- Window-tabs docs name occupancy `window_count`.
- `get_show_bar` occupancy snapshots include `on_view_count`.
- `jwm-tool health` occupancy line includes on_view_count.
- `get_status.show_bar` rustdoc names `on_view_count`.
- README occupancy JSON names `on_view_count`.
- `tools/README.md` occupancy JSON names `on_view_count`.
- Window-tabs docs name occupancy `on_view_count`.
- `get_show_bar` occupancy snapshots include `floating_count`.
- `jwm-tool health` occupancy line includes floating_count.
- `togglefloating` emits `monitor/bar` after arrange.
- `get_status.show_bar` rustdoc names `floating_count`.
- README occupancy JSON names `floating_count`.
- `tools/README.md` occupancy JSON names `floating_count`.
- Window-tabs docs name occupancy `floating_count`.
- `get_show_bar` occupancy snapshots include `minimized_count`.
- `jwm-tool health` occupancy line includes minimized_count.
- Minimizing or restoring a client emits `monitor/bar` even when it was not fullscreen.
- `get_status.show_bar` rustdoc names `minimized_count`.
- README occupancy JSON names `minimized_count`.
- `tools/README.md` occupancy JSON names `minimized_count`.
- Window-tabs docs name occupancy `minimized_count`.
- `get_show_bar` occupancy snapshots include `sticky_count`.
- `jwm-tool health` occupancy line includes sticky_count.
- Toggling sticky emits `monitor/bar` even when the client was not fullscreen.
- `get_status.show_bar` rustdoc names `sticky_count`.
- README occupancy JSON names `sticky_count`.
- `tools/README.md` occupancy JSON names `sticky_count`.
- Window-tabs docs name occupancy `sticky_count`.
- `get_show_bar` occupancy snapshots include `urgent_count`.
- `jwm-tool health` occupancy line includes urgent_count.
- Urgency changes emit `monitor/bar`.
- Demands-attention changes emit `monitor/bar`.
- `get_status.show_bar` rustdoc names `urgent_count`.
- README occupancy JSON names `urgent_count`.
- `tools/README.md` occupancy JSON names `urgent_count`.
- Window-tabs docs name occupancy `urgent_count`.
- `get_show_bar` occupancy snapshots include `fullscreen_count`.
- `jwm-tool health` occupancy line includes fullscreen_count.
- `get_status.show_bar` rustdoc names `fullscreen_count`.
- README occupancy JSON names `fullscreen_count`.
- `tools/README.md` occupancy JSON names `fullscreen_count`.
- Window-tabs docs name occupancy `fullscreen_count`.
- `get_show_bar` occupancy snapshots include `pip_count`.
- `jwm-tool health` occupancy line includes pip_count.
- Toggling picture-in-picture emits `monitor/bar`.
- `get_status.show_bar` rustdoc names `pip_count`.
- README occupancy JSON names `pip_count`.
- `tools/README.md` occupancy JSON names `pip_count`.
- Window-tabs docs name occupancy `pip_count`.
- `get_show_bar` occupancy snapshots include `maximized_count`.
- `jwm-tool health` occupancy line includes maximized_count.
- Toggling maximize emits `monitor/bar`.
- Unmaximize-in-place emits `monitor/bar`.
- Reinstating a maximize snapshot emits `monitor/bar`.
- `get_status.show_bar` rustdoc names `maximized_count`.
- README occupancy JSON names `maximized_count`.
- `tools/README.md` occupancy JSON names `maximized_count`.
- Window-tabs docs name occupancy `maximized_count`.
- `get_show_bar` occupancy snapshots include `above_count`.
- `jwm-tool health` occupancy line includes above_count.
- Keep-above and keep-below changes emit `monitor/bar`.
- `get_status.show_bar` rustdoc names `above_count`.
- README occupancy JSON names `above_count`.
- `tools/README.md` occupancy JSON names `above_count`.
- Window-tabs docs name occupancy `above_count`.
- `get_show_bar` occupancy snapshots include `below_count`.
- `jwm-tool health` occupancy line includes below_count.
- `get_status.show_bar` rustdoc names `below_count`.
- README occupancy JSON names `below_count`.
- `tools/README.md` occupancy JSON names `below_count`.
- Window-tabs docs name occupancy `below_count`.
- `get_show_bar` occupancy snapshots include `scratchpad_count`.
- `jwm-tool health` occupancy line includes scratchpad_count.
- Hiding a scratchpad emits `monitor/bar`.
- Showing a scratchpad emits `monitor/bar`.
- `get_status.show_bar` rustdoc names `scratchpad_count`.
- README occupancy JSON names `scratchpad_count`.
- `tools/README.md` occupancy JSON names `scratchpad_count`.
- Window-tabs docs name occupancy `scratchpad_count`.
- `get_show_bar` occupancy snapshots include `tabbed_count`.
- `jwm-tool health` occupancy line includes tabbed_count.
- `get_status.show_bar` rustdoc names `tabbed_count`.
- README occupancy JSON names `tabbed_count`.
- `tools/README.md` occupancy JSON names `tabbed_count`.
- Window-tabs docs name occupancy `tabbed_count`.
- `get_show_bar` occupancy snapshots include `dock_count`.
- `jwm-tool health` occupancy line includes dock_count.
- `get_status.show_bar` rustdoc names `dock_count`.
- README occupancy JSON names `dock_count`.
- `tools/README.md` occupancy JSON names `dock_count`.
- Window-tabs docs name occupancy `dock_count`.
- `get_show_bar` occupancy snapshots include `desktop_count`.
- `jwm-tool health` occupancy line includes desktop_count.
- `get_status.show_bar` rustdoc names `desktop_count`.
- README occupancy JSON names `desktop_count`.
- `tools/README.md` occupancy JSON names `desktop_count`.
- Window-tabs docs name occupancy `desktop_count`.
- `get_show_bar` occupancy snapshots include `never_focus_count`.
- `jwm-tool health` occupancy line includes never_focus_count.
- `get_status.show_bar` rustdoc names `never_focus_count`.
- README occupancy JSON names `never_focus_count`.
- `tools/README.md` occupancy JSON names `never_focus_count`.
- Window-tabs docs name occupancy `never_focus_count`.
- `get_show_bar` occupancy snapshots include `skip_taskbar_count`.
- `jwm-tool health` occupancy line includes skip_taskbar_count.
- `get_status.show_bar` rustdoc names `skip_taskbar_count`.
- README occupancy JSON names `skip_taskbar_count`.
- `tools/README.md` occupancy JSON names `skip_taskbar_count`.
- Window-tabs docs name occupancy `skip_taskbar_count`.
- `get_show_bar` occupancy snapshots include `skip_pager_count`.
- `jwm-tool health` occupancy line includes skip_pager_count.
- `get_status.show_bar` rustdoc names `skip_pager_count`.
- README occupancy JSON names `skip_pager_count`.
- `tools/README.md` occupancy JSON names `skip_pager_count`.
- Window-tabs docs name occupancy `skip_pager_count`.
- `_NET_WM_STATE_SKIP_TASKBAR` emits `monitor/bar`.
- `_NET_WM_STATE_SKIP_PAGER` emits `monitor/bar`.
- `get_show_bar` occupancy snapshots include `no_decorations_count`.
- `jwm-tool health` occupancy line includes no_decorations_count.
- `get_status.show_bar` rustdoc names `no_decorations_count`.
- README occupancy JSON names `no_decorations_count`.
- `tools/README.md` occupancy JSON names `no_decorations_count`.
- Window-tabs docs name occupancy `no_decorations_count`.
- Reconciling decoration hints emits `monitor/bar`.
- `get_show_bar` occupancy snapshots include `drag_float_count`.
- `jwm-tool health` occupancy line includes drag_float_count.
- `get_status.show_bar` rustdoc names `drag_float_count`.
- README occupancy JSON names `drag_float_count`.
- `tools/README.md` occupancy JSON names `drag_float_count`.
- Window-tabs docs name occupancy `drag_float_count`.
- Drag-start floating emits `monitor/bar`.
- `get_show_bar` occupancy snapshots include `swallowed_count`.
- `jwm-tool health` occupancy line includes swallowed_count.
- `jwm-tool` raises the serde_json fixture recursion limit to 512.
- `get_status.show_bar` rustdoc names `swallowed_count`.
- README occupancy JSON names `swallowed_count`.
- `tools/README.md` occupancy JSON names `swallowed_count`.
- Window-tabs docs name occupancy `swallowed_count`.
- `get_show_bar` occupancy snapshots include `demands_attention_count`.
- `jwm-tool health` occupancy line includes demands_attention_count.
- `get_status.show_bar` rustdoc names `demands_attention_count`.
- README occupancy JSON names `demands_attention_count`.
- `tools/README.md` occupancy JSON names `demands_attention_count`.
- Window-tabs docs name occupancy `demands_attention_count`.
- `get_show_bar` occupancy snapshots include `fixed_count`.
- `jwm-tool health` occupancy line includes fixed_count.
- `get_status.show_bar` rustdoc names `fixed_count`.
- README occupancy JSON names `fixed_count`.
- `tools/README.md` occupancy JSON names `fixed_count`.
- Window-tabs docs name occupancy `fixed_count`.
- `get_show_bar` occupancy snapshots include `strut_count`.
- `jwm-tool health` occupancy line includes strut_count.
- `get_status.show_bar` rustdoc names `strut_count`.
- README occupancy JSON names `strut_count`.
- `tools/README.md` occupancy JSON names `strut_count`.
- Window-tabs docs name occupancy `strut_count`.
- `get_show_bar` occupancy snapshots include `maximize_promoted_count`.
- `jwm-tool health` occupancy line includes maximize_promoted_count.
- `get_status.show_bar` rustdoc names `maximize_promoted_count`.
- README occupancy JSON names `maximize_promoted_count`.
- `tools/README.md` occupancy JSON names `maximize_promoted_count`.
- Window-tabs docs name occupancy `maximize_promoted_count`.
- `get_show_bar` occupancy snapshots include `status_bar_count`.
- `jwm-tool health` occupancy line includes status_bar_count.
- `get_status.show_bar` rustdoc names `status_bar_count`.
- README occupancy JSON names `status_bar_count`.
- `tools/README.md` occupancy JSON names `status_bar_count`.
- Window-tabs docs name occupancy `status_bar_count`.
- Cancelling a pointer drag emits `monitor/bar`.
- Size-hint fixed-state changes emit `monitor/bar`.
- Window-type dock/desktop updates emit `monitor/bar`.
- WM_HINTS never-focus changes emit `monitor/bar`.
- `get_show_bar` occupancy snapshots include `prev_layout`.
- Library crate raises the serde_json fixture recursion limit to 512.
- `jwm-tool health` occupancy line includes prev_layout.
- `get_status.show_bar` rustdoc names `prev_layout`.
- README occupancy JSON names `prev_layout`.
- `tools/README.md` occupancy JSON names `prev_layout`.
- Window-tabs docs name occupancy `prev_layout`.
- Drag-snap drops emit `monitor/bar`.
- `setcfact` emits `monitor/bar`.
- External strut property updates emit `monitor/bar`.
- External strut rehosts emit `monitor/bar`.
- Window-tab reorders emit `monitor/bar`.
- `get_show_bar` occupancy snapshots include `closed_placement_count`.
- `jwm-tool health` occupancy line includes closed_placement_count.
- `get_status.show_bar` rustdoc names `closed_placement_count`.
- README occupancy JSON names `closed_placement_count`.
- `tools/README.md` occupancy JSON names `closed_placement_count`.
- Window-tabs docs name occupancy `closed_placement_count`.
- Tag client counts accumulate `closed_placement`.
- Workspace rows report `closed_placement_count`.
- Monitor rows report `closed_placement_count`.
- Tree nodes report `closed_placement_count`.
- Portal monitor rows deserialize `closed_placement_count`.
- Portal window rows deserialize `remembers_closed_placement`.
- `incnmaster` emits `monitor/bar`.
- Scrolling column moves emit `monitor/bar`.
- Scrolling consume emits `monitor/bar`.
- Scrolling expel emits `monitor/bar`.
- Output geometry changes emit `monitor/bar`.
- WM_CLASS status-bar identity flips emit `monitor/bar`.
- Title status-bar identity flips emit `monitor/bar`.
- Config reload emits `monitor/bar`.
- Scrolling column focus emits `monitor/bar`.
- `movestack` emits `monitor/bar`.
- Closed-placement settle emits `monitor/bar`.
- `pop` emits `monitor/bar`.
- Transient-for floating emits `monitor/bar`.
- `get_workspaces` docs name `closed_placement_count`.
- `get_monitors` docs name `closed_placement_count`.
- `get_tree` docs name `closed_placement_count`.
- Overview confirm emits `monitor/bar`.
- Expose exit emits `monitor/bar`.
- Window-switcher commit emits `monitor/bar`.
- Window-placement docs name `closed_placement_count`.
- WM setup emits `monitor/bar`.
- `focus_tab` emits `monitor/bar`.
- Expose docs name `monitor/bar` on focused exit.
- Tags-overview docs name `monitor/bar` on confirm.
- Window-switcher docs name `monitor/bar` on commit.
- Layout-picker docs name `monitor/bar` on live apply.
- `get_closed_placement` / `get_cp` return the focused monitor's
  `closed_placement_count`.
- Capabilities list `get_closed_placement` and `get_cp`.
- `get_status` nests compact `closed_placement` beside `prev_layout`.
- `jwm-tool msg` help lists `get_closed_placement` and `get_cp`.
- `jwm-tool msg` after-help examples include `get_cp`.
- README control examples include `get_cp`.
- `tools/README.md` control examples include `get_cp`.
- Compatibility docs name `get_cp` among short query aliases.
- Window-placement docs name `get_closed_placement` / `get_cp`.
- Focused layout-knob query docs name `get_closed_placement`.
- Launcher window activation emits `monitor/bar`.
- Launcher docs name `monitor/bar` on window activation.
- `_NET_ACTIVE_WINDOW` activation emits `monitor/bar`.
- Foreign-toplevel activate emits `monitor/bar`.
- Cube-effects docs name `monitor/bar` on overview confirm.
- `jwm-tool capabilities` text lists `get_cp -> get_closed_placement`.
- `jwm-tool health` prints compact `closed_placement` beside occupancy.
- `get_status.closed_placement` rustdoc names `get_closed_placement` / `get_cp`.
- Minimized-dock docs name `monitor/bar` on restore.
- README health text names compact `closed_placement`.
- `tools/README.md` health text names compact `closed_placement`.
- Window-tabs docs name `get_closed_placement` / `get_cp`.
- `jwm-tool health` prints compact `prev_layout` beside occupancy.
- README health text names compact `prev_layout`.
- `tools/README.md` health text names compact `prev_layout`.
- `jwm-tool capabilities` text lists `get_pl -> get_prev_layout`.
- Window-tabs docs name `monitor/bar` on focus / reorder.
- `jwm-tool msg` help lists `get_prev_layout` and `get_pl`.
- `jwm-tool msg` after-help examples include `get_pl`.
- README control examples include `get_pl`.
- `tools/README.md` control examples include `get_pl`.
- `jwm-tool health` prints compact `cfact` beside occupancy.
- `jwm-tool health` prints compact `gaps` beside occupancy.
- `jwm-tool health` prints compact `mfact` beside occupancy.
- `jwm-tool health` prints compact `nmaster` beside occupancy.
- README health text names compact `cfact` / `gaps` / `mfact` / `nmaster`.
- `tools/README.md` health text names compact `cfact` / `gaps` / `mfact` /
  `nmaster`.
- `jwm-tool capabilities` text lists `get_cf -> get_cfact`.
- `jwm-tool capabilities` text lists `get_gap -> get_gaps`.
- `jwm-tool capabilities` text lists `get_mf -> get_mfact`.
- `jwm-tool capabilities` text lists `get_nm -> get_nmaster`.
- `jwm-tool health` prints compact `layout` beside occupancy.
- `jwm-tool capabilities` text lists `get_lt -> get_layout`.
- README health text names compact `layout`.
- `tools/README.md` health text names compact `layout`.
- `jwm-tool health` prints compact `tabs` beside occupancy.
- README health text names compact `tabs`.
- `tools/README.md` health text names compact `tabs`.
- `jwm-tool capabilities` text lists `get_tab,get_tabs -> get_tab_bar`.
- `jwm-tool health` prints compact `selected` beside occupancy.
- README health text names compact `selected`.
- `tools/README.md` health text names compact `selected`.
- `jwm-tool capabilities` text lists `get_sel -> get_selected`.
- `jwm-tool health` prints compact `struts` beside occupancy.
- README health text names compact `struts`.
- `tools/README.md` health text names compact `struts`.
- `jwm-tool capabilities` text lists `get_strut -> get_struts`.
- `jwm-tool health` prints compact `scratchpads` beside occupancy.
- README health text names compact `scratchpads`.
- `tools/README.md` health text names compact `scratchpads`.
- `jwm-tool capabilities` text lists `get_pads,get_scratch -> get_scratchpads`.
- `jwm-tool health` prints compact `focused` beside occupancy.
- README health text names compact `focused`.
- `tools/README.md` health text names compact `focused`.
- `jwm-tool capabilities` text lists `get_fw -> get_focused_window`.
- `jwm-tool health` prints compact `monitors` beside occupancy.
- README health text names compact `monitors`.
- `tools/README.md` health text names compact `monitors`.
- `jwm-tool capabilities` text lists `get_mons,get_outputs -> get_monitors`.
- `jwm-tool health` prints compact `workspaces` beside occupancy.
- README health text names compact `workspaces`.
- `tools/README.md` health text names compact `workspaces`.
- `jwm-tool capabilities` text lists `get_ws,get_tags,get_desktops -> get_workspaces`.
- `jwm-tool health` prints compact `windows` beside occupancy.
- README health text names compact `windows`.
- `tools/README.md` health text names compact `windows`.
- `jwm-tool capabilities` text lists `get_wins,get_clients,get_cli -> get_windows`.
- `jwm-tool health` prints compact `tree` beside occupancy.
- README health text names compact `tree`.
- `tools/README.md` health text names compact `tree`.
- `jwm-tool health` prints compact `effects` beside occupancy.
- README health text names compact `effects`.
- `tools/README.md` health text names compact `effects`.
- `jwm-tool capabilities` text lists `get_fx,get_effects -> get_effect_status`.
- `jwm-tool health` prints compact `mic` beside occupancy.
- README health text names compact `mic`.
- `tools/README.md` health text names compact `mic`.
- `jwm-tool capabilities` text lists `get_mic,get_mute -> get_mic_mute`.
- `jwm-tool health` prints compact `bench` beside occupancy.
- README health text names compact `bench`.
- `tools/README.md` health text names compact `bench`.
- `jwm-tool capabilities` text lists `get_bench,get_bm -> benchmark_report`.
- `jwm-tool health` prints compact `floating` beside occupancy.
- README health text names compact `floating`.
- `tools/README.md` health text names compact `floating`.
- `jwm-tool health` prints compact `minimized` beside occupancy.
- README health text names compact `minimized`.
- `tools/README.md` health text names compact `minimized`.
- `jwm-tool health` prints compact `sticky` beside occupancy.
- README health text names compact `sticky`.
- `tools/README.md` health text names compact `sticky`.
- `jwm-tool health` prints compact `urgent` beside occupancy.
- README health text names compact `urgent`.
- `tools/README.md` health text names compact `urgent`.
- `jwm-tool health` prints compact `fullscreen` beside occupancy.
- README health text names compact `fullscreen`.
- `tools/README.md` health text names compact `fullscreen`.
- `jwm-tool health` prints compact `pip` beside occupancy.
- README health text names compact `pip`.
- `tools/README.md` health text names compact `pip`.
- `jwm-tool health` prints compact `notifications` beside occupancy.
- README health text names compact `notifications`.
- `tools/README.md` health text names compact `notifications`.
- `jwm-tool health` prints compact `blur` beside occupancy.
- README health text names compact `blur`.
- `tools/README.md` health text names compact `blur`.
- `jwm-tool health` prints compact `hdr` beside occupancy.
- README health text names compact `hdr`.
- `tools/README.md` health text names compact `hdr`.
- `jwm-tool health` prints compact `dnd` beside occupancy.
- README health text names compact `dnd`.
- `tools/README.md` health text names compact `dnd`.
- `jwm-tool health` prints compact `system_ui` beside occupancy.
- README health text names compact `system_ui`.
- `tools/README.md` health text names compact `system_ui`.
- `jwm-tool capabilities` text lists `get_ui -> get_system_ui`.
- `jwm-tool health` prints compact `idle` beside occupancy.
- README health text names compact `idle`.
- `tools/README.md` health text names compact `idle`.
- `jwm-tool health` prints compact `clipboard` beside occupancy.
- README health text names compact `clipboard`.
- `tools/README.md` health text names compact `clipboard`.
- `jwm-tool health` prints compact `session_lock` beside occupancy.
- README health text names compact `session_lock`.
- `tools/README.md` health text names compact `session_lock`.
- `jwm-tool capabilities` text lists `get_lock,get_sess -> get_session_lock`.
- `jwm-tool health` prints compact `tearing` beside occupancy.
- README health text names compact `tearing`.
- `tools/README.md` health text names compact `tearing`.
- `jwm-tool capabilities` text lists `get_tearing,get_th -> get_tearing_hints`.
- `jwm-tool health` prints compact `xwayland` beside occupancy.
- README health text names compact `xwayland`.
- `tools/README.md` health text names compact `xwayland`.
- `jwm-tool capabilities` text lists `get_xwayland,get_xw -> get_xwayland_status`.
- `jwm-tool health` prints compact `scrolling` beside occupancy.
- README health text names compact `scrolling`.
- `tools/README.md` health text names compact `scrolling`.
- `jwm-tool capabilities` text lists `get_scrolling -> get_scrolling_status`.
- IPC short query alias `get_scroll` reaches `get_scrolling_status`.
- `jwm-tool health` prints compact `color_management` beside occupancy.
- README health text names compact `color_management`.
- `tools/README.md` health text names compact `color_management`.

- `jwm-tool capabilities` text lists `get_cm -> get_color_management_status`.

- `jwm-tool health` prints compact `night_light` beside occupancy.

- README health text names compact `night_light`.

- `tools/README.md` health text names compact `night_light`.

- `jwm-tool capabilities` text lists `get_nl -> get_night_light`.

- `jwm-tool health` prints compact `magnifier` beside occupancy.

- README health text names compact `magnifier`.

- `tools/README.md` health text names compact `magnifier`.

- `jwm-tool capabilities` text lists `get_mag -> get_magnifier`.

- `jwm-tool health` prints compact `peek` beside occupancy.

- README health text names compact `peek`.

- `tools/README.md` health text names compact `peek`.

- `jwm-tool capabilities` text lists `get_pk -> get_peek`.

- `jwm-tool health` prints compact `expose` beside occupancy.

- README health text names compact `expose`.

- `tools/README.md` health text names compact `expose`.

- `jwm-tool health` prints compact `gesture` beside occupancy.

- README health text names compact `gesture`.

- `tools/README.md` health text names compact `gesture`.

- `jwm-tool capabilities` text lists `get_gest -> get_gesture`.

- `jwm-tool health` prints compact `wayland` beside occupancy.

- README health text names compact `wayland`.

- `tools/README.md` health text names compact `wayland`.

- `jwm-tool capabilities` text lists `get_wl -> get_wayland`.

- `jwm-tool health` prints compact `recording` beside occupancy.

- README health text names compact `recording`.

- `tools/README.md` health text names compact `recording`.

- `jwm-tool capabilities` text lists `get_rec -> get_recording`.

- `jwm-tool health` prints compact `audio_recording` beside occupancy.

- README health text names compact `audio_recording`.

- `tools/README.md` health text names compact `audio_recording`.

- `jwm-tool capabilities` text lists `get_arec -> get_audio_recording`.

- `jwm-tool health` prints compact `capture` beside occupancy.

- README health text names compact `capture`.

- `tools/README.md` health text names compact `capture`.

- `jwm-tool capabilities` text lists `get_cap -> get_capture`.

- `jwm-tool health` prints compact `waterlily` beside occupancy.

- README health text names compact `waterlily`.

- `tools/README.md` health text names compact `waterlily`.

- `jwm-tool capabilities` text lists `get_wly -> get_waterlily`.

- `jwm-tool health` prints compact `audio` beside occupancy.

- README health text names compact `audio`.

- `tools/README.md` health text names compact `audio`.

- `jwm-tool capabilities` text lists `get_devices -> get_audio`.

- `jwm-tool health` prints compact `wallpaper` beside occupancy.

- README health text names compact `wallpaper`.

- `tools/README.md` health text names compact `wallpaper`.

- `jwm-tool capabilities` text lists `get_wall -> get_wallpaper`.

- `jwm-tool health` prints compact `bluetooth` beside occupancy.

- README health text names compact `bluetooth`.

- `tools/README.md` health text names compact `bluetooth`.

- `jwm-tool capabilities` text lists `get_bt -> get_bluetooth`.

- `jwm-tool health` prints compact `resources` beside occupancy.

- README health text names compact `resources`.

- `tools/README.md` health text names compact `resources`.

- `jwm-tool capabilities` text lists `get_res -> get_resources`.

- `jwm-tool health` prints compact `connectivity` beside occupancy.

- README health text names compact `connectivity`.

- `tools/README.md` health text names compact `connectivity`.

- `jwm-tool capabilities` text lists `get_network -> get_connectivity`.

- `jwm-tool health` prints compact `power` beside occupancy.

- README health text names compact `power`.

- `tools/README.md` health text names compact `power`.

- `jwm-tool health` prints compact `media` beside occupancy.

- README health text names compact `media`.

- `tools/README.md` health text names compact `media`.

- `jwm-tool capabilities` text lists `get_clip -> get_clipboard`.

- `jwm-tool capabilities` text lists `get_idl -> get_idle`.

- `jwm-tool capabilities` text lists `get_notif -> get_notifications`.

- `jwm-tool capabilities` text lists `get_dnd -> get_do_not_disturb`.

- `jwm-tool capabilities` text lists `get_pair -> get_bluetooth`.

- `jwm-tool capabilities` text lists `get_conn -> get_connectivity`.

- `jwm-tool capabilities` text lists `get_power -> get_power_status`.

- `jwm-tool capabilities` text lists `get_media -> get_media_status`.

- `docs/expose.md` names compact `expose` beside health.

- `docs/tags-overview.md` names compact `tabs` beside health.

- `docs/window-switcher.md` names compact `selected` beside health.

- `docs/layout-picker.md` names compact `layout` beside health.

- `docs/launcher.md` names compact `system_ui` beside health.

- `docs/cube-effects.md` names compact `effects` beside health.

- `docs/minimized-dock.md` names compact `minimized` beside health.

- `docs/window-tabs.md` names compact `tabs` beside health.

- `docs/clipboard.md` names compact `clipboard` beside health.

- `docs/idle.md` names compact `idle` beside health.

- `docs/notifications.md` names compact `notifications` beside health.

- `docs/audio-recording.md` names compact `audio_recording` beside health.

- `docs/wallpaper.md` names compact `wallpaper` beside health.

- `docs/waterlily.md` names compact `waterlily` beside health.

- `docs/hdr.md` names compact `hdr` beside health.

- `docs/resources.md` names compact `resources` beside health.

- `docs/media-controls.md` names compact `media` beside health.

- `docs/control-center.md` names compact `system_ui` beside health.

- `docs/session-menu.md` names compact `session_lock` beside health.

- `docs/window-placement.md` names compact `closed_placement` beside health.

- `docs/performance.md` names compact `bench` beside health.

- `docs/debug-hud.md` names compact `wayland` beside health.

- `docs/output-layout.md` names compact `monitors` beside health.

- `docs/calendar.md` names compact `system_ui` beside health.

- `docs/remote-control.md` names compact `session_lock` beside health.

- `docs/startup.md` names compact `wayland` beside health.

- `docs/support-bundles.md` names compact `bench` beside health.

- `docs/ui-theme.md` names compact `blur` beside health.

- `docs/daily-drive.md` names compact `idle` beside health.

- `docs/architecture.md` names compact `tree` beside health.

- README health text names `get_lock` beside compact `session_lock`.

- README health text names `get_th` beside compact `tearing`.

- README health text names `get_xw` beside compact `xwayland`.

- README health text names `get_scroll` beside compact `scrolling`.

- README health text names `get_cm` beside compact `color_management`.

- README health text names `get_nl` beside compact `night_light`.

- README health text names `get_mag` beside compact `magnifier`.

- README health text names `get_pk` beside compact `peek`.

- README health text names `get_gest` beside compact `gesture`.

- README health text names `get_wl` beside compact `wayland`.

- README health text names `get_rec` beside compact `recording`.

- README health text names `get_arec` beside compact `audio_recording`.

- README health text names `get_cap` beside compact `capture`.

- README health text names `get_wly` beside compact `waterlily`.

- README health text names `get_devices` beside compact `audio`.

- README health text names `get_wall` beside compact `wallpaper`.

- README health text names `get_bt` beside compact `bluetooth`.

- README health text names `get_res` beside compact `resources`.

- README health text names `get_conn` beside compact `connectivity`.

- README health text names `get_clip` beside compact `clipboard`.

- README health text names `get_idl` beside compact `idle`.

- README health text names `get_notif` beside compact `notifications`.

- README health text names `get_dnd` beside compact `dnd`.

- README health text names `get_ui` beside compact `system_ui`.

- README health text names `get_lt` beside compact `layout`.

- README health text names `get_tab` beside compact `tabs`.

- README health text names `get_sel` beside compact `selected`.

- README health text names `get_strut` beside compact `struts`.

- README health text names `get_pads` beside compact `scratchpads`.

- README health text names `get_fw` beside compact `focused`.

- README health text names `get_mons` beside compact `monitors`.

- README health text names `get_ws` beside compact `workspaces`.

- README health text names `get_wins` beside compact `windows`.

- README health text names `get_fx` beside compact `effects`.

- README health text names `get_mute` beside compact `mic`.

- README health text names `get_bm` beside compact `bench`.

- Wave 628: Health compact `session_lock` is the operator twin of `get_lock`.

- Wave 629: Health compact `tearing` is the operator twin of `get_th`.

- Wave 630: Health compact `xwayland` is the operator twin of `get_xw`.

- Wave 631: Health compact `scrolling` is the operator twin of `get_scroll`.

- Wave 632: Health compact `color_management` is the operator twin of `get_cm`.

- Wave 633: Health compact `night_light` is the operator twin of `get_nl`.

- Wave 634: Health compact `magnifier` is the operator twin of `get_mag`.

- Wave 635: Health compact `peek` is the operator twin of `get_pk`.

- Wave 636: Health compact `gesture` is the operator twin of `get_gest`.

- Wave 637: Health compact `wayland` is the operator twin of `get_wl`.

- Wave 638: Health compact `recording` is the operator twin of `get_rec`.

- Wave 639: Health compact `audio_recording` is the operator twin of `get_arec`.

- Wave 640: Health compact `capture` is the operator twin of `get_cap`.

- Wave 641: Health compact `waterlily` is the operator twin of `get_wly`.

- Wave 642: Health compact `audio` is the operator twin of `get_devices`.

- Wave 643: Health compact `wallpaper` is the operator twin of `get_wall`.

- Wave 644: Health compact `bluetooth` is the operator twin of `get_bt`.

- Wave 645: Health compact `resources` is the operator twin of `get_res`.

- Wave 646: Health compact `connectivity` is the operator twin of `get_conn`.

- Wave 647: Health compact `clipboard` is the operator twin of `get_clip`.

- Wave 648: Health compact `idle` is the operator twin of `get_idl`.

- Wave 649: Health compact `notifications` is the operator twin of `get_notif`.

- Wave 650: Health compact `dnd` is the operator twin of `get_dnd`.

- Wave 651: Health compact `system_ui` is the operator twin of `get_ui`.

- Wave 652: Health compact `layout` is the operator twin of `get_lt`.

- Wave 653: Health compact `tabs` is the operator twin of `get_tab`.

- Wave 654: Health compact `selected` is the operator twin of `get_sel`.

- Wave 655: Health compact `struts` is the operator twin of `get_strut`.

- Wave 656: Health compact `scratchpads` is the operator twin of `get_pads`.

- Wave 657: Health compact `focused` is the operator twin of `get_fw`.

- Wave 658: Health compact `monitors` is the operator twin of `get_mons`.

- Wave 659: Health compact `workspaces` is the operator twin of `get_ws`.

- Wave 660: Health compact `windows` is the operator twin of `get_wins`.

- Wave 661: Health compact `effects` is the operator twin of `get_fx`.

- Wave 662: Health compact `mic` is the operator twin of `get_mute`.

- Wave 663: Health compact `bench` is the operator twin of `get_bm`.

- Wave 664: Health compact `closed_placement` is the operator twin of `get_cp`.

- Wave 665: Health compact `prev_layout` is the operator twin of `get_pl`.

- Wave 666: Health compact `cfact` is the operator twin of `get_cf`.

- Wave 667: Health compact `gaps` is the operator twin of `get_gap`.

- Wave 668: Health compact `mfact` is the operator twin of `get_mf`.

- Wave 669: Health compact `nmaster` is the operator twin of `get_nm`.

- Wave 670: Health compact `floating` is the operator twin of `get_status.floating`.

- Wave 671: Health compact `minimized` is the operator twin of `get_status.minimized`.

- Wave 672: Health compact `sticky` is the operator twin of `get_status.sticky`.

- Wave 673: Health compact `urgent` is the operator twin of `get_status.urgent`.

- Wave 674: Health compact `fullscreen` is the operator twin of `get_status.fullscreen`.

- Wave 675: Health compact `pip` is the operator twin of `get_status.pip`.

- Wave 676: Health compact `blur` is the operator twin of `get_status.blur`.

- Wave 677: Health compact `hdr` is the operator twin of `get_status.hdr`.

- Wave 678: Health compact `expose` is the operator twin of `get_status.expose`.

- Wave 679: Health compact `media` is the operator twin of `get_media`.

- Wave 680: Health compact `power` is the operator twin of `get_power`.

- Wave 681: `get_lock` and health compact `session_lock` share one Status nest.

- Wave 682: `get_th` and health compact `tearing` share one Status nest.

- Wave 683: `get_xw` and health compact `xwayland` share one Status nest.

- Wave 684: `get_scroll` and health compact `scrolling` share one Status nest.

- Wave 685: `get_cm` and health compact `color_management` share one Status nest.

- Wave 686: `get_nl` and health compact `night_light` share one Status nest.

- Wave 687: `get_mag` and health compact `magnifier` share one Status nest.

- Wave 688: `get_pk` and health compact `peek` share one Status nest.

- Wave 689: `get_gest` and health compact `gesture` share one Status nest.

- Wave 690: `get_wl` and health compact `wayland` share one Status nest.

- Wave 691: `get_rec` and health compact `recording` share one Status nest.

- Wave 692: `get_arec` and health compact `audio_recording` share one Status nest.

- Wave 693: `get_cap` and health compact `capture` share one Status nest.

- Wave 694: `get_wly` and health compact `waterlily` share one Status nest.

- Wave 695: `get_devices` and health compact `audio` share one Status nest.

- Wave 696: `get_wall` and health compact `wallpaper` share one Status nest.

- Wave 697: `get_bt` and health compact `bluetooth` share one Status nest.

- Wave 698: `get_res` and health compact `resources` share one Status nest.

- Wave 699: `get_conn` and health compact `connectivity` share one Status nest.

- Wave 700: `get_clip` and health compact `clipboard` share one Status nest.

- Wave 701: `get_idl` and health compact `idle` share one Status nest.

- Wave 702: `get_notif` and health compact `notifications` share one Status nest.

- Wave 703: `get_dnd` and health compact `dnd` share one Status nest.

- Wave 704: `get_ui` and health compact `system_ui` share one Status nest.

- Wave 705: `get_lt` and health compact `layout` share one Status nest.

- Wave 706: `get_tab` and health compact `tabs` share one Status nest.

- Wave 707: `get_sel` and health compact `selected` share one Status nest.

- Wave 708: `get_strut` and health compact `struts` share one Status nest.

- Wave 709: `get_pads` and health compact `scratchpads` share one Status nest.

- Wave 710: `get_fw` and health compact `focused` share one Status nest.

- Wave 711: `get_mons` and health compact `monitors` share one Status nest.

- Wave 712: `get_ws` and health compact `workspaces` share one Status nest.

- Wave 713: `get_wins` and health compact `windows` share one Status nest.

- Wave 714: `get_fx` and health compact `effects` share one Status nest.

- Wave 715: `get_mute` and health compact `mic` share one Status nest.

- Wave 716: `get_bm` and health compact `bench` share one Status nest.

- Wave 717: `get_cp` and health compact `closed_placement` share one Status nest.

- Wave 718: `get_pl` and health compact `prev_layout` share one Status nest.

- Wave 719: `get_cf` and health compact `cfact` share one Status nest.

- Wave 720: `get_gap` and health compact `gaps` share one Status nest.

- Wave 721: `get_mf` and health compact `mfact` share one Status nest.

- Wave 722: `get_nm` and health compact `nmaster` share one Status nest.

- Wave 723: `get_status.floating` and health compact `floating` share one Status nest.

- Wave 724: `get_status.minimized` and health compact `minimized` share one Status nest.

- Wave 725: `get_status.sticky` and health compact `sticky` share one Status nest.

- Wave 726: `get_status.urgent` and health compact `urgent` share one Status nest.

- Wave 727: `get_status.fullscreen` and health compact `fullscreen` share one Status nest.

- Wave 728: `get_status.pip` and health compact `pip` share one Status nest.

- Wave 729: `get_status.blur` and health compact `blur` share one Status nest.

- Wave 730: `get_status.hdr` and health compact `hdr` share one Status nest.

- Wave 731: `get_status.expose` and health compact `expose` share one Status nest.

- Wave 732: `get_media` and health compact `media` share one Status nest.

- Wave 733: `get_power` and health compact `power` share one Status nest.

- Wave 734: Doctor bundles include health compact `session_lock` from `get_lock`.

- Wave 735: Doctor bundles include health compact `tearing` from `get_th`.

- Wave 736: Doctor bundles include health compact `xwayland` from `get_xw`.

- Wave 737: Doctor bundles include health compact `scrolling` from `get_scroll`.

- Wave 738: Doctor bundles include health compact `color_management` from `get_cm`.

- Wave 739: Doctor bundles include health compact `night_light` from `get_nl`.

- Wave 740: Doctor bundles include health compact `magnifier` from `get_mag`.

- Wave 741: Doctor bundles include health compact `peek` from `get_pk`.

- Wave 742: Doctor bundles include health compact `gesture` from `get_gest`.

- Wave 743: Doctor bundles include health compact `wayland` from `get_wl`.

- Wave 744: Doctor bundles include health compact `recording` from `get_rec`.

- Wave 745: Doctor bundles include health compact `audio_recording` from `get_arec`.

- Wave 746: Doctor bundles include health compact `capture` from `get_cap`.

- Wave 747: Doctor bundles include health compact `waterlily` from `get_wly`.

- Wave 748: Doctor bundles include health compact `audio` from `get_devices`.

- Wave 749: Doctor bundles include health compact `wallpaper` from `get_wall`.

- Wave 750: Doctor bundles include health compact `bluetooth` from `get_bt`.

- Wave 751: Doctor bundles include health compact `resources` from `get_res`.

- Wave 752: Doctor bundles include health compact `connectivity` from `get_conn`.

- Wave 753: Doctor bundles include health compact `clipboard` from `get_clip`.

- Wave 754: Doctor bundles include health compact `idle` from `get_idl`.

- Wave 755: Doctor bundles include health compact `notifications` from `get_notif`.

- Wave 756: Doctor bundles include health compact `dnd` from `get_dnd`.

- Wave 757: Doctor bundles include health compact `system_ui` from `get_ui`.

- Wave 758: Doctor bundles include health compact `layout` from `get_lt`.

- Wave 759: Doctor bundles include health compact `tabs` from `get_tab`.

- Wave 760: Doctor bundles include health compact `selected` from `get_sel`.

- Wave 761: Doctor bundles include health compact `struts` from `get_strut`.

- Wave 762: Doctor bundles include health compact `scratchpads` from `get_pads`.

- Wave 763: Doctor bundles include health compact `focused` from `get_fw`.

- Wave 764: Doctor bundles include health compact `monitors` from `get_mons`.

- Wave 765: Doctor bundles include health compact `workspaces` from `get_ws`.

- Wave 766: Doctor bundles include health compact `windows` from `get_wins`.

- Wave 767: Doctor bundles include health compact `effects` from `get_fx`.

- Wave 768: Doctor bundles include health compact `mic` from `get_mute`.

- Wave 769: Doctor bundles include health compact `bench` from `get_bm`.

- Wave 770: Doctor bundles include health compact `closed_placement` from `get_cp`.

- Wave 771: Doctor bundles include health compact `prev_layout` from `get_pl`.

- Wave 772: Doctor bundles include health compact `cfact` from `get_cf`.

- Wave 773: Doctor bundles include health compact `gaps` from `get_gap`.

- Wave 774: Doctor bundles include health compact `mfact` from `get_mf`.

- Wave 775: Doctor bundles include health compact `nmaster` from `get_nm`.

- Wave 776: Doctor bundles include health compact `floating` from `get_status.floating`.

- Wave 777: Doctor bundles include health compact `minimized` from `get_status.minimized`.

- Wave 778: Doctor bundles include health compact `sticky` from `get_status.sticky`.

- Wave 779: Doctor bundles include health compact `urgent` from `get_status.urgent`.

- Wave 780: Doctor bundles include health compact `fullscreen` from `get_status.fullscreen`.

- Wave 781: Doctor bundles include health compact `pip` from `get_status.pip`.

- Wave 782: Doctor bundles include health compact `blur` from `get_status.blur`.

- Wave 783: Doctor bundles include health compact `hdr` from `get_status.hdr`.

- Wave 784: Doctor bundles include health compact `expose` from `get_status.expose`.

- Wave 785: Doctor bundles include health compact `media` from `get_media`.

- Wave 786: Doctor bundles include health compact `power` from `get_power`.

- Wave 787: Support triage reads health compact `session_lock` before `get_lock` dumps.

- Wave 788: Support triage reads health compact `tearing` before `get_th` dumps.

- Wave 789: Support triage reads health compact `xwayland` before `get_xw` dumps.

- Wave 790: Support triage reads health compact `scrolling` before `get_scroll` dumps.

- Wave 791: Support triage reads health compact `color_management` before `get_cm` dumps.

- Wave 792: Support triage reads health compact `night_light` before `get_nl` dumps.

- Wave 793: Support triage reads health compact `magnifier` before `get_mag` dumps.

- Wave 794: Support triage reads health compact `peek` before `get_pk` dumps.

- Wave 795: Support triage reads health compact `gesture` before `get_gest` dumps.

- Wave 796: Support triage reads health compact `wayland` before `get_wl` dumps.

- Wave 797: Support triage reads health compact `recording` before `get_rec` dumps.

- Wave 798: Support triage reads health compact `audio_recording` before `get_arec` dumps.

- Wave 799: Support triage reads health compact `capture` before `get_cap` dumps.

- Wave 800: Support triage reads health compact `waterlily` before `get_wly` dumps.

- Wave 801: Support triage reads health compact `audio` before `get_devices` dumps.

- Wave 802: Support triage reads health compact `wallpaper` before `get_wall` dumps.

- Wave 803: Support triage reads health compact `bluetooth` before `get_bt` dumps.

- Wave 804: Support triage reads health compact `resources` before `get_res` dumps.

- Wave 805: Support triage reads health compact `connectivity` before `get_conn` dumps.

- Wave 806: Support triage reads health compact `clipboard` before `get_clip` dumps.

- Wave 807: Support triage reads health compact `idle` before `get_idl` dumps.

- Wave 808: Support triage reads health compact `notifications` before `get_notif` dumps.

- Wave 809: Support triage reads health compact `dnd` before `get_dnd` dumps.

- Wave 810: Support triage reads health compact `system_ui` before `get_ui` dumps.

- Wave 811: Support triage reads health compact `layout` before `get_lt` dumps.

- Wave 812: Support triage reads health compact `tabs` before `get_tab` dumps.

- Wave 813: Support triage reads health compact `selected` before `get_sel` dumps.

- Wave 814: Support triage reads health compact `struts` before `get_strut` dumps.

- Wave 815: Support triage reads health compact `scratchpads` before `get_pads` dumps.

- Wave 816: Support triage reads health compact `focused` before `get_fw` dumps.

- Wave 817: Support triage reads health compact `monitors` before `get_mons` dumps.

- Wave 818: Support triage reads health compact `workspaces` before `get_ws` dumps.

- Wave 819: Support triage reads health compact `windows` before `get_wins` dumps.

- Wave 820: Support triage reads health compact `effects` before `get_fx` dumps.

- Wave 821: Support triage reads health compact `mic` before `get_mute` dumps.

- Wave 822: Support triage reads health compact `bench` before `get_bm` dumps.

- Wave 823: Support triage reads health compact `closed_placement` before `get_cp` dumps.

- Wave 824: Support triage reads health compact `prev_layout` before `get_pl` dumps.

- Wave 825: Support triage reads health compact `cfact` before `get_cf` dumps.

- Wave 826: Support triage reads health compact `gaps` before `get_gap` dumps.

- Wave 827: Support triage reads health compact `mfact` before `get_mf` dumps.

- Wave 828: Support triage reads health compact `nmaster` before `get_nm` dumps.

- Wave 829: Support triage reads health compact `floating` before `get_status.floating` dumps.

- Wave 830: Support triage reads health compact `minimized` before `get_status.minimized` dumps.

- Wave 831: Support triage reads health compact `sticky` before `get_status.sticky` dumps.

- Wave 832: Support triage reads health compact `urgent` before `get_status.urgent` dumps.

- Wave 833: Support triage reads health compact `fullscreen` before `get_status.fullscreen` dumps.

- Wave 834: Support triage reads health compact `pip` before `get_status.pip` dumps.

- Wave 835: Support triage reads health compact `blur` before `get_status.blur` dumps.

- Wave 836: Support triage reads health compact `hdr` before `get_status.hdr` dumps.

- Wave 837: Support triage reads health compact `expose` before `get_status.expose` dumps.

- Wave 838: Support triage reads health compact `media` before `get_media` dumps.

- Wave 839: Support triage reads health compact `power` before `get_power` dumps.

- Wave 840: Nested smoke checks health compact `session_lock` after `get_lock`.

- Wave 841: Nested smoke checks health compact `tearing` after `get_th`.

- Wave 842: Nested smoke checks health compact `xwayland` after `get_xw`.

- Wave 843: Nested smoke checks health compact `scrolling` after `get_scroll`.

- Wave 844: Nested smoke checks health compact `color_management` after `get_cm`.

- Wave 845: Nested smoke checks health compact `night_light` after `get_nl`.

- Wave 846: Nested smoke checks health compact `magnifier` after `get_mag`.

- Wave 847: Nested smoke checks health compact `peek` after `get_pk`.

- Wave 848: Nested smoke checks health compact `gesture` after `get_gest`.

- Wave 849: Nested smoke checks health compact `wayland` after `get_wl`.

- Wave 850: Nested smoke checks health compact `recording` after `get_rec`.

- Wave 851: Nested smoke checks health compact `audio_recording` after `get_arec`.

- Wave 852: Nested smoke checks health compact `capture` after `get_cap`.

- Wave 853: Nested smoke checks health compact `waterlily` after `get_wly`.

- Wave 854: Nested smoke checks health compact `audio` after `get_devices`.

- Wave 855: Nested smoke checks health compact `wallpaper` after `get_wall`.

- Wave 856: Nested smoke checks health compact `bluetooth` after `get_bt`.

- Wave 857: Nested smoke checks health compact `resources` after `get_res`.

- Wave 858: Nested smoke checks health compact `connectivity` after `get_conn`.

- Wave 859: Nested smoke checks health compact `clipboard` after `get_clip`.

- Wave 860: Nested smoke checks health compact `idle` after `get_idl`.

- Wave 861: Nested smoke checks health compact `notifications` after `get_notif`.

- Wave 862: Nested smoke checks health compact `dnd` after `get_dnd`.

- Wave 863: Nested smoke checks health compact `system_ui` after `get_ui`.

- Wave 864: Nested smoke checks health compact `layout` after `get_lt`.

- Wave 865: Nested smoke checks health compact `tabs` after `get_tab`.

- Wave 866: Nested smoke checks health compact `selected` after `get_sel`.

- Wave 867: Nested smoke checks health compact `struts` after `get_strut`.

- Wave 868: Nested smoke checks health compact `scratchpads` after `get_pads`.

- Wave 869: Nested smoke checks health compact `focused` after `get_fw`.

- Wave 870: Nested smoke checks health compact `monitors` after `get_mons`.

- Wave 871: Nested smoke checks health compact `workspaces` after `get_ws`.

- Wave 872: Nested smoke checks health compact `windows` after `get_wins`.

- Wave 873: Nested smoke checks health compact `effects` after `get_fx`.

- Wave 874: Nested smoke checks health compact `mic` after `get_mute`.

- Wave 875: Nested smoke checks health compact `bench` after `get_bm`.

- Wave 876: Nested smoke checks health compact `closed_placement` after `get_cp`.

- Wave 877: Nested smoke checks health compact `prev_layout` after `get_pl`.

- Wave 878: Nested smoke checks health compact `cfact` after `get_cf`.

- Wave 879: Nested smoke checks health compact `gaps` after `get_gap`.

- Wave 880: Nested smoke checks health compact `mfact` after `get_mf`.

- Wave 881: Nested smoke checks health compact `nmaster` after `get_nm`.

- Wave 882: Nested smoke checks health compact `floating` after `get_status.floating`.

- Wave 883: Nested smoke checks health compact `minimized` after `get_status.minimized`.

- Wave 884: Nested smoke checks health compact `sticky` after `get_status.sticky`.

### Changed

- Support queries share one two-second request/response deadline after
  connection and require complete newline-delimited responses within 4 MiB.
- Control tools bound Unix-socket connection establishment independently of
  subsequent I/O: 10 seconds for performance recording, 2 seconds for support
  queries and 5 seconds for `jwm-tool` requests. CLI request writes also have
  one 5-second deadline; subscriptions retain their event-waiting policy.
- Atomic configuration batches validate the final candidate before publishing,
  including dormant feature settings when a batch enables that feature.
- Performance fallback resolution uses the full monitor bounding box,
  independent of coordinate origin. Host-label inputs are bounded to 1 MiB;
  unusable idle intervals and counter resets produce errors rather than
  manufactured zero measurements.
- Performance IPC responses require complete newline-delimited envelopes with
  a boolean success flag and fit within 4 MiB including the newline. Request
  writes and response reads share one 10-second deadline after connection.
- Performance process samples are bounded to 64 KiB. Daemon PID/legacy-lock
  records, legacy command lines and response files use bounded nonblocking
  regular-file reads, preserving boot/start-time identity checks.
- Performance configuration labels and baseline inputs use bounded regular-file
  reads, reject FIFOs without waiting for a writer, and preserve ordinary
  symlink inputs. Failed configuration reads produce an unknown fingerprint
  that the comparison contract refuses.
- Process ancestry probes bound status input to 256 KiB and stop before invalid
  PIDs or repeated ancestors. Support-bundle system facts use 4 KiB kernel and
  64 KiB distribution input limits.
- The next ten rounds traverse only assigned workspace tag bits, accumulate
  tree flags while projecting windows, and cache stack positions and swallow
  relationships within each window/tree query.
- Scrolling overview preparation indexes column membership once instead of
  rescanning the strip for each visible client. Cursor selection follows the
  nearest nominal theme size even when different sizes share pixel dimensions.
- Notification, configuration and support-bundle writes retry occupied
  temporary names without deleting another writer's file. Temporary creation
  and configuration-backup name searches stop after 128 candidates.
- Ten follow-up rounds batch all six `get_status` window flags, accumulate
  workspace counts from one client lookup per monitor entry, and reuse
  monitor/scratchpad/tab projections across window and tree queries.
- Clipboard filtering normalizes the Unicode query once per panel rebuild.
  Incoming image offers are checked before raster decoding: at most 16,384
  pixels per edge and 16 Mi pixels overall, with a 64 MiB decoder allocation
  budget. PNG passthrough also verifies the detected format.
- Battery discovery retains only the lexically first 64 directory candidates
  while preserving battery selection and peripheral-scope behavior.
- IPC statistics use one client traversal per monitor or workspace and constant
  time scratchpad/tab lookups. `get_status` counts and window flag summaries
  read WM state directly instead of rebuilding detailed window, monitor,
  workspace and tree replies.
- Launcher, information-panel and clipboard search inputs accept up to 256
  Unicode characters. Calculator expressions have the same parsing budget.
- Configuration readers, backups and restores reject special files and payloads
  over 4 MiB. Backups publish complete synced files without overwriting earlier
  recovery points; restore uses atomic replacement, preserves dotfile symlinks
  and existing target permissions, and creates missing targets privately.

### Fixed

- The source installer recovers once from missing generated XCB sources by
  cleaning only that package's current-profile artifacts. Workspace/bridge
  artifact lookup follows `CARGO_TARGET_DIR`; native bars use independent
  caches and their dependency locks. Cargo and log-writing failures retain
  their respective exit status.
- Launcher arithmetic applies powers before unary signs, rejects non-finite
  intermediate values and preserves tiny nonzero results in scientific notation.
- EDID identity parsing preserves valid names/serials when later descriptors
  are blank and rejects invalid manufacturer letter codes. VmRSS parsing
  requires the kernel's `kB` unit and a complete field.
- Idle tracking recognizes activity when the sampled clock decreases, including
  delayed polls, and restores dim/off stages independently when live settings
  disable or postpone them. Calendar navigation stays within renderable years.
- Invalid spawn argv containing NUL and wrong numeric arguments for
  `incnmaster`, `focusstack` and `setmfact` warn and skip the affected binding.
  Existing files continue loading their other valid settings.
- Repeated scrolling admission preserves existing columns, widths and focus.
  Window boundary constraints retain wide intermediate sizes including borders,
  and visibility checks avoid mistaking overflowing visible geometry for a
  parked window.
- Notification identifier wraparound skips identifiers still in the bounded
  history. Launcher history writes accept bare relative paths and never
  remove released temporary names after a directory-sync failure.
- Desktop Exec preserves explicit empty arguments. String command parsing
  rejects internal NUL before launch; terminal probes use the Linux default
  search path when PATH is absent while honoring explicitly empty PATH.
- EDID text identity ignores descriptors with nonzero reserved prefix bytes.
  Performance comparison refuses invalid effective ratio/absolute budgets
  while preserving the unused-bound semantics of Exact rules.
- Launcher history rejects control characters in application identifiers,
  preventing new entries from injecting extra persisted rows. Notification
  history restores only the newest occurrence of each identifier.
- Monitor reference navigation counts steps among the other outputs and
  recovers from invalid reference indices without a repeat-until-match loop.
- Performance comparison refuses recorded negative or non-finite metrics
  before applying budgets, so corrupted timings cannot pass as improvements.
- Monitor picker cycles and display placement/alignment avoid integer overflow,
  preserving representable results with wide intermediate arithmetic. PNG
  metadata rejects truncated IHDR chunks and invalid dimensions.
- Session and closed-placement saves never delete a temporary path reused after
  a successful rename, including when the final directory sync fails.
- HUD CPU samples re-establish their counter baseline after resets or unusable
  intervals instead of displaying a false zero or ratio; aggregate counter
  overflow is rejected.
- Launcher and searchable information panels no longer panic on substring
  matches deep inside long titles. Every substring match ranks above a
  subsequence match, while substring position ordering remains strict.
- Notification history accepts bare relative output filenames. Failed commits
  clean up owned temporaries; directory-sync failures after a successful
  commit never clean up a released name another writer could have reused.
- Support-bundle file output now explicitly applies mode `0600` and syncs the
  destination directory after replacement.
- Session snapshots omit uninitialized restore geometry, continue reading
  historical double-zero old-geometry slots, and reject malformed dimensions.
- Session, closed-window placement and launcher usage saves retry temporary
  name collisions without deleting another writer's file. Retries are bounded
  to 128 candidates and leave existing state intact when all are occupied.
- Session and closed-placement loaders reject FIFOs without waiting for a
  writer. Native X11 tests use Xvfb's displayfd readiness notification and a
  retained X11 setup connection instead of a socket-existence timing guess.
- Session restore retains parking coordinates calculated for the current output
  topology and clamps minimized/fullscreen restore rectangles to the current
  monitor work area, preventing stale saved geometry from placing windows off
  screen.
- Updated the switcher integration test to exercise horizontal-wheel wraparound
  and made the control-snapshot test tolerate formatted command aliases.
- Corrected the XWM integration fixture to provide Smithay's required XWayland
  client data; audio-device OSD coverage now executes the flush and verifies
  that confirmed feedback is consumed once.
- Cleared existing workspace formatting drift and restricted test-only monitor
  helpers to test builds so the default Clippy warning gate is clean.

### Added

- `get_monitors` / `get_workspaces` / `get_tree` report swallowed / on_view /
  maximize_promoted / strut / status_bar counts.
- IPC query aliases: `get_pk`, `get_bm`, `get_conf`, `get_rec`, `get_arec`,
  `get_cap`, `get_xw`, `get_wly`, `get_idl`.
- IPC command aliases: `kill`, `last`, `loop`, `save`, `restore`, `pad`,
  `ftab`, `fwin`, `case`, `palette`, `region`, `attach`, `scol`, `smov`,
  `swin`, `scons`, `sexp`, `twifi`, `tbt`, `clayout`.
- `get_status` nests compact `capabilities` / `selected` / `bench` /
  `floating` / `minimized` / `sticky` / `urgent` / `fullscreen` / `pip`.
- `get_config` polish7: animation_speed/easing/duration, backend_family,
  buttons/chord/termcmd lengths, color accents, behavior feature mirrors.
- Session snapshot v17 persists optional `hidden_x`.
- Clipboard + notification center: docs advertise vertical or horizontal wheel.
- Portal `WindowInfo` deserializes maximized_vert/horz; `MonitorInfo` gains
  swallowed/on_view/maximize_promoted/strut/status_bar counts.

- `get_monitors` / `get_workspaces` / `get_tree` report skip_taskbar /
  skip_pager / no_decorations / drag_float counts.
- IPC query aliases: `get_lt`, `get_cf`, `get_sel`, `get_fw`, `get_pl`,
  `get_bar`, `get_tr`, `get_win`, `get_conn`, `get_st`, `get_fx`,
  `get_mute`, `get_cli`, `get_wc`, `get_th`, `get_pair`.
- IPC command aliases: `cal`, `clip`, `monlayout`, `aout`, `ain`, `unlock`,
  `snap`, `record`, `arecord`, `bar`, `comp`, `play`, `next`, `prev`,
  `stop`, `unfocus`, `damage`, `cycle`.
- `get_status` nests compact `monitors` / `workspaces` / `windows` / `tree` /
  `focused` / `cfact` / `prev_layout` / `effects` / `mic`.
- `get_config` polish6: status_bar_* / cursor / drag_threshold /
  client_moveresize / new_client_position / animation / key/rules counts /
  layout_tags / compositor_enabled / tagmask.
- Session snapshot v16 persists optional `old_geometry`.
- Calendar: horizontal wheel (and Shift+horizontal) twins vertical month/year.
- Portal `WindowInfo` deserializes status_bar / swallowed / maximize_promoted;
  `MonitorInfo` gains skip_taskbar/skip_pager/no_decorations/drag_float counts.

- `get_monitors` / `get_workspaces` / `get_tree` report dock / desktop /
  never_focus / demands_attention counts.
- IPC query aliases: `get_bt`, `get_wl`, `get_nl`, `get_cm`, `get_sess`,
  `get_strut`, `get_scratch`, `get_mons`, `get_ws`, `get_gap`, `get_nm`,
  `get_mf`, `get_tab`, `get_bench`, `get_gest`, `get_wall`.
- IPC command aliases: `hub`, `switcher`, `tags`, `overview`, `peek`, `mag`,
  `annotate`, `lily`, `night`, `caffeine`, `wifi`, `bt`, `wall`, `session`,
  `floating`, `sticky`, `pip`, `maximize`.
- `get_status` nests compact `tabs` / `struts` / `scratchpads` / `gaps` /
  `mfact` / `nmaster` / `show_bar` / `metrics` / `version_info`.
- `get_config` polish5: rule-list counts, swallow lists, blur-by-hz/monitor,
  appearance `ui_theme` / `border_px` / `gap_px` / `snap`.
- Session snapshot v15 persists optional `hidden_restore`.
- System UI panels (launcher, Hub, pickers, …): horizontal wheel browses
  like the vertical wheel.
- Portal `WindowInfo` deserializes dock/desktop/strut/fixed/skip_*/decor/
  drag_float; `MonitorInfo` gains dock/desktop/never_focus/demands_attention
  counts.

- `get_monitors` / `get_workspaces` report urgent / fullscreen / pip /
  maximized / above / below / fixed / scratchpad / tabbed counts (tree /
  workspace symmetry).
- IPC query aliases: `get_caps`, `get_pads`, `get_mag`, `get_perf`,
  `get_res`, `get_wins`, `get_devices`, `get_cfg`, `get_ver`.
- IPC command aliases: `launcher`, `notif_center`, `screenshot`,
  `screenshot_fullscreen`, `lock`, `layouts`, `load_session`,
  `toggle_do_not_disturb`.
- `get_status` nests compact `tearing` / `xwayland` / `scrolling` /
  `color_management` / `audio` / `wallpaper` / `bluetooth` / `system_ui` /
  `layout`.
- `get_config` polish4 scalars: color grading, tilt details, particles,
  wayland_enable_*, present/audio sync, wallpaper_colors, …
- Session snapshot v14 persists optional `minimized_order`.
- Layout picker horizontal wheel browses like Left/Right.
- Portal `WindowInfo` deserializes below/scratchpad/tabbed/never_focus/
  demands_attention/client_fact; `MonitorInfo` gains count family +
  scale/refresh.

- `get_windows` / `get_tree` / `window/state` report optional `stack_index`.
- `get_workspaces` / `get_tree` report pip / maximized / above / below /
  fixed counts; tree nodes also report scratchpad / tabbed counts.
- `get_monitors` / `get_tree` report per-monitor `window_count` /
  `floating_count` / `minimized_count` / `sticky_count`.
- IPC query aliases: `get_notif`, `get_ui`, `get_lock`, `get_xwayland`,
  `get_tearing`, `get_scrolling`, `get_effects`, `get_tabs`, `get_network`,
  `get_clip`, `get_do_not_disturb`.
- IPC command aliases: `minimize_window`, `zoom_master`, `exit`,
  `reload_wm`, `persist_session`.
- `get_status` nests compact `waterlily` / `night_light` / `magnifier` /
  `peek` / `expose` / `gesture` / `wayland` / `dnd` / `session_lock`.
- `get_effect_status` / `get_effects` mirrors shell picker / lock /
  debug_hud feature flags.
- `get_config` polish2 keys: border colors/gradient, edge glow, tilt,
  ripple, wallpaper crossfade/dir, wobbly details, gesture_swipe,
  wayland_enable_*, power/idle commands, annotation colors, …
- Session snapshot v13 persists optional `old_border_w`.
- Window switcher Left/Right + horizontal wheel browse; overview cube
  horizontal wheel cycles faces.
- Portal `WindowInfo` deserializes sticky/pip/maximized/above/border/
  total/stack_index; `MonitorInfo` gains window counts.

- `get_windows` / `get_clients` / `get_tree` / `window/state` report
  border-inclusive `total_w` / `total_h`.
- `get_monitors` / `get_tree` report `lt_symbol` and optional `output_id`.
- `get_workspaces` report per-tag `urgent_count` / `fullscreen_count`;
  `get_tree` nodes also carry `minimized_count` / `sticky_count` /
  `fullscreen_count`.
- IPC aliases: `toggle_scratchpad`; short query twins (`get_idle`,
  `get_recording`, `get_audio_recording`, `get_blur`, `get_hdr`,
  `get_capture`, `get_power`, `get_media`, `get_gesture`, `get_wayland`,
  `get_waterlily`, `get_bluetooth`, `get_mic`, `get_wallpaper`,
  `get_audio`, `get_color_management`).
- `get_status` feature flags include `monitor_lock` / `debug_hud`; nests
  compact `idle` / `recording` / `audio_recording` / `clipboard` summaries.
- `get_resources` memory reports `available_kib`; `get_connectivity`
  reports `scanning` on network / bluetooth.
- `get_config` keys: clipboard_history, border_glow_*, shadow extras,
  genie/focus durations, snap_preview_color, new_client_position,
  drag_threshold_px, client_moveresize, resize_hints, lock_fullscreen,
  compositor_api, resource_rows, gesture_swipe_threshold,
  wayland_enable_tearing_control, window_animation*, attention_animation.
- Session snapshot v12 persists `never_focus` / `old_state` /
  `pip_restore_sticky` / `remembers_closed_placement`.
- Per-tag layout persistence saves / restores `show_bar`; `togglebar`
  marks layout dirty.
- Expose horizontal wheel browses Left/Right (tags-overview twin).
- Portal `WindowInfo` deserializes floating/fullscreen/minimized/urgent /
  monitor_name / layout; `MonitorInfo` gains monitor_name /
  output_connector / lt_symbol / output_id.

- `get_windows` / `get_clients` / `get_tree` / `window/state` report optional
  `hidden_x`, optional `sync_counter`, and `sync_value`.
- `get_monitors` / `get_tree` report dual-tagset `sel_tags` /
  `previous_tags`, pertag `cur_tag` / `prev_tag`, and optional
  `output_connector` (raw connector name).
- `get_workspaces` report per-tag `minimized_count` / `floating_count` /
  `sticky_count`.
- IPC aliases: `get_cfact`, `get_show_bar`, `get_prev_layout`, `get_selected`,
  `get_focused_window`; `set_cfact`; underscore twins
  (`toggle_floating` / `toggle_sticky` / `toggle_pip` / `toggle_maximize` /
  `toggle_bar` / `toggle_compositor` / `toggle_partial_damage` /
  `toggle_tag` / `toggle_view` / `kill_client` / `focus_stack` /
  `move_stack` / `focus_mon` / `tag_mon` / `cycle_layout` / `last_layout` /
  `inc_nmaster` / `loop_view`).
- `get_status` feature flags include shell pickers (`control_center`,
  `clipboard_picker`, `wifi_picker`, `bluetooth_picker`,
  `wallpaper_picker`, `theme_picker`, `audio_*_picker`, `media_players`,
  `window_switcher`) and `session_lock`; nests compact `resources` /
  `connectivity` / `power` / `media` / `notifications` / `blur` / `hdr` /
  `capture` summaries.
- `get_effect_status` mirrors launcher / session_menu / notifications flags.
- `get_config` keys: expose/peek/tags_overview/layout_picker, magnifier trio,
  window_tabs / tab_bar_height, VRR trio, compositor, swallow, idle lock /
  screen-off, genie/focus_highlight/snap_preview, blur_strength /
  shadow_radius, opacities, wallpaper / wallpaper_mode, persist_tags,
  status-bar chrome, cursor theme/size, system_ui_font, recording bitrate /
  max_height / output_dir.
- Tags overview: middle-click confirms like Enter; horizontal wheel browses
  Left/Right. See [docs/tags-overview.md](docs/tags-overview.md).

- Session snapshot v11 persists `is_urgent`, `demands_attention`,
  `skip_taskbar` / `skip_pager`, `is_fixed`, and optional `border_w` (older
  snapshots keep defaults / leave border alone).
- Portal IPC `WindowInfo` deserializes optional `monitor`, `connector`, and
  `is_on_view` (serde defaults) for picker enrichment.
- `get_config` keys: `hdr_enabled`, `idle_dim_secs` / `idle_dim_level`,
  `night_light` / `night_light_temp` / `night_light_start` /
  `night_light_end` / `night_light_transition_mins`,
  `remember_closed_placement`, plus WaterLily env mirrors
  `waterlily_enabled` / `waterlily_opacity`.
- `get_effect_status` reports `magnifier_radius` and `compositor_active`;
  `get_magnifier` reports `radius`; `get_peek` reports `compositor_active`.
- `get_tree` nodes report `urgent_count` / `floating_count`; `get_tab_bar`
  reports `selected_id` when a tab group is focused.
- Calendar: `End` jumps to today (with `Home` / `t`); `Shift`+wheel steps
  years. See [docs/calendar.md](docs/calendar.md).
- Clipboard history documents that HEIC/HEIF/JXL offers are skipped (no
  decoder in the bundled `image` crate). See
  [docs/clipboard.md](docs/clipboard.md).

- IPC aliases: `get_mfact`; `set_mfact` (= `setmfact`), `set_gaps` (= `setgaps`);
  `get_outputs` (= `get_monitors`), `get_tags` / `get_desktops` (= `get_workspaces`);
  subscribe topic `workspace` (= `tag`).
- `get_night_light` / `get_night_light_status` report `active`, `override`, and
  configured `temp`.
- `get_scratchpads` maps scratchpad name → window id; `get_struts` reports
  per-monitor reservations plus contributing window ids; `get_window` filters
  `get_windows` by `id`.
- `get_idle_status` / `idle/state` split inhibit sources as `manual_inhibit`,
  `client_inhibit`, and `recording_inhibit` (aggregate `inhibited` kept;
  `caffeine` aliases manual). See [docs/idle.md](docs/idle.md).
- `get_status` feature flags include `launcher`, `session_menu`,
  `notifications`, `waterlily`, `night_light`, and `idle_inhibit`.

- `get_windows` / `get_clients` / `get_tree` / `window/state` report
  `old_border_w`, optional `hidden_restore` / `maximize_restore_anchor`,
  `pip_restore_sticky`, `old_state`, `remembers_closed_placement`, dock
  exclusive-zone / anchors, and `is_status_bar`.
- `get_monitors` / `get_tree` report `vrr_min_hz` / `vrr_max_hz`,
  `prev_layout`, `show_bar`, strut reservations, and `selected_id`.
- `get_workspaces` reports per-tag `show_bar`, `prev_layout`, and
  `selected_id`.

- `get_windows` / `get_clients` / `get_tree` / `window/state` report resting
  `float_rect` and `old_geometry` beside live `x`/`y`/`w`/`h`.
- `get_monitors` / `get_tree` report optional `hdr_metadata`, physical size
  (`physical_width_mm` / `physical_height_mm`), and preferred mode fields.
- `get_tree` nodes carry `selected_id` and `window_count`.
- IPC aliases: `get_clients` (= `get_windows`), `set_layout` (= `setlayout`),
  `setnmaster` / `set_nmaster` (absolute master count).
- `get_config` accepts optional `keys` to return a field subset; also reports
  `overview_enabled` and `modkey`.
- Overview cube: `Home`/`End` jump, `Page Up`/`Down` page, vertical wheel
  cycles faces. See [docs/cube-effects.md](docs/cube-effects.md).
- Window switcher icons fall back through `WM_CLASS` instance. See
  [docs/window-switcher.md](docs/window-switcher.md).
- `get_audio_recording_status` reports `output_bytes`.
- `get_status` feature flags include `layout_picker`, `tags_overview`,
  `calendar`, `keybindings`, and `monitor_layout`.
- Oversized maximize restore rects shrink to fit the target work area on
  cross-output migration.

- `get_recording_status` reports top-level `elapsed_secs`, `capture_target`,
  and `last_error` beside the existing nested `capture` block and segment
  fields.
- `get_idle_status` / `idle/state` report `dim_level`, `idle_for`, and
  `secs_until_dim` / `secs_until_lock` / `secs_until_screen_off`. See
  [docs/idle.md](docs/idle.md).
- Clipboard history decodes TIFF/AVIF offers into PNG under the 4 MiB image
  cap (after PNG, JPEG, WebP/GIF, BMP). See
  [docs/clipboard.md](docs/clipboard.md).
- Portal `pick_outputs` / restore tokens honor DRM connectors (enriched from
  `get_monitors`); `JWM_PORTAL_WINDOW=pid:<n>` matches via IPC pid. See
  portal picker docs.
- `get_effect_status` reports recording / selecting / layout_picker /
  tags_overview / calendar / keybindings / monitor_layout flags.
- `get_waterlily_status` reports `active_case` / `active_palette` while the
  layer is on screen. See [docs/waterlily.md](docs/waterlily.md).
- `get_windows` / `get_tree` / `window/state` report optional `size_hints`
  when ICCCM / xdg hints are valid.

- `get_windows` / `get_tree` / `window/state` report `maximize_promoted`,
  optional `maximize_restore` `{x,y,w,h}`, `minimized_order`, optional
  `swallowing` / `swallowed_by` / `transient_for`, and `is_tabbed` /
  optional `tab_index`. See
  [docs/window-placement.md](docs/window-placement.md).

- Layout picker answers `Home`/`End`/`Page Up`/`Page Down` and middle-click
  confirms like Enter. Tags overview vertical wheel browses; calendar wheel
  steps months; MonitorLayout Home/End/Page jump the target and middle-click
  applies; keybindings Info wheel pages like PgUp/PgDn. See
  [docs/layout-picker.md](docs/layout-picker.md),
  [docs/tags-overview.md](docs/tags-overview.md),
  [docs/calendar.md](docs/calendar.md).

- Session snapshots are version 10: `client_fact`, hand-float
  (`is_drag_floating`), and `no_decorations` persist across restart;
  maximize restore re-applies via `adopt_client_maximized` using the
  saved `promoted` bit. See
  [docs/window-placement.md](docs/window-placement.md).

- `get_monitors` / `get_tree` report optional `name` (wl_output), `vendor` /
  `product_code` / `serial_number` / `monitor_serial`, and
  `vrr_supported` / `vrr_enabled`. `get_workspaces` rows add `is_occupied`
  and `has_fullscreen`. See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- `get_system_ui` reports whether a shell panel is open and its `kind`
  (`launcher`, `notification_center`, `keybindings`, …).
  `get_notifications` adds `center_open` and `selected_id`.
  `get_tab_bar` / `MonitorInfoIpc.tab_bar_reserved` expose the window tab
  strip reservation and focused-monitor membership.

- Keybinding viewer synthetic tag chords follow config `modkey` (not
  hardcoded `Mod1`); configured leader-chord bindings appear as
  `{leader} then {key}` rows. See README `Alt+Shift+/`.

- IPC `setgaps` (parity with the keybinding / `incnmaster` / `setmfact`).

- `get_monitors` / `get_tree` report live `m_fact`, `n_master`, and
  `transform` (`wl_output` 0..=7; live on wayland-udev, else `0`). See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- `get_recording_status` reports `segments`, `segment_count`, and
  `pending_output_path`. `get_effect_status` reports `expose`.

- `get_workspaces` / `get_monitors` / `get_tree` report tiling `gap`
  (pixels). Focused-monitor queries `get_layout` (layout + `m_fact` +
  `n_master` + `gap`), `get_gaps`, and `get_nmaster` return the selected
  monitor's live values. See [docs/monitor-lock.md](docs/monitor-lock.md).

- `get_windows` / `get_tree` / `window/state` report `is_fixed`, `is_dock`,
  `is_desktop`, `is_drag_floating`, `never_focus`, `skip_taskbar`,
  `skip_pager`, `no_decorations`, `demands_attention`, `has_strut`, and
  `client_fact` beside the existing state flags. See
  [docs/window-placement.md](docs/window-placement.md).

- `get_monitors` / `get_tree` report `scale` (fractional), `refresh_mhz`
  (mode refresh in millihertz; `60000` is 60 Hz), and `hdr_capable`. See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- The Alt+Tab switcher answers `Page Up` / `Page Down` (one visible page,
  no wrap). Tags overview and expose answer `Home` / `End` (first / last
  cell) and `Page Up` / `Page Down` (one grid row), without committing. See
  [docs/window-switcher.md](docs/window-switcher.md),
  [docs/tags-overview.md](docs/tags-overview.md), and
  [docs/expose.md](docs/expose.md).

- `get_magnifier` / `get_peek` query the magnifier (enabled + zoom) and peek
  overlay; `get_effect_status` also carries `magnifier_zoom`.

- `get_waterlily_status` reports the last delivered `requested_case` /
  `requested_palette` (verbatim, including `next` / `auto`). See
  [docs/waterlily.md](docs/waterlily.md).

- `get_windows` / `get_tree` / `window/state` report `is_scratchpad`,
  `border_w`, optional `scratchpad` (binding name), and optional `layout`
  (the monitor's current layout symbol). Terminal swallow / unswallow
  broadcasts `window/state` when `is_swallowed` flips. See
  [docs/window-placement.md](docs/window-placement.md).

- The Alt+Tab switcher answers `Home` / `End` (newest / oldest row). Expose's
  vertical wheel steps the highlight Up / Down without committing; a
  horizontal scroll stays inert. See
  [docs/window-switcher.md](docs/window-switcher.md) and
  [docs/expose.md](docs/expose.md).

- Calendar `Page Up` / `Page Down` step a year (twin of `Up` / `Down`). See
  [docs/calendar.md](docs/calendar.md).

- Clipboard history decodes WebP/GIF offers into PNG under the 4 MiB image
  cap (after PNG, JPEG, then WebP/GIF, then BMP). See
  [docs/clipboard.md](docs/clipboard.md).

- `get_windows` / `get_tree` / `window/state` report `is_swallowed` (terminal
  swallowed by a child) and `is_on_view` (tags intersect the monitor's active
  tags, or sticky — on the current view, not merely mapped). See
  [docs/window-placement.md](docs/window-placement.md).

- `get_monitors` / `get_tree` expose the work area as `wx` / `wy` / `ww` / `wh`
  (bar, struts, and tab bar excluded), beside the full-output `x` / `y` / `w` /
  `h`. See [docs/monitor-lock.md](docs/monitor-lock.md).

- Session snapshots (v9+) persist fullscreen and picture-in-picture;
  `restore_session` re-applies them through `setfullscreen` /
  `set_client_pip` (Fullscreen wins if both). See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- `get_workspaces` reports `is_urgent` per tag (same urgent mask as the
  status bar). See [docs/monitor-lock.md](docs/monitor-lock.md).

- Application launcher middle-click activates the pointed row through the
  same Enter path as left-click. See [docs/launcher.md](docs/launcher.md).

- Session snapshots (v8+) persist minimized; `restore_session` re-applies it
  through `set_client_minimized`. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- `get_windows` / `get_tree` / `window/state` and `get_workspaces` expose an
  optional `monitor_name` (EDID name) beside `connector`, omitted when
  unknown. See [docs/monitor-lock.md](docs/monitor-lock.md) and
  [docs/window-placement.md](docs/window-placement.md).

- Portal `JWM_PORTAL_WINDOW` matching consults jwm IPC `get_windows` when the
  Wayland foreign-toplevel `app_id` / title miss, so `class:firefox` still
  resolves; Wayland-only matching remains the fallback.

- Clipboard history decodes JPEG/BMP offers into PNG under the 4 MiB image
  cap (PNG still preferred when advertised). See
  [docs/clipboard.md](docs/clipboard.md).

- Theme, Wallpaper, Audio device, and Players pickers, plus the Session menu,
  middle-click the pointed row (or Wallpaper preview) and apply through the
  same Enter path as left-click; Session keeps its two-press confirm. See
  [docs/control-center.md](docs/control-center.md),
  [docs/wallpaper.md](docs/wallpaper.md),
  [docs/ui-theme.md](docs/ui-theme.md),
  [docs/media-controls.md](docs/media-controls.md), and
  [docs/session-menu.md](docs/session-menu.md).

- V-stack `focusstack` and scrolling column focus rearrange broadcast
  `window/state` for every visible client on the monitor after geometries
  move (focus flips alone already covered the selection). See
  [docs/window-placement.md](docs/window-placement.md).

- `get_monitors` / `get_tree` expose an optional `monitor_name` (EDID name)
  beside `connector`, omitted when unknown. See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- Hub Shell routes, Audio Output, Session…, Lock Screen, Lock This Monitor,
  and Unlock Monitor middle-click select the pointed row and activate through
  the same Enter path as left-click. Brightness and read-only System rows
  stay inert. See [docs/control-center.md](docs/control-center.md).

- `get_workspaces` exposes an optional `connector` field on each workspace
  (the same live output identity as `MonitorInfoIpc` / `WindowInfo`), omitted
  when unknown. See [docs/monitor-lock.md](docs/monitor-lock.md).

- Hub Power Profile middle-click selects the pointed row and advances one
  notch through the same Enter / OSD path as left-click. Hub Media
  middle-click selects the pointed row and pins the next player through
  the same `p` path. See [docs/control-center.md](docs/control-center.md).

- Session snapshots (v7+) persist Above / Below (`_NET_WM_STATE_ABOVE` /
  `_BELOW`); `restore_session` re-applies them through
  `apply_external_stacking_request` (Above wins if both). See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- Hub Do Not Disturb, Caffeine, and Night Light middle-click select the
  pointed row and toggle through the same Enter / OSD path as left-click.
  See [docs/control-center.md](docs/control-center.md).

- Session snapshots (v6+) persist sticky (`_NET_WM_STATE_STICKY`);
  `restore_session` re-applies it through `set_client_sticky`, and manage
  adopts a pre-map Sticky atom like Above / Below / maximize. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- `get_windows` / `get_tree` / `window/state` expose an optional `connector`
  field on each window (the same live output identity as `MonitorInfoIpc`),
  omitted when unknown. See
  [docs/window-placement.md](docs/window-placement.md).

- `get_monitors` / `get_tree` expose an optional `connector` field
  (`OutputIdentity.stable_key` / connector name) so status bars and scripts
  can key panels the same way session / closed-placement / per-tag layouts
  do. Omitted when the live output map has no identity. See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- Per-tag layout entries (`[[layout.tags]]`) key the output by optional
  `connector` (`OutputIdentity.stable_key` / connector name) alongside the
  numeric `monitor` index, so hotplug hole-fill renumbering restores each
  panel's layouts across a restart. Older files without `connector` still
  load and fall back to `monitor` (`-1` remains the any-monitor wildcard).
  See [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- Session snapshots (v5+) key client placement and per-monitor tile order on
  the output's connector / `stable_key`, not bare `monitor_num`, so hotplug
  hole-fill renumbering restores windows to the same panel across a restart.
  v4 and older `session.json` files without a connector still load and fall
  back to `monitor_num`. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- Closed-placement memory keys reopen on the output's connector /
  `stable_key` (schema v2), not bare `monitor_num`, so hotplug hole-fill
  renumbering after wave 33 persist no longer restores to the wrong
  monitor across a restart. v1 `closed_placement.json` files without a
  connector still load and fall back to `monitor_num`. See
  [docs/window-placement.md](docs/window-placement.md).

- Wayland closed-placement attribution reads real PIDs: xdg/layer surfaces
  via the client's socket credentials, XWayland via `_NET_WM_PID`. See
  [docs/window-placement.md](docs/window-placement.md).

- Closed-placement memory survives a JWM restart: up to 256 class/instance →
  (monitor, tags) entries are written atomically to
  `closed_placement.json` beside `session.json` under the XDG state
  directory. Disabling `behavior.remember_closed_placement` clears memory
  and deletes the file. The JWM launch registry is not persisted. See
  [docs/window-placement.md](docs/window-placement.md).

- Seamless X11 restart keeps a visible hand-floated window's layout membership
  in `_JWM_FLOATING_V1` so `togglefloating` survives exec at the same
  rectangle. The property is cleared when the window is tiled, maximized,
  fullscreen, PiP or drag-floating. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- Seamless X11 restart keeps a visible maximized window's pre-maximize
  rectangle in `_JWM_MAXIMIZE_RESTORE_V1` so unmaximize after exec returns to
  the same slot instead of the centered fallback. The property's promoted
  flag re-admits a tiling-layout `togglemaximize` across the restart; a plain
  tiled maximize without that flag is still refused. Session snapshots (v4+)
  also persist maximize axes, restore rectangle and promote flag, and
  re-apply them after resting placement. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- IPC `window/state` event: an accepted maximize / unmaximize / drag-cancel
  reinstate, fullscreen enter/leave, minimize/unminimize, `togglefloating`,
  urgency (`is_urgent`), sticky (`is_sticky`), a real `tag` / `toggletag`
  change, a cross-monitor `sendmon` / `tagmon`, an Above/Below stacking
  flip, PiP enter/leave, a real title / `WM_CLASS` change, a real focus
  change, a half/quarter float snap, a free float move/resize that changes
  geometry, a tiling reorder (`zoom` / `movestack` / scrolling column move
  or resize), a layout-parameter change (`incnmaster` / `setmfact` /
  `setgaps` / `setlayout`·cycle·last), a tag `view` / `toggleview`
  that rearranges visible clients, a scratchpad hide/reveal, or a
  `togglebar` / `setcfact` rearrange, a maximize work-area refit, or a
  strut / output topology rearrange pushes a `WindowInfo`-shaped payload
  so subscribers of `window` need not poll `get_windows`. Refused maximize
  requests and no-op mode changes stay silent. See
  [docs/window-placement.md](docs/window-placement.md#who-may-maximize-what).

- `get_windows` / `get_tree` / `window/state` report `pid` when the backend
  knows the client process id.

- `get_windows` / `get_tree` / `window/state` report `is_above` and
  `is_below` for `_NET_WM_STATE_ABOVE` / `BELOW`.

- Maximize is a real window state on every backend. Native X11
  `_NET_WM_STATE` (per axis; a message naming both atoms is one request),
  xdg-shell, XWayland and wlr-foreign-toplevel requests go through one shared
  transaction that fills the monitor's work area (the bar, docks and the tab
  bar excluded, the border inside it, no gaps), keeps the pre-maximize
  rectangle in a restore slot of its own, rolls back completely when a
  property or geometry write fails, and publishes the accepted state back to
  the client. Maximized windows follow work-area changes (struts, docks, the
  bar, the tab bar, output resize) and stay maximized when they move to
  another monitor or lose their output. New bindable and IPC command
  `togglemaximize`, which also takes a tiled window out of the layout while
  it is maximized; toggling again puts it back into the slot it left, so the
  master stays master. `get_windows`/`get_tree` report `is_maximized` (both
  axes), `is_maximized_vert` and `is_maximized_horz`. See
  [docs/window-placement.md](docs/window-placement.md#maximize) and
  [docs/compatibility.md](docs/compatibility.md).

- Per-monitor lock: `lock_monitor` (`Alt+Ctrl+Shift+Escape`) puts one output
  behind an opaque compositor shade — above its clients, its status bar and
  every overlay, and inside screenshots, recordings and the remote viewer —
  while the rest of the desktop keeps working. Focus, the pointer, the
  `Alt+Tab` switcher, the launcher's window search and expose all leave the
  locked monitor alone; the shade comes off through the same PAM password as
  the session lock, on a card drawn on that monitor (`unlock_monitor`, or the
  same key again). At least one monitor always stays unlocked and a
  compositor is required, so a shade is never invisible. The control center
  carries **Lock This Monitor** and **Unlock Monitor N…** rows for sessions
  driven without a pointer, and existing key lists gain the chord through the
  same back-fill the audio recorder and tags overview use. The key is a
  toggle: over a shade it means the monitor under the pointer — the one place
  the pointer and the selection can disagree, because the selection is never
  on a locked monitor — so pressing it there asks for the password instead of
  locking something else. New `monitor/lock`
  IPC event and a `locked` flag on `get_monitors`/`get_tree`. See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- Closed-placement memory: a regular window's monitor and tags are
  remembered under its `WM_CLASS` when it closes, and the next window with
  that identity that JWM did not launch itself (opened from a terminal, by an
  agent inside one, or by a running application) returns there. A window
  that lands on another monitor or tag than the focused one never steals
  focus or switches the view; its tag is marked urgent and opens on it.
  Keybinding, launcher, scratchpad and shell-panel launches keep the
  pointer's monitor and tag; `[[rules]]` still win. New
  `behavior.remember_closed_placement` (default `true`). See
  [docs/window-placement.md](docs/window-placement.md).

- Tab bar frosted glass is common-linear-aware: backdrop capture follows the
  bound target domain and tint/rim decode via `u_scene_linear`, so tab-only
  frames no longer force the exact-sRGB HDR fallback.

- Particles are common-linear-aware (`u_scene_linear`), matching EdgeGlow, so
  particle-only frames no longer force the exact-sRGB HDR fallback.

- Idle-dimmed Wayland screenshots/recordings: dedicated capture views bake
  brightness at encode time; EncodedOutput screenshot readback runs after the
  final brightness pass. See [docs/idle.md](docs/idle.md).

- Documented the framebuffer-envelope refuse path and workarounds in
  [docs/output-layout.md](docs/output-layout.md).

- Edge glow is common-linear-aware (`u_scene_linear`) so glow-only frames no
  longer force the exact-sRGB HDR fallback. See
  [docs/sota-gap-queue.md](docs/sota-gap-queue.md).

- Idle dim is a final fullscreen brightness multiply after toast/OSD/system UI
  on X11 and Wayland, so compositor chrome dims with the desktop. See
  [docs/idle.md](docs/idle.md).

- SOTA daily-drive positioning: `wayland-udev` is the primary production
  backend and the CLI/`JWM_BACKEND` default when compiled in; X11 remains a
  first-class compatibility surface. See [README](README.md),
  [docs/architecture.md](docs/architecture.md),
  [docs/hardware-validation.md](docs/hardware-validation.md),
  [docs/daily-drive.md](docs/daily-drive.md), and
  [docs/sota-gap-queue.md](docs/sota-gap-queue.md).

- XWayland interactive move/resize: `XwmHandler` move/resize requests feed
  the shared `_NET_WM_MOVERESIZE` drag pipeline.

- Official status bar designation (`tao_glow_bar`) plus packaging sketches
  under [bars/README.md](bars/README.md) and [packaging/README.md](packaging/README.md).

- Native X11 clipboard contract tests self-host an isolated Xvfb and run in
  the default `cargo test` suite (no longer `#[ignore]` / separate CI step).
  `xcb` `Clipboard::start` takes an optional display name like the x11rb
  path. See [docs/clipboard.md](docs/clipboard.md).

- IPC `set_audio_device` queues on the controls worker (picker path); the
  named OSD and `audio/devices` publish follow the verifying re-read via
  `adopt_audio_switch`. See [docs/control-center.md](docs/control-center.md).

- X11 `compositor_frame_deadline` joins toast and OSD envelope boundaries
  (with recording), matching Wayland `next_wakeup` overlay terms; the 20 ms
  composited idle cadence remains a safety net.

- Layout picker right-click cancels and restores the origin layout (pointer
  twin of `Esc`); middle-click stays inert. See
  [docs/layout-picker.md](docs/layout-picker.md).

- Keybinding viewer takes a pointer grab; click outside dismisses like
  `Esc`. See [README.md](README.md).

- Wallpaper side-preview click applies the highlighted candidate (pointer
  twin of `Enter`). See [docs/wallpaper.md](docs/wallpaper.md).

- Interactive screenshot and recording middle-click cycles the capture
  target (pointer twin of `Tab`); right-click still cancels. See
  [README.md](README.md).

- Calendar clock-line click returns to today (pointer twin of `t` /
  `Home`). See [docs/calendar.md](docs/calendar.md).

- Control-center Network and Bluetooth middle-click toggle the radio
  (same path as `Left`/`Right`; Bluetooth power-off still arms). See
  [docs/control-center.md](docs/control-center.md).

- Control-center Input middle-click mutes the microphone on the pointed row
  (same path as `m`); Volume middle-click mute is unchanged. Other Hub rows
  and blank stay inert. See
  [docs/control-center.md](docs/control-center.md).

- Calendar weekday-header sides step the year (left `Mo`–`We` previous,
  right `Fr`–`Su` next — pointer twin of `Up`/`Down`); middle `Th` stays
  inert. See [docs/calendar.md](docs/calendar.md).

- Clipboard history PNG rows show an in-memory thumbnail in the picker's
  icon column (never written to disk); text rows stay text-only. See
  [docs/clipboard.md](docs/clipboard.md).

- Media row position suffix is click-to-seek when the player reports
  `CanSeek` (bridge `SetPosition` / relative `Seek`); without the flag the
  suffix stays display-only. See
  [docs/media-controls.md](docs/media-controls.md).

- Control-center Volume middle-click mutes the pointed row (same path as
  `m` / `Enter`); other Hub rows and blank stay inert — never seeks.
  See [docs/control-center.md](docs/control-center.md).

- MPRIS player pin persists across bridge restarts in
  `$XDG_STATE_HOME/jwm/mpris-pin` (else `~/.local/state/jwm/mpris-pin`); an
  empty bus keeps the pin so a later launch can reclaim it, while a dead
  name among live players still clears. See
  [docs/media-controls.md](docs/media-controls.md).

- Notification center middle-click dismisses the pointed row (same path as
  `d`/`Delete`); blank and the action strip stay inert — never clear-all.
  See [docs/notifications.md](docs/notifications.md).

- Wi-Fi and Bluetooth pickers middle-click forget with the same two-press
  arm as `d` (blank and passphrase/PIN/confirm stay inert). See
  [docs/control-center.md](docs/control-center.md).

- Confirmed audio device switches raise a labeled Audio Device OSD (picker
  after adopt-took; IPC `set_audio_device` after the worker re-read) — queued
  like volume / mic mute. See [docs/control-center.md](docs/control-center.md)
  and [docs/media-controls.md](docs/media-controls.md).

- Control-center Power Profile Enter/click and wheel cycle like
  Right / Left–Right (labeled OSD), so the row is no longer pointer-dead.
  See [docs/control-center.md](docs/control-center.md).

- Status-bar ShellHub while the shell is open: same page / Hub home
  dismisses (Alt+F10 twin); a different page hands over and opens that
  route. Lock still refuses. See
  [docs/control-center.md](docs/control-center.md).

- Clipboard history middle-click forgets the pointed row (same one-shot
  path as `d`/`Delete`); blank middle-click stays inert. See
  [docs/clipboard.md](docs/clipboard.md).

- Control-center Input row: click the mic icon (or press `m` while the row is
  selected) toggles mute through the same OSD path as `XF86AudioMicMute`;
  the rest of the row still opens the audio-input device picker. See
  [docs/control-center.md](docs/control-center.md) and
  [docs/media-controls.md](docs/media-controls.md).

- Multi-player media rows append a trailing `· o` hit target that opens the
  Players picker like the `o` key; `· p ‹next›` still cycles. Single-player
  rows stay unchanged. See [docs/media-controls.md](docs/media-controls.md).

- Control-center Power Profile `Left`/`Right` and IPC `set_power_profile`
  raise a labeled Power Profile OSD (same icons as the Hub row, no bar).
  See [docs/control-center.md](docs/control-center.md).

- Bridge `player_details` (`{player, identity?, status?}`) rides beside the
  existing `players` string list. The Hub Players picker prefers MPRIS
  Identity and shows a Playing/Paused/Stopped cue when details arrive;
  cycle/select keys stay bus suffixes. Old bridge↔jwm pairs stay
  suffix-only. `media/status` and `get_media_status` expose the field
  append-only. See [docs/media-controls.md](docs/media-controls.md).

- Hub Theme selection surgically persists `appearance.ui_theme` to the live
  TOML (comments preserved); IPC `set_config` stays session-only. See
  [docs/ui-theme.md](docs/ui-theme.md).

- Screenshot→clipboard on Wayland defers the native data-device PNG offer to
  the completion poll (no worker `wl-copy`); nested Wayland shares the same
  path. See [docs/clipboard.md](docs/clipboard.md).


- With more than one MPRIS player, `o` on the control-center media row opens a
  Players picker (audio-device shape): Enter/click pins that bus suffix via
  `select_player` and returns to the Hub; `p` and the `· p ‹next›` hint still
  cycle. Single-player sessions leave `o` a no-op. See
  [docs/media-controls.md](docs/media-controls.md) and
  [docs/control-center.md](docs/control-center.md).

- Wayland sessions re-offer clipboard PNG history through the compositor's
  own data-device `image/png` selection (`Backend::set_clipboard_png` on the
  three Wayland backends). Picker activate and `clipboard_copy` prefer the
  native image sender, then that path, then `wl-copy`. See
  [docs/clipboard.md](docs/clipboard.md).

- Control-center media row transport glyphs are pointer-operable: a click on
  the previous / next arrow skips like `Left`/`Right`, while the title and
  status icon still play/pause and the trailing `· p ‹next›` hint still
  cycles players. See [docs/media-controls.md](docs/media-controls.md).

- Alt+Tab switcher middle-click closes the pointed row without ending the
  gesture — the pointer twin of `Delete` / `BackSpace`, matching expose and
  the window tab strip. A middle click on blank is inert. See
  [docs/window-switcher.md](docs/window-switcher.md).

- Shell Hub **Theme** page (`T`, wire parameter `6`): lists the seven known
  `appearance.ui_theme` values, applies via the wallpaper `set_config` path,
  session-only. See [docs/control-center.md](docs/control-center.md) and
  [docs/ui-theme.md](docs/ui-theme.md).

- A click on the control-center media row's trailing `· p ‹next›` hint
  cycles players like the `p` key; the rest of the row still play/pauses.
  `media/status` and `get_media_status` now carry the append-only `players`
  list so bars can build a picker without scraping the row. See
  [docs/media-controls.md](docs/media-controls.md).

- Clipboard history now keeps PNG images alongside text. Image-only copies
  (and screenshots published to the clipboard) land in `Alt+Ctrl+V` as text
  labels — `PNG 1920×1080  1.2M` when the IHDR is readable — filtered by
  tokens like `png` / `image` / dimensions, and re-offered on activate
  through the native X11 image sender, the Wayland data-device PNG offer, or
  `wl-copy` as fallback. Text still wins when an offer carries both;
  payloads over 4 MiB are skipped; the store stays memory-only and never
  writes images to disk. `get_clipboard` exposes kind/size/dims metadata
  only — never raw PNG bytes. See [docs/clipboard.md](docs/clipboard.md).

- Bars and scripts can follow the microphone mute flag end to end.
  `get_mic_mute` answers `{ "muted": true|false|null }` from the same
  cached flag the control-center Input row reads (`null` means never read),
  warming the coalesced control snapshot first like `get_audio_devices`.
  Every shown-flag change publishes `audio/mic` on the `audio` topic. See
  [docs/media-controls.md](docs/media-controls.md).

- The microphone's mute flag is settable over IPC and visible in the shell.
  `set_mic_mute {"muted": bool}` sets the default source's mute with the
  volume keys' queued semantics — an immediate `ok` ack and the mic OSD
  drawn from the optimistic estimate, then the controls worker's read-back
  confirming or correcting it (the OSD refreshes in place) — same queued
  shape as `set_audio_device`. A non-boolean
  `muted` is rejected (`set_mic_mute: expected boolean field 'muted'`), a
  session with no audio tool gets the key path's own
  `no working audio control (wpctl/pactl/amixer)` answer, and the command is
  advertised through `get_capabilities`. The control-center Input row is the
  indicator: the slashed microphone icon while the default source is muted,
  the unchanged row when unmuted or never read, and an open panel repaints
  when a read-back corrects the shown flag. See
  [docs/media-controls.md](docs/media-controls.md).

- With more than one MPRIS player running, `p` on the control-center media
  row pins the row — and the transport keys with it — to the next player in
  the bridge's list, wrapping around; the row names the target ahead of time
  with a trailing `· p ‹next player›` hint. The bridge holds the pin while
  that player's bus name is alive, clearing it when the pinned player exits,
  and re-publishes its state, so the switch raises the media OSD like any
  track change. Single-player sessions are unchanged (`p` is a no-op, no
  hint), and the lock screen's now-playing row never grows the hint. The
  bridge's push carries the player list as an append-only `players` field.
  See [docs/media-controls.md](docs/media-controls.md).

- The calendar card answers the pointer: a click on a blank leading cell of
  the month grid flips to the previous month, a blank trailing cell flips to
  the next, and a click on today's bracketed cell returns to the current
  month — the pointer counterparts of `Left`/`Right` and `t`, and now what
  the footer hint advertises. Clicks on ordinary days, the clock line and
  the header remain no-ops. See [docs/calendar.md](docs/calendar.md).

### Changed

- X11 `updategeom` / `createmon` seed per-tag layouts by the output's
  connector / `stable_key` (same key as Wayland `add_monitor`), so RandR
  display changes restore connector-keyed `[[layout.tags]]` instead of only
  the positional monitor index. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- Session restore clamps a maximized window's saved restore rectangle into
  the destination monitor's work area before re-applying maximize, so
  absolute coords from another geometry cannot land off-screen after a
  connector remap. See
  [docs/window-placement.md](docs/window-placement.md#restarts-and-sessions).

- xdg-shell dual-axis maximize clears the four `Tiled*` edge states, and
  configure / focus / size-enforce paths keep them off while the window is
  maximized or fullscreen, so clients do not keep tile-edge CSD under a
  work-area fill. See
  [docs/window-placement.md](docs/window-placement.md#axes).

- wlr-foreign-toplevel `set_maximized` / `unset_maximized` are treated as
  pager/user origin, so a Wayland taskbar can promote a tiled window like an
  EWMH pager (`data[3] == 2`). xdg-shell and XWayland stay client-like. See
  [docs/window-placement.md](docs/window-placement.md#who-may-maximize-what).

- Float snap halves and corner quarters (`snap_window` and mouse edge/corner
  drops) fill the monitor work area — the same area maximize uses — so they
  no longer cover the status bar, docks or tab bar. Drop-zone hit-tests still
  use the outer monitor edges; top-edge / `snap_window maximize` is unchanged.
  See [docs/window-placement.md](docs/window-placement.md#drag-and-snap).

- Native X11 `_NET_WM_STATE` maximize requests honor EWMH source indication
  (`data[3]`): a pager (2) may promote a tiled window like `togglemaximize`;
  an application or unspecified source (0/1) still cannot. xdg-shell and
  XWayland have no source field and keep the client-like admission rule;
  wlr-foreign-toplevel is mapped to pager (see above). See
  [docs/window-placement.md](docs/window-placement.md#who-may-maximize-what).

- `snap_window maximize` (`Alt+Shift+Up`) and dropping a dragged window at
  the top edge now perform a real maximize: they fill the work area instead
  of the whole monitor, set the EWMH/xdg maximized state, and toggle back to
  the previous rectangle. Dragging a maximized window, or snapping it to a
  half or a quarter, unmaximizes it in place first, and a cancelled drag
  restores it maximized. `togglefloating` on a maximized window unmaximizes
  it first. A maximize that an application, a source-less protocol or a
  pre-set atom asks for is refused for a window the tiling layout manages,
  as in sway, and pre-set maximized atoms on tiled windows are cleared when
  JWM manages them; use `togglemaximize` or an EWMH pager (source 2) to
  maximize a tiled window. Fixed-size windows no longer advertise Resize or
  Maximize in `_NET_WM_ALLOWED_ACTIONS`.

- IPC `view`, `tag`, `toggleview` and `toggletag` reject a missing argument, a
  zero mask, and a mask with no bit inside the configured `tags_length`, with
  an error naming the command. They used to report success for a call that did
  nothing (or, for `view`, switched to an empty tag set). A mask that selects
  at least one configured tag, the all-tags mask included, behaves as
  before.

- IPC `set_hdr_metadata` rejects an `enabled` that is present but not a
  boolean (`"false"`, `0`, `null`) instead of turning HDR on, and
  `clipboard_copy` rejects an `index` that is present but not a non-negative
  integer instead of offering the newest entry. Leaving either field out keeps
  its old default.

- `set_power_profile` and the Hub's Power Profile row no longer run
  `powerprofilesctl` on the compositor thread: the switch is queued on the
  controls worker like the volume keys and `set_mic_mute`. The IPC reply
  acknowledges the submission, the labeled OSD draws the requested profile at
  once, and the worker's read-back confirms or corrects it; `power/profile`
  carries the profile that actually took. The synchronous "stayed on" error is
  gone, and before the profile list has been read the command answers "not
  read yet" and starts the read. See
  [docs/control-center.md](docs/control-center.md#ipc).

- `Alt+Ctrl+M` stops the microphone recorder without waiting for the file: the
  MIC chip clears at once and the stop card follows once the file is written.
  Meanwhile `get_audio_recording_status` reports `"finalizing": true` (a new
  field) and a new recording is refused until the file is done. IPC
  `stop_audio_recording` still waits, so its reply confirms the file is
  finalized. See [docs/audio-recording.md](docs/audio-recording.md#stopping).

- A subscription is acknowledged with what the server did with it instead of a
  bare success: `unknown_topics` names the requested topics no event can ever
  match (a mistyped `windows`, for example), `subscribed` is the normalized
  list it stored, and `dropped` names the topics it did not store with a
  reason (`empty`, `too_long`, `duplicate` or `limit`), counted in full by
  `dropped_total`. Valid topics deliver exactly as before. See
  [README.md](README.md#control-jwm).

- The session lock and the idle lock replace an open shell panel (launcher,
  Hub, pickers, notification center) instead of being refused while it is up,
  and run the panel's own teardown; the idle lock no longer retries every 5
  seconds behind a panel. Opening any panel, the lock included, cancels an
  active screenshot or recording-region selector instead of taking over its
  grabs, and the overview, expose and the annotation layer refuse to open on
  top of each other, a panel or a capture selector. Likewise IPC
  `take_screenshot`, `toggle_recording` (when it would start the region
  selector) and `adjust_recording_region`, which used to reply `ok` there, now
  fail while a panel, the overview, expose, the annotation layer or the other
  capture selector is on screen, for example with `screenshot selection cannot
  start while expose is active`; a screenshot request still waiting for the
  pointer (a status-bar pill click) is dropped instead of opening over them.
  Stopping a recording and cancelling an open selector still work. See
  [docs/idle.md](docs/idle.md).

- `get_idle_status` and the `idle/state` event report `locked` for the session
  lock only; a monitor showing its own unlock prompt reports `false`, and that
  prompt no longer holds the idle lock off — the lock stage replaces it. See
  [docs/idle.md](docs/idle.md#over-ipc).

- Locking a monitor exits expose, and closes the overview when the windows it
  is cycling are on that monitor, instead of leaving the newly covered windows
  on show elsewhere. See [docs/monitor-lock.md](docs/monitor-lock.md).

- Alt+Tab, the launcher's window search and expose no longer list managed
  docks, panels (polybar, tint2) or desktop icon layers: such a window sits on
  every tag at the tail of the MRU order, exactly where `Alt+Shift+Tab`
  starts, so committing could focus the bar. See
  [docs/window-switcher.md](docs/window-switcher.md).

- `jwm-tool perf compare` fails the gate when the candidate lost a measurement
  the baseline recorded (scenario skipped or absent, metric missing) instead
  of printing it as not comparable, except for `input_latency` and
  `allocation_steady`, whose presence depends on the recording conditions.
  `jwm-tool perf record` records a session without a compositor, or a refused
  benchmark start, as skipped scenarios and still writes the baseline. It
  stops a benchmark that overruns its 300-second deadline; the overdue run
  keeps its system label from the report `benchmark stop` returns, so
  `compare` against a completed baseline prints `[FAIL]` for it instead of
  refusing the pair. It checks `get_waterlily_status` before
  `--waterlily-workload`, skipping with the reason when WaterLily or its
  worker is unavailable, uses an animation that is already running as is,
  and switches WaterLily back off only when it switched it on. See
  [docs/performance.md](docs/performance.md).

- `jwm` and `jwm-support` without `--backend`/`JWM_BACKEND` pick wayland-udev
  only when it is compiled in, and otherwise the first compiled backend; a
  slim `--no-default-features` build no longer starts or diagnoses a backend
  it does not contain.

- Screen recording: a region reshaped mid-recording is scaled uniformly to fit
  the fixed video canvas and centred with black bars, cursor included, on both
  X11 and Wayland, instead of being stretched; moving the region, or a region
  of the original shape, still fills the whole canvas. See
  [docs/audio-recording.md](docs/audio-recording.md).

- The screen recorder's ffmpeg log is no longer a fixed, world-shared file in
  `/tmp` (`jwm-ffmpeg.log` on X11, `jwm-wayland-recording-ffmpeg.log` on
  Wayland): it lives in `$XDG_RUNTIME_DIR` (or under a per-user name in the
  temp directory), is created mode 0600, and a symlink or someone else's file
  at that path is refused. The X11 recorder logs the path when a recording
  starts.

- X11: a pager's `_NET_CURRENT_DESKTOP` request switches the selected monitor
  to that tag, and `_NET_RESTACK_WINDOW` is no longer advertised in
  `_NET_SUPPORTED`: JWM never acted on it, so pagers were promised a restack
  that could not happen.

- The Hub Night Light row toggles through `toggle_night_light`, so Enter
  raises the same labeled OSD the keybinding does. Network and Bluetooth
  row radio flips (and a confirmed Bluetooth power-off) raise the Wi-Fi /
  Bluetooth OSD too; the first press that only arms Bluetooth power-off
  stays quiet. See [docs/control-center.md](docs/control-center.md) and
  [docs/session-menu.md](docs/session-menu.md).

- `Alt+Shift+/` (`show_keybindings`) lists configured `behavior.gesture_swipe`
  rows after the keyboard binds — `3f left`-style shortcuts with the same
  action-description style as keys (raw function names stay as configured).
  An empty swipe table leaves the viewer unchanged.

- The lock screen's now-playing row is documented as the control-center
  media row minus its transport cluster **and** the player-switch hint, so
  a multi-player session does not grow a `· p …` control on the lock card.
  See [docs/idle.md](docs/idle.md).

- The screenshot editor's toolbar eases in when it appears instead of
  popping in at full alpha: one blank frame after publish, then a 120 ms
  ease-out fade to full opacity carrying the track, the button chips and the
  icons together. Moving the pointer across the buttons does not restart the
  fade, withdrawing is instant, and with animations disabled the strip is at
  full opacity from its first frame and renders no extra frames. Hit
  geometry is untouched. See [README.md](README.md#the-screenshot-editor).

### Fixed

- X11: a toast card press that dismisses or invokes an action now swallows
  the paired button release (same one-shot latch as the screenshot wheel),
  so clients under the card no longer see a stuck button. Wayland already
  latched toast buttons at the compositor.

- Wayland: `unset_maximized` from an xdg-shell client always gets a
  configure, and every set/unset request gets exactly one, a refused one
  included. XWayland maximize requests are written back to the window, and a
  maximized XWayland window can no longer move or resize itself out of its
  maximized geometry.

- XCB: `_NET_WM_STATE` writes no longer replace the whole property with a
  single atom when reading it fails; the write fails and the transaction
  that asked for it rolls back instead. A `_NET_WM_STATE` of the wrong type
  (not an ATOM list) is still replaced like an absent one, as on x11rb, so it
  cannot make every later maximize, fullscreen or minimize write fail.

- Wayland: a direct-scanout frame can no longer bypass a locked monitor's
  shade, the MIC chip or the capture hint; each of them now forces
  composition, so a fullscreen game or video is never scanned out over a
  shade.

- Wayland: `ext-session-lock`'s `locked` event is sent only once a locked
  frame (black shield, lock surface on top) has been presented on every output
  that was showing content, as the protocol requires, so `swaylock -f &&
  systemctl suspend` can no longer suspend with the desktop still on screen.
  An output that cannot present confirms the lock after 1 second at most, and
  lock surfaces follow an output's new mode, scale or transform while locked.
  A lock request made while another live locker holds the lock, or is waiting
  for it, is refused with `finished`; before, any client that could bind the
  session-lock manager could take over a live lock and unlock it without the
  password. A new locker can take over only after the previous one died or
  gave up its request.

- X11: a `jwm-remote` capture lease that is already held when the compositor
  starts, or is re-enabled, keeps the screen composited; fullscreen unredirect
  could freeze the remote view until the lease was republished.

- Wayland: an ext-image-copy-capture request for a toplevel that has closed,
  or an output that is gone, gets a stopped session instead of a stream of the
  first output, and such stale targets no longer abort the compositor. Capture
  sessions of an output that is unplugged or replaced by a KMS rebuild are
  stopped too, and so is the session of a window that closes while it is being
  captured, instead of failing every frame the client asks for.

- Wayland: sandboxed clients (connected through `wp_security_context_v1`, such
  as Flatpak apps) are no longer offered the privileged globals: layer shell,
  screencopy, image-copy capture, output management, output power, gamma
  control, virtual pointer and keyboard, input method, session lock, data
  control, foreign-toplevel management, the ext-foreign-toplevel list,
  ext-workspace, or a nested security context. Keyboard-shortcut inhibitors
  created by sandboxed clients never take effect, so a focused sandboxed
  window cannot swallow JWM's own bindings. XDG activation tokens are
  single-use, and unused ones expire after 10 seconds.

- The notification bridge and the status bars' screenshot request refuse an
  IPC socket whose runtime directory is not a real directory owned by the user
  with no group or other access, or whose endpoint is not the user's own
  socket — the checks the compositor applies to the directory it serves from.
  They used to talk to whatever answered at the `$XDG_RUNTIME_DIR` or
  `/tmp/jwm-<uid>` path, which another local user could create first. An
  explicit `JWM_SOCKET` is still used as given.

- A nested, benchmark or test JWM that inherits the session's runtime
  directory no longer makes the real `jwm-tool daemon` quit — taking the
  user's session with it — when it exits. Only the JWM the daemon launched
  (which it now marks with `JWM_DAEMON_PID`) asks the daemon to quit.

- The Wi-Fi passphrase reaches nmcli on its standard input (`nmcli --ask`)
  instead of its command line, where `ps` showed it to every local user; a
  passphrase with a control character is refused with a reason. Joining no
  longer blocks the compositor on the saved-profile lookup, which the scan
  worker now reads with the list. Localized sessions keep nmcli and rfkill
  detection (`nmcli -t radio wifi`, `rfkill` under `LC_ALL=C`). See
  [docs/control-center.md](docs/control-center.md#passphrases).

- `_NET_CLOSE_WINDOW` and foreign-toplevel close requests for a window JWM
  does not manage are ignored instead of closing it — JWM's own check window
  included.

- `jwm-remote`'s trusted-LAN notice says what the transport does: it encrypts
  and authenticates with the pre-shared key, without forward secrecy. It used
  to claim the screen was not encrypted.

- Monitor lock: unplugging or changing outputs can no longer leave the
  selection on a shaded monitor, or a shade on an output other than the one
  that was locked. Activating or restoring a window that sits behind a shade
  (taskbar, launcher, `focus_window`) is refused instead of switching tags on
  the unlocked monitor or un-minimizing it behind the shade. A display change
  that drops the last lock hands back a compositor a panel had switched on,
  unless a panel is still open. See
  [docs/monitor-lock.md](docs/monitor-lock.md).

- The overview drops windows that closed under it, so cycling and `Enter` no
  longer land on a dead entry, and closes when none are left.

- Wayland: session-lock surfaces are sized in logical units on scaled and
  rotated outputs.

- Wayland: a dialog-like toplevel whose initial configure fell back to the
  compositor's timer no longer deadlocks the compositor.

- Wayland: two idle inhibitors on one surface no longer cancel each other, and
  a surface that is destroyed, or a client that crashes, stops inhibiting
  idle.

- XWayland: an X11 copy retires the clipboard-history entry JWM was offering,
  X11 applications can paste history entries, and an X11 owner that exits no
  longer clears a newer offer. Clipboard-history captures are recorded in
  selection order.

- XWayland: a managed window's ConfigureRequest goes through the same policy
  as on X11, so an XWayland window can no longer move or resize itself out of
  its tile, and a Wayland client's own geometry no longer overrides a
  fullscreen, picture-in-picture or parked window. XWayland clients that ask
  for a keyboard grab (virtual machines, remote desktops) get it.

- Wayland: a destroyed layer-shell role is fully retired, so a new layer role
  on the same surface is managed afresh.

- Wayland: wlr-screencopy regions on scaled and rotated outputs are captured
  from the right pixels (they were taken as buffer pixels and checked against
  the unrotated mode). The output-management and workspace managers answer
  `stop` with `finished`, so clients waiting for the handshake no longer hang.

- Wayland: wlr-gamma-control allows one control per output; a second one
  (gammastep alongside wlsunset, say) fails instead of fighting over the ramp,
  a control for an output that is gone fails instead of landing on another
  output, and destroying a control restores the identity ramp only if it had
  applied one. A control whose output is unplugged, replaced by a KMS rebuild
  or moved to a LUT of another size is sent `failed` at once, so a night-light
  client that is not changing its ramp learns to re-create it. The gamma
  takeover clears the colour pipeline's CTM and LUT in one atomic request.

- Wayland: a replaced output's `wl_output` global is withdrawn from clients
  and destroyed, instead of lingering after every hotplug and VT switch.

- Wayland/KMS: rebuilding the KMS state (VT switch back, hotplug) keeps each
  surviving monitor's mode, scale, transform and position, and a client gamma
  ramp, where they can still be honoured, and the window layout follows the
  replayed positions. A failed rebuild is retried with backoff (from 250 ms,
  doubling up to 8 seconds, six retries), and DRM devices that are replaced,
  or whose build failed after opening them, are returned to the seat. Output
  captures queued for an output that went dark or away are failed instead of
  left waiting, and frame callbacks and presentation feedback follow the
  refresh rate after a modeset.

- Wayland: a change applied through wlr-output-management (wlr-randr, kanshi)
  reaches the window layout, and a soft-disabled head leaves it, moving its
  windows to a visible monitor; output-management clients are sent the new
  heads after a hotplug and after their own apply, and a configuration made
  against stale heads is cancelled. The nested backends, which cannot apply
  one, no longer advertise output management. ext-workspace clients (waybar)
  see each monitor's group, its active workspace and hotplugged outputs, and
  activating a workspace switches the tag on the monitor it belongs to rather
  than on the selected one. Workspace groups follow an output rebuilt by a VT
  switch or re-plug, a `wl_output` bound later still gets `output_enter`, and
  the workspace count follows the configured `tags_length` instead of always
  being nine; a config reload that changes it re-sends each workspace group
  with the new count at once. Foreign-toplevel handles get
  `output_enter`/`output_leave`, so per-output taskbars list each window on
  the output it is on, and XWayland windows are listed too.

- Wayland: partial-damage frames no longer leave stale pixels. The first frame
  after a panel, the launcher, a lock shade, the debug HUD, the tab bar, the
  screenshot toolbar, idle dim, a postprocess filter or an animation's last
  frame goes away is redrawn in full; the cursor and layer surfaces are no
  longer blended over their own retained pixels, and on the scene-linear (HDR
  and wide-gamut) routes a resting cursor or an unchanged top or overlay layer
  surface such as a bar is redrawn only inside the damaged area, so it does
  not widen a partial redraw; and raising or lowering a window without moving
  it redraws the overlap.

- Wayland: on the scene-linear routes the frosted status bar decodes its
  sRGB-encoded backdrop instead of mixing it as linear light, the wallpaper is
  no longer sRGB-decoded twice, and idle dim matches the encoded routes and
  screenshots exactly (HDR highlights are clamped while dimmed). A wallpaper
  change that also changes the mode keeps each image in its own mode through
  the crossfade.

- Wayland: expose and peek draw windows with an alpha channel with their real
  transparency instead of black; peek keeps the focused window's menus,
  submenus and input-method popups lit; the recording-region veil no longer
  blacks out the screen; the overview's selected title is drawn as UI text;
  the snap-preview outline stays in colour range; and the capture hint forces
  full frames like the REC and MIC chips. Direct-scanout diagnostics stop
  rejecting fullscreen windows for decorations that are never drawn on them,
  and per-window subpixel state no longer grows over a long session.

- X11: a window whose content changes on a hidden tag no longer forces a
  full-screen composite every frame; a window skipped by the
  texture-from-pixmap budget is refreshed on a follow-up frame instead of
  staying stale; a window destroyed while presented directly no longer leaves
  composition bypassed; the EGL preserved-buffer fallback keeps partial
  redraw; GLX binds only 32-bit visuals through an RGBA config; and a
  WaterLily worker that stops reading can no longer block the compositor.

- The wallpaper picker opens at once and lists the folder in the background,
  without a stat per file and in a stable order, and browsing it quickly no
  longer queues a decode per entry: a superseded side preview is dropped
  before it decodes or resizes, on X11 and Wayland alike.

- XCB: output queries use `GetScreenResourcesCurrent`, like x11rb, instead of
  forcing a RandR hardware probe, and a click delivered to the root over a
  client that does not select button presses reaches that client instead of
  counting as a root or tab-strip click.

- X11: removing a window's strut, size hints, `WM_TRANSIENT_FOR`, or a
  `_MOTIF_WM_HINTS`/`_GTK_FRAME_EXTENTS` hint that dropped its border takes
  effect (the border comes back); worker threads can no longer swallow the
  SIGCHLD that reaps launched programs, and the programs JWM starts (launched
  applications, the status bar, session-menu actions, the idle lock and DPMS
  commands, the audio recorder, the screen-recording encoders, and long-lived
  helpers such as the daemon a launcher leaves behind or the Bluetooth
  pairing agent) no longer inherit a blocked SIGCHLD; and routine
  asynchronous X errors from destroy races are logged at debug level, the
  rest as warnings, instead of being dropped (x11rb) or all logged as errors
  (XCB). On x11rb, key and button grabs follow a NumLock modifier that moved
  in a keymap change, and drag tracing is off unless `JWM_DEBUG_DRAG` is set.

- Aspect-ratio size hints shrink a window to fit its cell instead of growing
  it past the cell (a 16:9 client in a 900×1000 tile came out 1778×1000).
  Tatami's 2- and 4-window patterns and centered-master or three-column with a
  single stack window fill the work area.

- Dropping a dragged floating window snaps and settles that window, not
  whichever window the selection moved to during the drag. A move or resize
  requested by a window on a hidden tag, or a minimized one, updates the place
  it will come back to instead of showing it.
  `_NET_WM_STATE_DEMANDS_ATTENTION` honours Do Not Disturb and focus like the
  urgency hint, is cleared when the window is activated, and clearing it no
  longer clears a standing ICCCM urgency.

- Multi-monitor: a mode or scale change on an output refits its fullscreen
  windows; on X11, output ids stay with the monitors whose rectangle they
  cover when outputs are replugged or swap places, and a layout change carries
  visible floating windows with their monitor; on Wayland, unplugging and
  replugging an output no longer swaps geometry between two monitors; a
  replacement output takes in the windows left without one, preferring a
  monitor that is not locked; and `focus_tab` addresses a monitor by its
  number after a replug.

- Saving and restoring a session no longer records hidden scratchpads, and a
  restore never reveals one.

- Shrinking `tags_length` in a config reload moves windows off the retired
  tags onto their monitor's view instead of stranding them; a named scratchpad
  left without tags stays parked. Any change to `tags_length` reaches EWMH
  pagers (`_NET_NUMBER_OF_DESKTOPS`) and ext-workspace taskbars with the
  reload instead of at the next tag switch or monitor change.

- Restart is no longer refused forever when the config file cannot be written
  (for example a home-manager link into the read-only Nix store) while a
  per-tag layout change is pending: the layout write is logged and skipped,
  and the periodic retry stops warning every 2 seconds. See
  [docs/minimized-dock.md](docs/minimized-dock.md).

- Saving the theme from the Hub no longer breaks a config whose `[appearance]`
  header carries a comment, spaces or quotes; a file shape the line edit
  cannot handle safely is left untouched with a logged error while the theme
  still applies, and a per-tag layout save refuses the same way rather than
  writing a file that would no longer load. A config edit made before JWM's
  own theme or layout write is reloaded instead of being mistaken for that
  write. See [docs/ui-theme.md](docs/ui-theme.md).

- Expose and the tags overview never lay out more columns than windows, so a
  single window is centred; PiP and attention windows keep their own frame
  during the focus pulse when ordinary borders are off; and a
  `behavior.wallpaper_tags` entry with a tag index of 32 or more matches no
  tag instead of panicking.

- A peripheral battery (a wireless mouse or gamepad, `scope=Device`) is no
  longer taken for the system battery: the control center, the low-battery
  alert and the compositors' power saving (which throttled the frame rate on
  such desktops) ignore it.

- A screenshot whose annotations cannot be baked in keeps the original file
  intact and says so with an urgent "Screenshot saved without annotations"
  toast; the annotated file replaces the capture atomically. A clipboard
  capture whose annotations cannot be baked in is copied without them and
  says so with an urgent "Screenshot copied without annotations" toast
  instead of the ordinary "Screenshot copied to clipboard" one.

- The notification center keeps the highlight on the notification you picked
  when a newer one above it is closed, and the notification-history writer
  holds at most one pending snapshot however fast notifications arrive.

- Audio recording: a recorder that stops on its own (a USB microphone
  unplugged, ffmpeg killed) is collected moments later, clearing the MIC chip
  and reporting the failure with an urgent toast and an
  `audio_recording/error` event, instead of leaving the session looking active
  until the next toggle silently reopened the microphone. A recorder that has
  not initialized within 3 seconds is abandoned instead of hanging the
  compositor, and further starts are refused until the stuck device open
  returns. See [docs/audio-recording.md](docs/audio-recording.md).

- The wallpaper-derived theme no longer reports `pending: true` forever when
  its extraction thread could not start, and the next config apply retries it.

- IPC: replies to several requests sent at once arrive in request order even
  when one of them is malformed JSON; a final request without a trailing
  newline is answered when the client closes its write side; and a client that
  half-closes its socket (`socat`, `nc -N`) receives its whole reply instead
  of a cut-off one.

- IPC: `get_tree` marks only the focused window as focused, as `get_windows`
  does, instead of each monitor's selection, and `get_recording_status` no
  longer runs ffprobe on every poll of an unchanged file it already rejected.

- The debug HUD's CPU percentage and `get_metrics`' `cpu_load_percent` no
  longer read low while virtual machines run: guest time was counted twice.

- `jwm-tool debug` lists the JWM processes again: its grep received the flag
  and the pattern glued into one argument, and a bare `jwm` process did not
  match.

- Documentation: IPC examples use `jwm-tool msg` (there is no `jwm-msg`
  binary, and `jwm-tool get_tearing_hints` is not a subcommand), the Bluetooth
  picker closes with `Ctrl+Alt+Shift+B`, the VRR notes no longer describe
  `get_outputs` or a `set_vrr_enabled` IPC command, neither of which exists,
  and the hardware-validation and daily-drive baseline examples run
  `jwm-tool perf record --out <file>` and
  `jwm-tool perf compare <baseline> <candidate>` (there are no `--output`,
  `--baseline` or `--candidate` flags). A new `docs_drift` test keeps the IPC
  and `jwm-tool perf` examples and the Control Center chords honest.

- X11: the status bar's content is back under a glass theme. The bar's
  solid-glass sheet binds its shader behind the GL state tracker's back, and
  the tracker's dedupe then skipped restoring the window shader, so the bar
  pixmap was drawn through the glass program: a frosted sheet with no tags,
  title or clock on it. The tracker is now reset before the restore.

- X11: input-method candidate windows no longer flicker while typing. fcitx
  and ibus unmap and remap their popup on every keystroke, and each cycle
  started a closing fade and resumed an opening one, so the panel pulsed
  between opacities for as long as the user typed. Classes listed in the new
  `behavior.fade_exclude` skip every open/close effect (fade, scale, open
  ripple, close particles); the default covers `fcitx`, `fcitx5`,
  `fcitx-qimpanel`, `sogou-qimpanel` and `ibus-ui-gtk3`. Set it to `[]` to
  animate everything again.

- Pending OSD no longer drops a confirmed audio-device name card when a
  volume/mic correction races the same flush: dual slots prefer the named
  device card; the next volume key re-raises the level OSD. See
  [docs/control-center.md](docs/control-center.md).

- X11 screenshot→clipboard no longer toasts a false failure when the worker
  already offered the PNG and a later poll re-offer fails; Wayland’s
  first-offer-on-poll failure path stays Failed. See
  [docs/clipboard.md](docs/clipboard.md).

- `Enter` (join) in the Wi-Fi picker while a forget is still deleting its
  profile is now a no-op instead of racing the delete and the re-read its
  completion kicks off — the mirror of the forget key's own in-flight
  coalescing. See [docs/control-center.md](docs/control-center.md).

### Added

- The control centre's Volume and Brightness rows now answer the wheel:
  scrolling over a slider row adjusts that value by 5% per click — the
  pointer counterpart of `Left`/`Right` — and the selection pill follows the
  pointer's row, so a later keypress keeps acting on the row you scrolled.
  Scrolling anywhere else still browses the list. See
  [docs/control-center.md](docs/control-center.md).

- The bars themselves now take press-and-drag: pressing on the 20-cell bar
  sets the value to the pointed position and tracks the pointer until
  release, with the external tool calls throttled to actual percent
  changes. Pressing anywhere else on the row keeps its `Enter` action, so a
  click beside the bar still toggles mute on Volume — and setting a level
  on a muted sink unmutes it. The hit geometry is measured off the exact
  drawn strings in the configured UI font, not a guessed cell width.

- The notification centre's numbered action strip is pointer-clickable: a
  click on a chip invokes that action directly through the same pipeline as
  its digit key, the gutter and the gaps between chips are deliberate
  no-ops rather than near-misses that fire a neighbour, and hovering a chip
  moves the row's ✓ cursor onto it so a following `Enter` or digit acts on
  what the pointer was over. See
  [docs/notifications.md](docs/notifications.md).

- The modal dim behind shell panels now fades in with the card's own open
  spring instead of snapping to full darkness in one frame — and the layout
  filmstrip's and tags overview's own dims now ride the same `opened²`
  envelope. Fade-in only: closing any panel is still instant, and with
  animations disabled every dim is full from the first frame. The lock
  screen is exempt on purpose — hiding the desktop instantly is its job.

- Exposé thumbnails now carry window-title labels on both compositors:
  every cell draws the window's title centered just inside its top edge,
  ellipsized to the cell width in the same system-UI typography as the tab
  strip and cube overview titles. Empty titles draw nothing, labels fade
  with the grid, and hit-testing is unaffected. See
  [docs/expose.md](docs/expose.md).

- The lock screen now leads with the current time (`HH:MM`, 24-hour) and
  the spelled-out date above the password row, repainting on each
  wall-clock minute through a wakeup scheduled only while the lock is up,
  and shows a quiet "Caps Lock is on" row under the password row while caps
  lock is active. Information-only: the backdrop, grabs, and PAM exchange
  are unchanged. See [docs/idle.md](docs/idle.md).

- The wallpaper picker shows a live thumbnail of the highlighted candidate
  on a card to the right of the list, tracking arrow-key and pointer
  selection. Decodes run asynchronously on the wallpaper loader's worker
  pool with latest-highlight-wins, are aspect-fit into a 480×360 frame and
  never upscaled, and outputs too narrow to fit the frame keep the text
  list exactly as it was. No placeholder or spinner — until the thumbnail
  lands the picker looks unchanged. See [docs/wallpaper.md](docs/wallpaper.md).

- `snap_window <direction>` is a new bindable and IPC command —
  the keyboard/scripting equivalent of dragging a floating window to a
  screen edge, using the exact mouse-drop geometry (for halves and quarters:
  full monitor rect, size hints respected, and the snapped rect becomes what
  a later `togglefloating` restores; `maximize` is a real, toggleable
  maximize that fills the work area and restores the previous rect).
  Directions: `left` / `right` / `top-left` / `top-right` / `bottom-left` /
  `bottom-right` / `maximize` (case-insensitive); tiled and fullscreen
  windows are deliberate no-ops; an invalid direction is an error. Dropping
  a dragged window into a screen corner (within `snap_dist` of both edges)
  now snaps it to that corner's quarter instead of resolving to a half.
  Default bindings: `Alt+Shift+Left` / `Alt+Shift+Right` / `Alt+Shift+Up` —
  the quarters have none (corners don't map honestly onto arrows); over IPC:
  `jwm-tool msg snap_window --args '"top-left"'`.

- The launcher and the window switcher show application icons beside their
  rows: resolved from each desktop entry's `Icon=` key (and each window's
  class via `StartupWMClass`) through the same cached icon-theme resolver
  the status bars use, decoded on worker threads, and popped in without
  moving the text. A missing icon keeps the plain text row — there is never
  an empty hole. See [docs/launcher.md](docs/launcher.md).

- The control-center media row shows `m:ss / m:ss` when the player reports
  both position and length — clamped for display, refreshed on the bridge's
  sweep, holding while paused, reset on a track change, and never a
  placeholder for streams. The `media/status` broadcast and
  `get_media_status` reply carry the new `position_us` / `length_us` /
  `position_label` fields (append-only, tolerated both directions).
  Display-only — no seeking. See
  [docs/media-controls.md](docs/media-controls.md).

- The clipboard picker is type-to-filter: typing narrows the history by
  case-insensitive substring, `BackSpace` edits, and `Enter` / `d` / `c`
  act on the filtered selection; the filter starts empty on every open. See
  [docs/clipboard.md](docs/clipboard.md).

- Resting the pointer ~500 ms on a tab-strip cell whose title is
  ellipsized floats the full title in a chip off the strip — above it, or
  below when the strip hugs the screen's top edge. Truncated titles only,
  never covering the hovered cell, fade following the animation switch with
  the dwell applying either way. See
  [docs/window-tabs.md](docs/window-tabs.md).

- Screenshots and screen recordings announce themselves now. A finished
  capture raises a toast with the saved path (or confirms the clipboard
  copy), and a failed one surfaces even under Do Not Disturb; starting and
  stopping a recording both toast the output path. While a recording runs,
  a small `REC` chip with the running time sits in the bottom-right corner
  of the recorded output — drawn after the frame's pixels are read, so it
  never lands in the video or in screenshots.

- The window switcher closes windows mid-gesture: with the list up,
  `Delete` / `BackSpace` closes the highlighted window through the same
  path as `killclient`, the next-oldest window slides under the highlight,
  and closing the last row ends the gesture. Pointer semantics are
  unchanged. See [docs/window-switcher.md](docs/window-switcher.md).

- Volume, brightness and media transport keys now work behind the lock
  screen: exactly the ten dedicated XF86 keysyms (volume raise/lower/mute,
  play/pause/next/previous/stop, brightness up/down), and only where the
  key is bound to the matching media action, so the press runs the same
  dispatch — controls worker included — as the unlocked session. The
  feedback stays invisible (the opaque backdrop hides the OSD) and every
  other key is still swallowed. See [docs/idle.md](docs/idle.md).

- The toggle family confirms on the OSD: Do Not Disturb, caffeine, night
  light, Wi-Fi and Bluetooth flips each raise a labeled card with its own
  icon carrying the target state (`Wi-Fi Off`, `Caffeine On`, …). DND uses
  the OSD precisely because toasts are DND-gated — a "Do Not Disturb On"
  toast would be swallowed by the state it announces — and the missing-tool
  error paths are unchanged. See
  [docs/control-center.md](docs/control-center.md).

- On the exposé grid the highlighted cell's window title now brightens with
  the hover ease — a pre-brightened texture composited over the normal
  label at the grid's opacity scaled by the hover progress (the ink mixed a
  pinned 0.35 toward white), with the keyboard selection riding the same
  channel. Wayland only: X11 keeps ring-only hover, per the standing rule
  that X11's existing visual features are eased, not expanded. See
  [docs/expose.md](docs/expose.md).

- The exposé grid closes windows mid-gesture, like the switcher: with the
  grid up, `Delete` / `BackSpace` closes the *highlighted* cell through the
  same `close_window` path `killclient` uses, the survivors keep their
  order with the entry after the closed one sliding under the highlight
  (the tail clamps), and closing the last cell ends the gesture. A
  highlight naming a window that already died is a no-op — the WM only
  knows the live candidate list, where the switcher owns its snapshot and
  can prune dead rows. Pointer semantics are unchanged. See
  [docs/expose.md](docs/expose.md).

- Toast cards now attribute their sender: when the sending application is
  known, a dim line above the title names it — the label ink at 0.72
  alpha, riding the title's one-line, 80-character sanitation. A card
  whose sender is unknown keeps its previous geometry pixel for pixel. See
  [docs/notifications.md](docs/notifications.md).

- `XF86AudioMicMute` toggles the default microphone's mute
  (`toggle_mic_mute`), riding the same controls worker as the volume keys
  — an optimistic flip on the event, the worker's read-back correcting —
  and raises a labeled `Microphone Muted` / `Microphone Unmuted` OSD card
  with its own icons. The key is deliberately absent from the lock-screen
  media passthrough: unmuting a microphone while locked is a privacy risk,
  so the passthrough stays at its ten keysyms. See
  [docs/media-controls.md](docs/media-controls.md).

- Standalone audio recording (`Alt+Ctrl+M`, or the `start_audio_recording`
  / `stop_audio_recording` IPC) now mirrors the screen recorder's
  feedback: a start toast carrying the output path once the recorder is
  actually running, a stop toast with the path, and failure toasts at
  critical urgency that break through Do Not Disturb. A persistent
  on-screen microphone indicator is deliberately out of scope. See
  [docs/audio-recording.md](docs/audio-recording.md).

- The Bluetooth picker's connected rows now carry the device's charge —
  `connected · 85%` — read from `org.bluez.Battery1` in the same
  `GetManagedObjects` sweep as everything else (an append-only payload
  field, so mixed old/new bridge↔jwm pairs keep working, and out-of-range
  readings are dropped). Disconnected rows show nothing: a stale charge is
  noise. See [docs/control-center.md](docs/control-center.md).

- Notification-center rows now resolve the record's app name through the
  same cached icon resolver the switcher's window rows use. App names are
  free-form sender strings rather than desktop ids, so a miss is the
  common case — and a safe one: unresolved rows render text-only exactly
  as before. See [docs/notifications.md](docs/notifications.md).

- Standalone audio recording now parks a persistent `MIC` chip for as long
  as the recorder runs — a red dot and a static label on the same flat
  pill as the screen recorder's `REC` chip. It takes the REC chip's
  bottom-right slot when the corner is free and stacks directly above it
  when screen and audio record together, never overlapping and never
  leaving the screen on degenerate displays. The chip shares the REC
  chip's draw discipline (drawn after the frame's pixels are read, so it
  never lands in a video or a screenshot, and direct scanout is blocked
  while it is up), appears only once the recorder has actually started,
  and clears on stop — success or failure — and at session teardown. A
  screen recording that captures the microphone keeps only the REC chip.
  See [docs/audio-recording.md](docs/audio-recording.md).

- The lock screen shows a now-playing row while a player is active: the
  control-center media row minus its transport cluster — `Title — Artist`,
  the `m:ss / m:ss` position, and the trailing status icon, so a paused
  player reads exactly as paused as it does in the control center. No
  album art, no controls; with no player active the row is absent and the
  lock is byte-identical to before. The row rides the bridge's 3-second
  push, re-syncing only when the visible text actually changes, and is
  seeded at lock-open so it is there from the first frame. See
  [docs/idle.md](docs/idle.md).

- Middle-clicking an exposé cell closes that cell's window — browser-tab
  semantics: the clicked cell, not the highlighted one — through the same
  close path the grid's `Delete` / `BackSpace` uses (the `killclient`
  `close_window`, the in-place rebuild with the tail clamp, and
  close-to-empty ending the gesture). A middle-click on empty space, or
  one naming a window that already died, is a no-op that leaves the grid
  up; the left-click commit and every other button are unchanged. See
  [docs/expose.md](docs/expose.md).

- Both connectivity pickers can forget what they list. In the Bluetooth
  picker `d` removes the highlighted *paired* device — a two-press armed
  confirm like switching the controller off (moving the selection disarms,
  and an unpaired row gets a status-line refusal) — through an async
  `bluetoothctl remove` worker, with the list re-reading on completion;
  forgetting the connected device drops the connection with the bond. In
  the Wi-Fi picker `d` deletes the highlighted network's saved profile by
  UUID (`nmcli connection delete uuid …`, so duplicate profile names
  cannot remove the wrong one); a network with no saved profile gets the
  honest `no saved profile for <ssid>` answer, and deleting the profile
  the link runs on is allowed — the post-delete re-read lands the truth
  on the row. See [docs/control-center.md](docs/control-center.md).

### Changed

- Volume and brightness input no longer blocks the WM on subprocesses.
  Keys, scrolls and slider drags queue onto a single controls worker that
  coalesces to the newest pending level — a full slider sweep costs a
  couple of tool invocations instead of two sequential spawns per percent —
  while the OSD and the slider row redraw instantly from an optimistic
  estimate, and the worker's read-back corrects the shown value only if it
  actually drifted. A hung `wpctl` (up to the 5 s helper timeout) no longer
  freezes the session. Mute-toggle ordering and set-on-muted-unmutes are
  preserved exactly.

- Pointer hover eases in everywhere the keyboard selection springs: the
  panels' quiet row preview, the exposé cell lift and ring, and the
  tab-strip hover fade in over ~120 ms with an ease-out curve instead of
  snapping. Hover-leave still clears in the same frame (nothing in the
  shell fades out), exposé hit-testing keeps using the base geometry, and
  with animations disabled everything snaps as before.

- Caps Lock now genuinely affects the lock screen's password field: ASCII
  letters shift (shift with caps types lowercase again), while digits and
  punctuation follow shift alone. Previously the modifier was stripped from
  the character path and only the indicator row noticed it. See
  [docs/idle.md](docs/idle.md).

- Lock-screen authentication no longer blocks the compositor: `Enter` hands
  the password to a one-shot PAM worker thread and the status row reads
  "Verifying…", so the ~2 s pam_unix wrong-password delay — or an
  arbitrarily slow pam_sss/fingerprint module — no longer freezes the
  session. While one attempt runs `Enter` is dead, typing collects the next
  attempt (and clears the row), `Esc` clears the field but cannot cancel
  the worker, and the password is wiped on every exit path; unlock still
  happens only on success, through the same code path. See
  [docs/idle.md](docs/idle.md).

- Wi-Fi and Bluetooth radio flips — the control-center rows and the
  key-bound toggles alike — no longer run blocking subprocesses on the WM
  thread. The set rides a background worker, the row and the
  `network/status` broadcast show the requested state optimistically, and
  the post-toggle re-read confirms or reverts it; a repeat press while a
  flip is in flight is a no-op, and the Bluetooth power-down two-press
  confirm is preserved. Known shape: on a machine with no nmcli or
  bluetoothctl the first press shows the optimistic OSD once before tool
  detection concludes; later presses keep the old error path. See
  [docs/control-center.md](docs/control-center.md).

- The audio device picker's `Enter` no longer runs two serial blocking
  `wpctl` spawns: the switch queues onto the controls worker (set-default,
  then the inventory re-read), the status line reads `Switching…`, and the
  marker moves only if the re-read says the switch actually took. See
  [docs/control-center.md](docs/control-center.md).

- In the launcher and the window switcher a window row's generic
  placeholder glyph now disappears in the same frame its real icon draws —
  the two never sit side by side; rows still decoding, or whose class
  resolves to nothing, keep the glyph. See
  [docs/window-switcher.md](docs/window-switcher.md).

- The tab-strip dwell tooltip keys on the window's session-stable id rather
  than its cell position: a relayout — a tab inserted, removed, or
  reordered — moves a showing chip with its window instead of dropping it
  or handing the accumulated rest to a neighbour, and a hovered window
  disappearing mid-dwell drops the chip the same frame. On outputs narrower
  than the chip's 500 px maximum its text budget shrinks, so the chip never
  overflows the right edge. See [docs/window-tabs.md](docs/window-tabs.md).

- The screenshot toolbar's buttons now ease their hover wash in over 120 ms
  (ease-out quad, gone the frame the hover leaves, full strength on the
  first frame with animations disabled) instead of flipping on in one
  frame. Settled states are pixel-identical to before, and the hit geometry
  is untouched. See [README.md](README.md#the-screenshot-editor).

- A fully settled toast no longer holds the compositor rendering
  display-rate frames for its whole hold (up to 30 s a card): frames flow
  only while a card's envelope is actually changing — fade-in, fade-out,
  dismiss, open spring — with wake-ups scheduled at the envelope
  boundaries. Hover pause, the fade timings and dismiss behavior are
  user-visible identical, and the volume/brightness OSD arm is deliberately
  unchanged this round. See [docs/notifications.md](docs/notifications.md).

- The tab strip now eases in over 120 ms when a group gains its second
  window — alpha only, a clamped ease-out quad with no overshoot — and a
  window joining an already-shown strip eases its cell the same way.
  Disappearing stays instant (nothing in the shell fades out), the first
  frame is full strength with animations disabled, and hit-testing is
  untouched. See [docs/window-tabs.md](docs/window-tabs.md).

- A fully settled OSD card no longer holds the compositor rendering
  display-rate frames for its whole 1400 ms hold: the settled-toast
  mechanism now covers the volume/brightness/media OSD too. Frames flow
  only while the card's envelope is actually changing — the fade-in and
  fade-out, the open spring, a replacement's width morph, and the frame
  that prunes the expired card — with wake-ups at the envelope boundaries
  (an explicit deadline on Wayland; X11 rides its standing 20 ms idle
  cadence). Every show or refresh, a held volume key's repeats included,
  is an input event that arms its own frame. Timings, appearance and
  direct-scanout exclusion are user-visible identical.

### Changed

- HDR signalling can now be enabled per output on the Wayland/KMS backend,
  behind a fail-closed gate that is re-evaluated every frame:
  `set_hdr_metadata` latches the request and the compositor asserts BT.2020 +
  `HDR_OUTPUT_METADATA` only while the whole chain holds — a capable EDID, the
  10-bit scanout chain, advanced colour management, the scene-linear target,
  a clean frame tail, no gamma-control client, and the software per-output
  delivery route. It is withdrawn the instant any of those stops holding (a
  toast, a session lock, a route change) and re-asserted when it comes back,
  so a single overlay no longer means re-issuing the command. Eleven named
  refusal reasons are reported per output. The CRTC LUT route is refused on
  purpose: it writes working-linear into a unorm framebuffer, clipping exactly
  the above-reference-white content HDR exists for. See the new
  [docs/hdr.md](docs/hdr.md). **Not verified against a real HDR display** —
  the compositor is proven never to signal HDR over sRGB pixels, but no panel
  has confirmed the metadata blob is interpreted as intended.

- The Wayland/KMS backend gained a per-output presentation policy, reported
  per output through `get_tearing_hints` and `render_decisions`. VRR is
  asserted while one mapped fullscreen client owns an output and cleared
  otherwise, on connectors where it can change without a modeset. The
  wp-tearing-control hint is now double-buffered and latched at
  `wl_surface.commit` as the protocol specifies, a second
  `wp_tearing_control_v1` on one surface raises `tearing_control_exists`, and
  every output carries a named reason for why a client asking to tear is not
  tearing — including `submission_cannot_request_async_flip`, which is the
  honest answer today.

- The Bluetooth picker can accept an *incoming* request: `a` arms a
  sixty-second window in which `jwm-bridge accept` registers a BlueZ agent,
  becomes the default agent, and makes the controller pairable and
  discoverable, so a device asking to bond (`RequestAuthorization`) or a
  bonded device asking for a profile (`AuthorizeService`) raises
  `Allow '<name>' to pair?` / `Allow '<name>' to use <profile>?` on the
  panel. `y`/`Enter` allows and `n` refuses — refusing fails the request on
  the BlueZ side rather than letting it quietly succeed — while `Esc` closes
  the window, and answering leaves it armed for the device it bound to.
  Well-known profile UUIDs are named. This is off by default and has no
  persistent form: with no window armed, BlueZ refuses such requests and the
  controller is neither pairable nor discoverable. The window binds to the
  first device that rings it and refuses every other one,
  cannot coexist with a pairing session, and turns the adapter flags it raised
  back off when it closes. `get_bluetooth_pairing` grows a `kind` field
  (`outbound`/`inbound`) and reports a null address until a window binds.

- Pairing a Bluetooth device now finishes the job: once the bond lands, the
  `jwm-bridge pair` helper marks the device trusted and connects its
  profiles, so a headset plays and a keyboard types without a second trip
  through the picker, and so BlueZ accepts the device's own reconnections
  later. The connect is bounded by whatever is left of the session's wall
  clock and can never turn a successful pairing into a failure — the picker
  says `Connected <name>`, or `Paired <name> — not connected` when the
  profiles did not come up. `bluetooth_pairing_done` carries a new
  `connected` field.

- Bluetooth discovery and device listing now go through
  `jwm-bridge discover [seconds]`, which drives `Adapter1.StartDiscovery` and
  reads the whole BlueZ object tree in one round trip, instead of scraping
  `bluetoothctl` and then spawning one `bluetoothctl info` child per device.
  Rows gained the signal strength that path parsed and discarded: an unpaired
  device shows its RSSI in dBm, and devices in the same bond state sort by
  signal before name, so the device in your hand is at the top of a scan that
  found thirty nameless beacons. `bluetoothctl` remains the fallback when the
  helper is not installed, and repeated `s` now coalesces onto the running
  scan instead of stacking another one.

- The Bluetooth picker can now pair new devices: `s` runs a bounded discovery
  scan, and `Enter` on a device that was never bonded spawns the one-shot
  `jwm-bridge pair` helper, which registers a `KeyboardDisplay` BlueZ agent
  and drives `Device1.Pair`. The picker renders PIN entry (masked),
  numeric-comparison confirm (`y`/`Enter` vs `n`/`Esc`), and display-only
  prompts; `Esc` or closing the panel cancels the session, prompts time out
  after 25 s, and sessions are bound by a per-session cookie passed through
  the helper's environment. IPC grows the `bluetooth_pairing_prompt` /
  `bluetooth_pairing_done` commands, the `get_bluetooth_pairing` query, and
  the `bluetooth` event topic carrying `bluetooth/pairing_response`.

- A GNOME-style tags overview (`Alt+O`, `toggle_tags_overview`) shows every
  tag of the current monitor at once as a wireframe grid: each cell carries
  the tag's number and outlines of its windows, occupied tags (minimized
  windows included) draw brighter, and the tags currently on screen keep a
  persistent accent frame. Arrow keys walk the grid with edge clamping,
  `Return` jumps to the highlighted tag, a digit key jumps straight to its
  tag, and `Esc` or a second `Alt+O` closes without switching.
  `behavior.tags_overview_enabled` (default true) gates entry, and
  configurations snapshotted before the action existed gain the `Alt+O`
  fallback binding unless they already spend the chord. The grid is
  pointer-operable too: hovering a cell moves the highlight, clicking a cell
  jumps to its tag, and clicking the dimmed desktop around the panel cancels.
  Cells draw wireframes rather than live thumbnails.
- The tags overview grid moves windows with the mouse: dragging a cell's
  wireframe onto another cell moves that window to the cell's tag while the
  grid stays open, through the same code path as `Mod1+Shift+N` — a
  multi-tag window's mask is replaced, not merged. A cell's commit moved
  from press to release to make room for the gesture, so a plain click still
  jumps to the tag, while a release landing on the dimmed desktop or the
  panel's dead space settles nothing.
- The tags overview marks tags holding an urgent window with a small dot in
  `behavior.attention_color` at the right end of the cell's label band — the
  same token the urgent window's own border breathes in. The marker follows
  the status bar's urgent mask: an urgent window marks every tag it sits on,
  minimized and swallowed urgent windows still count, and sticky or all-tags
  windows mark nothing.
- Native shell cards are now fully pointer-operable on X11 and Wayland:
  hovering an interactive row shows a quiet preview cue, clicking runs the same
  guarded action as `Enter`, the wheel browses lists (and calendar months),
  and clicking the modal scrim backs out like `Esc`. Hit testing uses the
  compositor's actual animated card geometry, while headings, empty states,
  notification action strips and password prompts remain non-clickable.
- Direct DRM/KMS color delivery now inventories capture, session-lock, drag
  icon, cursor, and top/overlay layer surfaces as separate frame-tail classes.
  The same plan selects the conservative global-sRGB fallback and is exposed
  as `last_policy_decision.linear_tail_blockers`, so diagnostics report the
  observed remaining adaptation inventory instead of one aggregate boolean.
  `Some([])` records an evaluated clear tail while missing/null preserves
  unknown or non-applicable legacy status; entries are not presented as proof
  that one class caused the selected route.
- Linear-tail diagnostics now share a bounded typed classifier across the
  backend status and render-decision IPC. The IPC retains its compatibility
  observation while adding distinct unknown/clear/blocked/malformed state, a
  stable non-payload issue code, and total/known/future blocker counts. Valid
  future names remain forward-compatible; malformed and over-limit arrays are
  never mistaken for unknown legacy status or reflected unboundedly.
- Long native-shell lists now share desktop-standard navigation: `Home`/`End`,
  `Page Up`/`Page Down`, and backward `Shift+Tab`, including the launcher,
  Shell Hub, notification history and device/content pickers. Direct Unicode
  keysyms are accepted in text fields instead of being discarded at an ASCII
  gate.
- A windowed list in the shell — the launcher's matches, the notification
  history, the Wi-Fi/Bluetooth/clipboard/wallpaper pickers and the Hub itself —
  draws a scroll indicator in the card's right-hand margin. The window manager
  sends the compositor a slice of a longer list, and until now nothing on the
  card said so.
- A hairline separates a shell panel's list from its footer hint, so the line
  naming the keys stops reading as one more row.
- `jwm-remote` provides an authenticated JWM-to-JWM remote desktop MVP for
  x11rb/xcb X11 sessions. It captures the shared Composite overlay out of
  process, sends bounded JPEG frames, maps a native X11 viewer back into host
  coordinates, and uses XTEST for explicitly enabled input. The direct LAN
  mode is authenticated but deliberately documented as unencrypted/trusted-LAN
  only; loopback plus SSH is the confidential deployment path.
- `behavior.recording_max_height` caps the encoded height, scaling the capture
  down to fit and preserving aspect ratio. Every downstream cost scales with the
  pixel count, so capping a 4K display to 1080p cuts the readback, the pipe and
  the encoder to a quarter, and the downscale itself is free because the capture
  blit already resamples the region into the output. 0, the default, records at
  the captured resolution.
- Status bars show the focused window's desktop icon beside its title. JWM
  publishes the window's application identity in shared-memory protocol v14 and
  `xbar_core` resolves it through the freedesktop desktop-entry and icon-theme
  lookup; `visibility.client_icon` and `ModelConfig::resolve_client_icons` turn
  it off.
- The bar's layout menu offers every layout the running window manager has
  rather than a fixed three. Protocol v14 carries the layout count and the
  layout in use, so the menu also marks the active entry, drops entries a
  compositor cannot enter, and keeps a newer compositor's extra layouts
  reachable.
- Tag-driven release automation with quality gates, an installable bundle, a
  Git source archive, SHA-256 checksums, and artifact provenance.
- Tested versioned install, upgrade, rollback, and uninstall operations.
- Compatibility, upgrade, and release-process documentation.

### Changed

- A notification that replaces one still on screen now updates that card in
  place — same slot, same open spring, the countdown restarted on the new
  text, and a hovered card left frozen under the pointer — instead of
  stacking a second copy behind the first. A progress notification that
  updates ten times is one card again, not four cards and a stack that
  evicted everything else.

- The notification history is no longer written from the compositor thread.
  Every change (post, replace, dismiss, clear) is queued to a writer thread
  that folds whatever arrives inside one second into a single atomic write,
  and exit and restart flush what it still holds before the process leaves, so
  a notification posted a moment before a restart is in the file the next
  process reads. A burst now costs the disk one rename-and-fsync instead of
  one per notification and no frame waits on an fsync; the price of the window
  is that a crash can lose up to a second of history where a synchronous write
  lost none.

- An action key longer than 64 characters is dropped rather than kept. The key
  travels back to its sender verbatim in `ActionInvoked`, so a shortened one
  would name an action the sender never offered — and an unbounded key could
  push a single record past the per-record budget the persisted history is
  sized from, which cost the whole file at the next startup rather than that
  one notification.

- `get_idle_status` and the `idle/state` event report the timeouts the policy
  will act on rather than the numbers in the configuration file: a
  `behavior.idle_lock_secs` below the 60-second floor is reported as 60, and
  `screen_off_secs` is `0` whenever `idle_screen_off_command` is empty, however
  the timeout is set. A bar counting down to the lock now counts down to the
  lock that will actually happen; the query is no longer a way to read the
  configuration back. `jwm --check-config` also checks the two idle keys it
  had been silent about — a lock timeout below the floor, and an
  `idle_dim_level` outside `[0, 1]`, which the dim stage replaces with 0.35.

- `get_power_status` and `get_audio_devices` answer from the control center's
  cached snapshot instead of forking `powerprofilesctl` or `wpctl status` on
  the frame thread, so a bar polling them can no longer stall a frame; a read
  also warms that snapshot on a worker, at most once every two seconds. Both
  payloads gained the marker that makes an empty answer unambiguous:
  `profile_pending` beside a null `profile`, and `pending` beside two empty
  device lists. True means "nothing has been read yet"; false means this
  machine really has no such control.

- **Breaking (diagnostics):** the colour-session policy object stopped
  hardcoding its HDR answers. `hdr_active` is now the last successful
  presentation's own report rather than a constant `false`,
  `hdr_active_semantics` says so, `hdr_signalling_enable_available` is derived
  from the per-output gate (and stays false where no gate reports in, which is
  not an availability claim), the fixed
  `hdr_signalling_enable_unavailable_until_external_elements_adapted` blocker
  is replaced by the gate's own per-output reasons in a new
  `hdr_enable_refusals` array, and the `limitations` list dropped three
  entries it had been contradicting since absolute luminance, the
  surface-description commit latch and atomic KMS colour delivery landed. Per
  output, `color_policy.selected_transfer_function` / `selected_primaries`
  now report the profile the display is actually being told instead of a
  literal sRGB, and `colorspace_signal` reports `bt2020_rgb` rather than
  `hdr_metadata_unspecified_colorspace` — the request always carried
  BT2020_RGB.

- **Breaking (diagnostics):** `render_decisions` is schema version 2.
  `tearing.active` now reports whether a frame was actually flipped
  asynchronously; under version 1 it was literally "a client asked", which
  made demand indistinguishable from an outcome — and since JWM does not
  tear, it reported `active: true` for something that never happened. Client
  demand moved to `tearing.client_demand`, `tearing.reason` carries a named
  blocker, and `tearing.outputs` plus the new `vrr` block report per output.
  `get_tearing_hints` keeps `active_surface_count` and gains the same
  per-output rows.

- VRR on the Wayland/KMS backend used to be written directly onto the CRTC
  `VRR_ENABLED` property, both unconditionally at output init and through
  the backend's `set_vrr_enabled`. Neither survived: Smithay re-asserts its
  own cached VRR value in every atomic request it builds, so the property was
  reset by the very next page flip while the call reported success. Both
  paths now go through Smithay's `use_vrr`. `set_vrr_enabled` fails
  explicitly on connectors where VRR would require a modeset, rather than
  silently taking a path that turns the next frame into a full commit. (No
  IPC command exposes `set_vrr_enabled`; an earlier version of this entry
  said otherwise.)

- Native X11 clipboard workers now block on the X socket plus an internal
  eventfd instead of waking on a 20 ms polling cadence. Their final backend or
  remote-half lease performs a bounded, joined shutdown and hands current text
  or PNG targets to an existing `CLIPBOARD_MANAGER` through `SAVE_TARGETS`,
  continuing to serve direct and INCR requests until the manager acknowledges,
  takes ownership, and every active chunk transfer finishes. With no manager,
  shutdown remains immediate; retained screenshot sender clones cannot delay
  it. Recoverable XCB requestor protocol errors also no longer kill the worker.
- The native X11 clipboard owner now completes the required ICCCM metadata
  contract (`TARGETS`, `TIMESTAMP`, and ordered `MULTIPLE`), acquires ownership
  with a real server timestamp, validates request epochs, and sizes direct and
  INCR writes from each server's request limit. Clipboard-history reads use a
  fresh requestor window per owner generation, fail closed when target atoms
  cannot be classified, and fully drain bounded incoming INCR transfers, so a
  late old-owner reply cannot cross into a password-manager offer or leave its
  source blocked. Stalled outgoing transfers also have per-requestor and total
  byte budgets and fall back to the idle polling cadence after their initial
  active burst.
- X11 screenshot copies are now owned and served by JWM itself instead of
  `xclip`. Both x11rb and XCB advertise `image/png`; payloads above the direct
  property limit use a bounded ICCCM INCR transfer with a real byte-count
  announcement, delete-driven chunks, and a zero-length terminator. This fixes
  wide-region PNGs crossing xclip 0.13's roughly 1 MiB failure boundary, and
  clipboard staging files are private and removed once their bytes are held in
  memory.
- X11 compositor OFF/ON is now a checked presentation hand-off instead of a
  renderer-only toggle. Before native exposure JWM flushes real borders and
  the current physical animation geometry; on re-enable it restores borderless
  input geometry and replays monitor, window-group, urgent, PiP, HUD,
  magnifier, peek and Night Light state. Compositor-only modal modes are
  quiesced before teardown, and failed transitions — including partial enable
  failures after presentation reconciliation — are reported to IPC callers.
  If such a partial enable was requested for a native-mode shell panel, the
  panel now adopts the already-running renderer as its temporary lease so its
  close/error path always restores compositor OFF instead of leaking it ON.
  `JWM_COMPOSITOR` keeps precedence across config reloads. `get_status`
  now reports actual, configured and temporary-lease compositor state without
  requiring callers to infer it from optional metrics, and retains the last
  hand-off target, attempt time, outcome and error even when no renderer
  exists to publish metrics.
- Native X11 interaction now has shared, overflow-safe move and eight-edge
  resize geometry in both XCB and X11RB, including fixed opposite edges and a
  1 px minimum. Unfocused urgent windows receive an attention border without the
  compositor, and the layout picker can temporarily lease the renderer while
  preserving a silent-cycle fallback. Runtime X11RB overlay hit testing and
  HDR setup now follow compositor recreation; XComposite teardown also releases
  the overlay by its root and cleans a failed selection-owner acquisition.
- Idle native X11 sessions now wake on counted readiness from IPC, status-bar
  commands, shell background jobs and clipboard captures instead of polling at
  20 ms. The old safety cadence remains automatic whenever a compositor is
  active or any notifier cannot be created, registered, drained or signalled.
  A status-bar bridge worker now revokes its health promise and sends one final
  eventfd wake on every exit path, including unwinding, so JWM immediately
  removes the dead notifier and restores the safety cadence. Bar frontends
  also replace an unhealthy notifier within the same transport generation,
  instead of waiting for a reconnect before event-driven updates recover.
- Native window admission parks new clients beyond the complete desktop with
  overflow-safe geometry, applies Motif/GTK client-decoration ownership before
  first map and reconciles it live, and keeps Dock/Desktop windows borderless.
  Smart borders and fullscreen round-trips no longer resurrect a server frame
  around client-decorated windows. Strut ownership is recomputed when a panel
  moves between outputs or the monitor topology changes, rather than retaining
  a stale output key.
  Floating ConfigureRequest coordinates remain root-relative on offset and
  negative-origin outputs, requested dimensions are bounded to the X11 wire
  range with overflow-safe workarea clamping, and native border changes are
  committed to the server without letting CSD, popup, Dock or Desktop windows
  regain a WM border. Explicit taskbar, launcher and IPC restores also reject
  stale layout-generated `EnterNotify` focus for a bounded 300 ms window, so
  focus-follows-mouse cannot immediately undo a successful activation.
  Multi-output struts are calculated from all four desktop edges with bounded
  arithmetic, so right/bottom panels and hostile properties cannot collapse a
  work area below one pixel.
- The nested X11 smoke matrix now performs two real compositor OFF/ON cycles
  for both X11RB and XCB, checking transition telemetry, JWM identity and the
  independently discovered native XID at every phase. Two consecutive probes
  require the X window to remain `IsViewable`, keep exact server geometry and
  expose non-black XWD RGB pixels; decoding follows the visual masks and byte
  order so alpha/unused bytes and row padding cannot fake visibility. The
  Xephyr harness reserves only displays with no lock or socket, confirms
  readiness through a private `-displayfd` pipe, shuts the server down
  gracefully and never guesses ownership or unlinks shared X11 artifacts.
  The diagnostic restores the compositor state it found so later lifecycle,
  screenshot and policy steps remain isolated.
- Explicit non-D65 surface and output descriptions now pass through a Bradford
  chromatic-adaptation transform between RGB→XYZ and XYZ→RGB. Neutral colors
  remain neutral across white points, D65 sRGB/BT.2020 matrices retain their
  existing identity-CAT path, and invalid or degenerate explicit primaries are
  rejected before they can inject unsafe color uniforms or KMS CTMs.
- The XCB transport now requires `xcb` 1.7.1, removing its vulnerable
  build-only `quick-xml` 0.30 dependency. The locked graph reuses the
  security-fixed 0.41 line already present in the workspace.
- Handler maintenance now has one exact wakeup contract shared by
  `needs_tick()` and `next_wakeup()`. Config debounce, layout picker, hidden
  parking, deferred grabs, transient-child insurance, scratchpad expiry,
  layout persistence, secondary-bar health/map/retry, ping send/timeout, idle,
  resources, and battery polling all contribute bounded deadlines with exact
  equality semantics. Battery/resources and Wi-Fi/Bluetooth job adoption no
  longer depend on an active compositor, preventing permanent zero deadlines
  in headless mode. X11RB/XCB keep their Timer in a calloop Dispatcher: events
  may only pull an existing deadline earlier, while a completed update may
  reset it, and vblank-blocked updates do not sleep a second frame. The 20 ms
  safety cap remains until clipboard and generic worker completion gain
  readiness notifications.
- JWM now exposes one process-lifetime epoll readiness descriptor to X11RB/XCB.
  It aggregates the IPC listener/dynamic clients and every monitor bar's
  command notifier without re-registering calloop. IPC fairness continuations
  and slow-client `EPOLLOUT` remain level-triggered, while a direction-aware
  xbar worker bridges each existing futex command queue to eventfd; status-bar
  clicks therefore wake JWM immediately instead of waiting up to 20 ms. Bar
  command bursts are bounded per monitor so a continuously refilling producer
  cannot monopolize one update. Retirement unregisters the source, destroys
  the owner ring to wake its futex, and joins the worker before fd reuse. The
  timer fallback remains for lifecycle and telemetry work that has not yet
  gained an exact deadline or notifier.
- X11RB and XCB no longer conflate JWM state-machine work with compositor-only
  damage in one 1 ms polling branch. Layout/overview/expose animations and
  deferred grabs now drive the shared update timer at 16 ms. X damage still
  renders immediately after event dispatch, while continuous compositor work
  is frame-paced instead of polling at 1 kHz. Idle maintenance remains at 20 ms
  until the remaining maintenance work gains deadlines/notifiers, avoiding the
  previous millisecond spin without delaying DamageNotify or recording. A
  timer-driven handler frame is also no longer followed by a redundant second
  compositor swap in the same dispatch.
- The udev Wayland backend no longer wakes every 16 ms just to discover that
  no shortcut is repeating. A repeatable press now arms one 400 ms timer and
  reuses it at the exact 50 ms interval; release, a new key, exact modifier
  mismatch, config replacement, built-in/protocol lock, or VT pause removes or
  invalidates it. Repeat timestamps follow real elapsed time with Wayland's
  wrapping u32 semantics, and successful config reloads refresh the backend's
  suppression/repeatability snapshot before the next physical key is routed.
- Wayland no longer wakes every 50 ms to clone every role root and traverse
  all surface trees for commit-timing barriers that cannot exist in the
  configured unmanaged protocol mode. Initial xdg-toplevel liveness is now a
  per-window 250 ms one-shot timer shared by udev and both nested backends;
  normal configure or destroy consumes its token and the later callback is an
  O(1) no-op.
- New configurations and the installer now agree on `tao_glow_bar` as the
  default status bar.
- The Shell Hub's first frame no longer waits for a chain of audio, backlight
  and power-profile tools. Slow controls now use one coalesced two-second
  stale-while-revalidate snapshot; panel opens and unrelated media,
  notification, clipboard, battery and connectivity rebuilds are memory-only.
  A worker result is epoch-guarded against newer user adjustments, async row
  insertion restores the selected `ControlKind`, and PipeWire output/input
  defaults share one `wpctl status` read instead of two. Passive Hub openings
  also reuse an in-flight connectivity read rather than detaching another
  nmcli worker.
- Opening the application launcher no longer recursively reads every desktop
  entry and executable on JWM's compositor/input thread. The catalog is built
  on a worker, shared as an immutable snapshot, reused for five minutes, and
  refreshed stale-while-revalidate; the first opening appears immediately with
  an indexing row. Application name sort keys are also precomputed, removing
  lowercase `String` allocations from every comparison on each keystroke.
- Native shell panels and the layout filmstrip now target the selected
  monitor's global viewport. Their card, scrim, bar dock and pointer hit-test
  use the same offset-aware geometry, including negative monitor origins; the
  lock screen deliberately remains full-virtual-desktop.
- System-UI and toast text is pixel-fitted with the configured font before CPU
  rasterization and GL upload. Long rows get an ellipsis, long queries retain
  the caret end, and a narrow nested output now overrides the normal desktop
  card-width floor instead of placing controls off-screen.
- `animation.enabled = false`, `animation.speed = "instant"`, and a zero
  duration now also snap compositor-owned shell cards, row highlights, toasts,
  OSD and HUD geometry. Their springs settle internally, so reduced motion
  does not keep requesting invisible follow-up frames.
- `tao_pixels_bar` switches from portable emoji to private-use
  Nerd Font icons only when the selected family is actually installed, and
  passes that exact canonical family to Cairo. Missing or invalid icon fonts
  now retain readable emoji instead of tofu or unrelated fallback glyphs.
- Several steady-state hot paths now avoid work whose cost scaled with frames
  or window count: KMS reuses cached output names; JWM-owned child status is an
  immediate SIGCHLD path plus a one-second fallback rather than one `wait` per
  child per update; missing `WM_NORMAL_HINTS` is negative-cached; and unchanged
  system-UI frames share their overlay and skip text joining/cache-key builds.
- The shell panels are mutually exclusive. `Alt+F10` pressed on top of
  `Alt+F9`'s calendar now closes the calendar and opens the Shell Hub in its
  place, instead of the press going nowhere; the same holds for every panel key,
  in both directions, and from the IPC socket. The keyboard and pointer grabs
  and any temporarily leased compositor are handed straight over rather than
  released and retaken, so the swap costs a frame instead of parking every
  hidden window twice and resetting the compositor's runtime state. A panel that
  cannot open — no `nmcli` for the Wi-Fi picker, clipboard history switched off,
  one output for the display layout — leaves the panel you had on screen. Each
  key still toggles its own panel off, and nothing at all replaces the lock
  screen.
- The modal shell card holds a stable width. The launcher re-measures its match
  list on every keystroke, and the card used to resize under the typing; it now
  only ever grows while a panel is open, in fixed steps, and starts over when
  the panel is replaced or closed.
- The shell card's selection highlight springs between rows instead of
  teleporting, and is placed rather than slid on a freshly opened panel so it
  never travels in from a row of the list it replaced.
- Every theme's footer hint is now held to WCAG's 3:1 contrast floor and the
  typed query line to the 4.5:1 body ratio. The default `glass` theme drew its
  hint at 1.6:1 — the least legible text on the panel was the line naming the
  keys — and its `hint_ink` has been darkened accordingly.
- A second `jwm-remote` viewer is told why it was refused instead of waiting
  out a timeout. The session ran inline in the accept loop, so a second
  connect completed into the backlog and then heard nothing at all: it waited
  five seconds and reported a bare end-of-file, indistinguishable from a wrong
  address, a firewall, or a dead host. The session now runs on its own thread
  and an extra peer is authenticated, sent `another viewer is already
  connected to this host`, and disconnected — measured at 16 ms. Authenticating
  first is deliberate: only a peer holding the key learns a session is in
  progress.
- The `jwm-remote` viewer forwards a single press of its grab-release key
  instead of swallowing it, releasing only on a second press within half a
  second. With `--grab-input` that key could previously never reach the remote
  machine at all, so an application there that wanted F12 simply could not be
  driven. `--escape-key` takes any X keysym name — `Pause`, `Scroll_Lock` — for
  keyboards without a usable F12, and an unknown name now fails immediately
  rather than after the connect and handshake had already succeeded.
- `jwm-remote host` can share one monitor or a fixed rectangle instead of the
  whole root, with `--monitor NAME` or `--region WxH+X+Y`. The X root spans
  every display, so a dual 1920x1080 desk is one 3840x1080 drawable and the
  default `--max-width 1280` delivered each monitor at 640x360; on a triple
  head, 426x240. `--monitor` is re-resolved by name whenever the layout
  changes, so unplugging or rearranging displays moves the shared area with
  them, and a name that does not exist fails at startup listing the names that
  do rather than when a client finally connects. Input and the host cursor are
  translated by the area's origin, so sharing the right-hand monitor no longer
  lands every click on the left one. Verified pixel-for-pixel against the live
  root: a region captured at +1200+300 differs from that crop by 0.4 mean
  absolute value, which is JPEG loss, against 2-13 at any other offset.
- `jwm-remote` can share the clipboard in both directions, off unless the host
  passes `--allow-clipboard` *and* the viewer passes `--clipboard`: the host's
  flag is policy and the client's is consent, and a session where either is
  missing never sends a clipboard record. UTF-8 text only, capped at 256 KiB,
  and text a password manager marked secret is never shared — the same rules
  JWM's local clipboard history already applies, because this reuses that
  implementation rather than growing a second one. Each side watches CLIPBOARD
  on its own X connection and thread, so a slow or hostile selection owner
  cannot delay a frame or an input event.
- `jwm-remote` viewers report their window size and the host stops encoding
  pixels the window cannot show. A 640-wide viewer previously received a
  1280-wide image and discarded half of it on arrival. `--max-width` becomes a
  ceiling rather than a target: a request may only narrow it, so a peer can
  never make the host spend more readback, encode time or bandwidth than the
  operator allowed. The encode is fitted inside both the viewer's width and its
  height, whichever binds harder: clamping width alone made one `--max-width`
  mean very different amounts of work depending on monitor arrangement, since a
  2560x2880 stacked root became 1280x1440 — 2.7 times the pixels of a
  side-by-side root at the same flag — and a portrait root was barely clamped
  at all. Measured on a 3440x1440 host as a viewer resized from 1280 to 640
  wide: capture 18.3 -> 9.5 ms per frame, encode 2.2 -> 0.6 ms, capture-to-ACK
  4.9 -> 2.4 ms.
- The `jwm-remote` viewer no longer clears its whole window before every frame.
  The backing pixmap is retained between frames and an upload only touches the
  image rectangle, so the bars around it stay correct until the letterbox
  itself moves; the per-frame fill also blocked on a synchronous round trip.
  Viewer draw time fell from 7.8-8.1 ms to 6.0-6.6 ms at native resolution.
- `jwm-remote` captures when the X server reports a change rather than on a
  fixed timer; `--fps` became a rate limiter instead of a schedule. The loop
  slept blindly to its next grid point and never watched the X connection, so
  at the default 12 FPS every interaction waited a mean of 42 ms for a tick
  that had nothing to do with it. The wait is bounded by one frame interval, so
  a missed notification degrades to exactly the old cadence rather than
  stalling, and events x11rb buffered while a capture waited on its own reply
  are drained before the descriptor is polled. Measured on a live 3440x1440
  session, `scheduled` fell from a constant 60 per five-second window to 21-51,
  following real screen activity.
- `jwm-remote` host telemetry leads with the capture paths actually in use —
  `mode overlay/xrender/shm/damage/cursor-events` — and announces every
  transition as it happens. Four separate facilities can degrade themselves at
  runtime, each of them expensive, and each previously announced only once by a
  line that had long since scrolled away.
- `jwm-remote` now encrypts the session, not just authenticates it. Screen and
  input payloads are sealed with ChaCha20-Poly1305 (transport version 2). This
  mattered most for input: a key press is a three-byte record, so a passive
  listener on the LAN could previously reconstruct a typed password, SSH
  passphrase or 2FA code byte for byte with no image analysis. The handshake
  gained magic and a version field, and both proofs and both traffic keys are
  now derived over the complete transcript, so rewriting the version changes
  every derived secret and fails closed instead of negotiating weaker terms.
  Small records are padded to 64/256/704-byte buckets, because length alone
  otherwise distinguishes a keystroke batch from a pointer run from a
  release-all; inter-keystroke timing is still not hidden. There is no forward
  secrecy — a leaked key file decrypts recorded sessions. The 16-byte tag is
  smaller than the 32-byte HMAC it replaces, and each record is now assembled
  in one reusable buffer and written with a single `write_all` instead of three
  unbuffered socket writes. Measured at native 3440x1440, the largest payloads
  this carries, the telemetry `write` stage was unchanged at 0.0-0.1 ms.
  ChaCha20-Poly1305 is pure Rust: no C toolchain or system dependency is added.
- `jwm-remote` video is now dirty-tile delta coded instead of whole-frame JPEG,
  which is application protocol version 4 (update both machines together). Each
  frame ships only the 16-pixel tiles that differ from the pixels the viewer was
  last sent, packed into one atlas image and encoded once. Measured on a
  3440x1440 desktop over loopback: 10.1 -> 0.5-0.9 Mbit/s at the default
  `--max-width 1280`, and 53.3 -> 1.8-2.6 Mbit/s at native resolution. Encode
  time fell from 60 ms to 1.5-2.7 ms per native frame, removing a ceiling that
  had capped the achievable rate near 16 fps regardless of link speed, and
  capture-to-ACK fell from 80-89 ms to 12-14 ms. Comparing against the last
  transmitted pixels rather than the previous capture means the small tolerance
  that absorbs scaler dither cannot accumulate into visible drift. Host
  telemetry gained `keyframes` and `tiles A/B (P% dirty)`.
- `jwm-remote host --once` returns the session's own result, so a scripted run
  that failed is distinguishable from a clean one by exit code.
- `jwm-remote` backpressure refresh is no longer clamped up to a fixed 250 ms
  floor, which had added fifteen frame-times of staleness at `--fps 60`.
- `jwm-remote` now downsizes both Composite-overlay and root-fallback frames
  with XRender before readback, overlaps capture with JPEG/network sending
  through a one-frame latest-wins queue, reports stage latency and dropped
  stale frames, and enforces an absolute negotiation deadline on both peers.
  Root capture first uses `IncludeInferiors` to copy the root and its same-depth
  children into a full-size staging pixmap, then scales into the small readback
  target; it never creates a Render Picture directly from the root window.
  Staging is capped at 64 MiB, resize/topology races retry once, and allocation,
  request or extension failures retain the same-frame full-resolution readback
  plus CPU resize.
- The JWM remote application protocol is now version 3 and deliberately rejects
  version 2, so both endpoints must be upgraded together. Cumulative frame
  acknowledgements still cap video at two frames beyond what the viewer has
  actually drawn, and sustained backpressure throttles redundant X11 captures.
  Input now travels in authenticated batches of at most 128 operations and 641
  bytes. Adjacent pointer positions are latest-wins without crossing key,
  button or release-all edges; the host preflights each complete batch before
  queuing it in order and flushing XTEST once.
- Host and viewer now publish five-second and final pipeline telemetry without
  changing the wire protocol. Host reports remain live through zero-send credit
  or socket stalls and separate capture-mailbox replacement from the viewer's
  decoded-frame replacement. Cumulative ACK output distinguishes the one proven
  `drawn-acks` target from all `retired` credits and inferred
  `viewer-superseded` frames; capture/send-to-ACK timings end when the host
  receives the ACK, while the viewer separately reports decode, queue and draw
  time.
- Host JPEG quality now uses same-setting display ACK feedback without changing
  the wire protocol. The existing `--jpeg-quality` value is its upper bound;
  repeated ACK RTT, viewer-supersede and frame-credit pressure cause a
  multiplicative decrease, while an FPS-scaled healthy run of at least three
  seconds recovers additively by one. The default floor is 40,
  `--jpeg-quality-floor` adjusts it, and `--fixed-jpeg-quality` restores a
  constant quality. In-flight quality epochs prevent late old-setting ACKs from
  causing a second decrease, and payload size remains diagnostic rather than a
  discrete threshold.
- The remote host suppresses an exactly unchanged captured frame before JPEG
  encoding. It compares source and image geometry plus every RGB byte against
  the last fully written wire frame; suppressed samples consume no frame
  sequence, display credit or quality decision. A successful unchanged frame
  is still sent every four seconds, safely inside the viewer's shared video
  idle timeout, and telemetry separates `unchanged-suppressed` from successful
  `unchanged-keepalive` frames.
- Composite-overlay capture now uses XDamage notifications to avoid
  reading back an entirely static desktop on every scheduled tick. Damage,
  compositor-owner, geometry, cursor-shape and pointer-position changes still
  capture immediately, while a two-second forced refresh feeds the existing
  four-second unchanged-frame keepalive. Root mode does not negotiate XDamage,
  and unavailable or rejected Damage requests permanently retain the proven
  per-tick capture path without ending the session. Damage object creation and
  destruction remain checked; per-frame Subtract is queued before the
  synchronous readback ordering barrier, and a server rejection is drained as
  an asynchronous Damage error in the same frame to disable gating. Host
  telemetry reports event-suppressed ticks separately as `damage-skipped`
  rather than conflating them with capture-mailbox backpressure.
- `jwm-remote` uses MIT-SHM 1.2 file-descriptor segments for local X11 image
  readback when the server and transport support them. It reuses a bounded
  mapping and falls back to core `GetImage` on the same drawable and frame if
  shared-memory setup or capture fails.
- Host capture converts the common depth-24, 32-bpp little-endian TrueColor
  readback directly from native BGRX rows into owned RGB frame storage. The
  fast path checks the current image format and visual masks on every eligible
  window readback and respects native row stride; nonstandard visuals and
  formats retain the generic `PixelLayout` decoder.
- Remote X11 capture now caches the root geometry, compositor owner and XFixes
  cursor shape behind checked Core/RandR and XFixes notifications. Stable
  frames reuse cursor pixels and their scaled image while querying only pointer
  position. All source modes observe compositor-owner epochs so a restarted
  compositor receives a fresh capture-inhibitor notification, while Root mode
  never acquires the overlay. Resize and owner races are authoritatively
  reconciled and retried once before disabling XRender or falling back from the
  overlay, and each unavailable notification path independently retains its
  previous polling fallback.
- Host video records now have one absolute 10-second budget across every
  partial socket write and the final flush. A peer can no longer keep a frame
  sender alive indefinitely by slowly draining bytes and restarting the
  per-system-call timeout; any incomplete record fails closed and triggers the
  existing session/input cleanup path.
- The X11 remote viewer reuses its native upload buffer, writes the common
  depth-24/32-bpp little-endian TrueColor layout directly, and uploads that
  layout through a reusable MIT-SHM 1.2 file-descriptor segment when available.
  It waits for the matching completion event before reusing shared pixels and
  retries rejected uploads with core `PutImage` on the same frame. The viewer
  retains a fully presented backing pixmap for Expose events; nonstandard
  visuals keep the generic/core path, and resize bursts allocate only their
  final size.
- Enlarging the remote viewer no longer makes the client resample and upload a
  window-sized image for every frame. When both fitted dimensions upscale the
  encoded image, XRender 0.10+ retains and scales a source-sized server pixmap
  into the letterboxed backing pixmap. This improves large-window presentation
  without increasing the host's encoded resolution or network traffic;
  one-to-one/downscaled presentation and unavailable XRender retain the
  existing CPU upload path.
- The remote viewer no longer wakes every four milliseconds while idle. It
  blocks on the X11 connection, a video-receiver wake descriptor, and the next
  heartbeat, telemetry or deferred-key deadline, while checking x11rb's
  already-buffered event queue before sleeping. With input negotiated, normal
  window close flushes `ReleaseAll` before one authenticated `Close`;
  X11/network failures shut the transport down directly so host cleanup
  releases input. Session cancellation is idempotent and receiver-thread
  joining has a bounded wait.
- Remote JPEG encoding and authenticated record reads now reuse bounded
  per-thread payload buffers. JPEG bytes are written directly behind the frame
  header without an intermediate allocation, receive buffers are exposed only
  after their MAC succeeds, and sustained use of much smaller frames releases
  capacity retained by an earlier extreme frame.
- Remote JPEG decoding now writes ordinary RGB frames directly into a reusable
  client allocation instead of allocating a new decoded image on every frame.
  The viewer retains that lease only while it is the Expose/resize source; old,
  superseded, failed and closed-window frames return it to a best-fit pool of at
  most two free buffers. Each retained buffer is capped at 32 MiB and the pool
  at 64 MiB, while grayscale JPEGs keep their compatible conversion path.
- The shared-memory protocol is v14. JWM and every bar must be rebuilt and
  restarted together, which the existing layout/version validation enforces.
- `display::CANONICAL_LAYOUTS` is now the single source for JWM's layout ids,
  names, symbols, labels and cycle order; `LayoutEnum` derives from it.
- No stable release has been published. The root `0.2.0` manifest version
  remains a development version, not a support commitment.
- CI now treats Clippy correctness, suspicious, and performance diagnostics as
  errors and explicitly tests the Linux action and D-Bus provider adapters.

### Fixed

- A Bluetooth pairing helper that dies before its inbound window ever armed
  — no system bus, no adapter, BlueZ refusing the agent — no longer leaves
  you watching the sixty-second countdown for a window that never existed.
  The helper's `bluetooth_pairing_failed` report is now dispatched instead
  of falling through to "unknown command", closing the armed window
  immediately with the reason on the picker's status line. The report is
  scoped by the session cookie, and only an inbound window nothing has
  bound to yet can be ended this way.

- The scroll wheel now reaches the window manager on the Wayland backends.
  Wheel rotation arrives there as axis events, which were only ever forwarded
  to the focused client — so while a shell panel, the window switcher, or a
  screenshot selection held the pointer, the wheel did nothing at all. During
  those grabs the vertical axis now becomes the same button-4/5 presses X11
  delivers (one press/release pair per detent, with the remainder carried so
  a touchpad's smooth scroll still adds up to clicks): panels browse, the
  switcher steps, the calendar pages, and the screenshot stroke width
  adjusts, on every Wayland backend. The nested X11/winit backends also
  forward the wheel to clients now — it previously reached nobody there at
  all.

- A Bluetooth prompt BlueZ withdraws before you answer — the device gave up
  or its request was superseded — now leaves the panel immediately instead of
  lingering until the 25-second prompt timeout. The helper reports the
  cancellation over a new `bluetooth_pairing_withdraw` IPC command; the
  session itself (including an armed inbound window) lives on.

- A toast card no longer swallows the physical buttons whose evdev codes map
  to 4-7 on the Wayland backend. The WM treats those codes as the wheel —
  never a click, never a dismissal — but the backend was withholding them
  from the client underneath the card as well, so the press went to nobody.

- The IME popup positioning failure logs no longer warn once per frame for a
  persistently broken popup; each failure kind warns once per popup and may
  warn again after the condition clears.

- The window switcher takes the same pointer grab every other clickable panel
  takes, so `Alt+Tab`'s documented click behaviour is finally the real one: a
  left click on a row commits that window and any other press cancels. The
  panel draws its rows over the very windows they name, and with the pointer
  left free X11 delivered the click to the window under the row instead. A
  wheel scroll now browses the list rather than throwing the gesture away — a
  touchpad flick with the modifier still held is asking for the next row. If
  another client already holds the pointer the panel opens keyboard-only
  instead of refusing.

- A notification the 64-record history cap pushes out now emits its
  `NotificationClosed` with reason 4. Nothing else was ever going to close it
  — a toast reaching the end of its timeout closes no record — so a sender
  waiting on `notify-send --wait` waited for a row that had already gone, and
  a later `CloseNotification` for it found nothing to close.

- A middle or right click on a toast card dismisses the card instead of
  falling through to whatever is underneath. The stack docks exactly where a
  monitor's tab strip lies, so the press used to close or focus the strip cell
  hidden under the card. Only the left button invokes an action chip, and the
  wheel is not a click: it dismisses nothing and goes to whatever is below.

- The tags overview hit-tests the highlighted cell where it is drawn. That
  cell is painted lifted about its own centre while the press was resolved
  against the unlifted rectangle, so a click near the edge of the cell under
  the pointer landed on its neighbour or in the gap. Overlaps resolve in paint
  order, so the card drawn on top of a pixel is the one that answers for it.

- A sticky window's wireframe in the tags overview no longer arms a drag
  that could not mean anything — tagging it would rewrite a mask stickiness
  ignores and the next `view` would put back — so the press settles as the
  cell's click instead. The outline still draws, and goes live, in every cell.

- The tags overview follows the selected monitor. `focus_monitor` over IPC and
  activating a window on the other head both move the selection without
  arranging anything, and the open grid went on describing the monitor it was
  opened on while its hit-test read the live viewport.

- A lock the backend refuses because it cannot start a compositor at all is no
  longer retried every five seconds for the rest of the idle period, and a
  refusal whose cause the backend could not explain (a VT switch, DRM master
  briefly held elsewhere) is now retried on a budget of twelve attempts —
  about a minute, enough to outlast the transient causes and bounded for a
  machine where it will never work. Something that passes on its own, such as
  a panel holding the pointer grab, is still retried for as long as the
  session stays idle.

- The idle policy no longer switches the X server's own blanker off before it
  knows it has a clock of its own. A backend whose idle clock cannot be read
  used to end up with neither policy: JWM's stages never ran and the server's
  timer had already been disabled. A clock that stops answering mid-session
  also puts back whatever the policy had dimmed, rather than leaving the
  screen dark with nothing left to notice the activity that would undo it.

- An inbound Bluetooth request you allowed is no longer reported as refused
  when you then close the window: the outcome reads off what was actually
  granted rather than off "the user ended the session", which closing always
  makes true.

- The prompt for an incoming Bluetooth request names the device that rang it
  even in a crowded room. The name was looked up through the picker's
  sorted, 64-device list, and an unpaired device with no RSSI sorts into the
  tail — so the question that matters most was the one most likely to name a
  bare MAC address.

- Closing an inbound Bluetooth window puts BlueZ's adapter-wide
  `PairableTimeout` and `DiscoverableTimeout` back where it found them. These
  are persisted, machine-wide settings rather than per-client state, so
  shortening them to the window length and walking away cut every other tool's
  pairable and discoverable window on that machine to sixty seconds,
  permanently. The window still only ever shortens a value longer than itself
  (or the "forever" `0`), and never re-imposes one a previously killed helper
  had already shortened.

- CI runs the tests it was only compiling: the portal crate's unit tests now
  execute in the portal job, and the bridge job installs `dbus-daemon` and
  sets `JWM_REQUIRE_DBUS_DAEMON=1`, so the BlueZ tests that stand up a private
  session bus fail loudly when the binary is missing instead of skipping
  themselves and letting the job go green having run none of them.

- Full-screen screenshot IPC now reports the asynchronous contract explicitly
  as `{status: "queued", path}`. It returns an error when destination/staging
  preflight fails or the backend rejects submission; region capture also
  preserves errors from its full-screen fallback. Logs no longer describe an
  accepted asynchronous request as an already-saved image. KMS now queues
  consecutive requests in order, and all compositor PNG writers publish
  atomically without overwriting an existing destination or exposing a partial
  image.
- `jwm-tool wayland-status` exits non-zero when IPC and every compatibility
  query are unavailable. Complete and partial legacy probes now publish an
  explicit `probe` status, counts, failed query names, mode, and fallback
  reason. Missing/null/scalar payloads no longer count as successful data, and
  legacy servers that close or return old-format aggregate responses still get
  the bounded per-query fallback.
- The GTK/Relm status bars compile with protocol-v14 client icons again; their
  renderer now maps `ClientIcon` to its own stable CSS node class.
- The headless WaterLily test job no longer precompiles the unused GLMakie/GLFW
  stack, and `Random` is an explicit package dependency instead of an
  undeclared transitive import.
- The toolbar contact-sheet test now runs as a deterministic in-memory
  composition assertion. Shared-structure subprocess helpers dispatch through
  their parent tests' ordinary exact-test entries, eliminating the repository's
  remaining Rust `#[ignore]` entries without no-op helper tests or filesystem
  side effects.
- The lock card's advertised `Esc  clear` action now securely overwrites and
  clears the entered password and authentication message while keeping the
  session locked.
- Deleting `WM_NORMAL_HINTS` no longer leaves stale fixed/min/max constraints
  on an X11 client, and a client with no hints no longer incurs a synchronous
  property query on every arrange.
- The X11 backends no longer report a refused keyboard grab as a success. Both
  `grab_keyboard` implementations discarded the reply's `GrabStatus`, so the
  lock screen's "never display a pretend lock if the exclusive keyboard grab
  failed" guard could not fire.
- Opening a shell panel during a pointer drag cancels the drag instead of
  silently stealing its grab and leaving it armed. The panel's grab replaces the
  drag's and drops motion events, while the motion and button-release handlers
  both bail while a panel is up, so the drag was never committed or cancelled.
- Locking from the Shell Hub's session page no longer leaves the lock marked as
  a page the Hub can be backed out to.
- A transient readback failure inside a scaled `jwm-remote` capture no longer
  retires XRender for the whole session. The accelerated path reads its small
  target back itself, and any error from that read was reported as an XRender
  fault; losing the scaler makes every later frame read back the
  full-resolution drawable and resize it on the CPU, so one transient error
  bought roughly a hundredfold host-CPU increase permanently. Failures are now
  attributed to the stage that failed, and a genuine XRender rejection suspends
  scaling with a 1 s / 5 s / 30 s backoff instead of retiring it outright.
- A single unparsed X11 event no longer disables all four of `jwm-remote`'s
  event-driven capture caches at once. Unknown events are attributed to the
  extension owning their event code and demote only that facility; steady state
  had otherwise gone from one blocking round trip per frame to about five, with
  no path back. Events from extensions the capture connection never selected
  are tolerated in a run of eight.
- `jwm-remote host` declares a 1 KiB inbound record ceiling instead of the
  global 32 MiB frame limit. It only ever receives a hello, empty heartbeats,
  eight-byte acknowledgements and input batches, so an unauthenticated length
  field could previously make it reserve megabytes before any tag was checked.
- A single unauthenticated TCP connection that resets immediately no longer
  kills `jwm-remote host`. The accept loop took the peer address from the
  accepted socket rather than from `accept` itself, and a peer that sent RST
  first made that fail with `ENOTCONN`, which propagated out of the listener.
  Aborted connections, interrupted syscalls and descriptor exhaustion are now
  logged and retried too, so one packet from a port scanner can no longer take
  down the host a user was relying on to reach the machine remotely.
- `jwm-remote` releases held keys and buttons after 600 ms of controller
  silence instead of waiting out the eight-second session idle timeout. The
  host X server generates autorepeat, so a network partition with a key down
  used to type into whatever had focus for the full eight seconds.
- A mouse button the controller sends that the host's pointer map does not
  define — a 12-button mouse, or horizontal-scroll buttons 6 and 7 — is dropped
  instead of ending the session. Because an input batch is validated before
  anything is queued, failing it also discarded every pointer motion and any
  release-all in the same record.
- `jwm-remote` pins a button's physical number when the press is queued, so a
  pointer remap arriving mid-press can no longer release a different button and
  leave the real one stuck down on the host.
- `jwm-remote connect` bounds its TCP connect. Every later phase already had an
  absolute deadline, but a black-holed address burned the kernel's full SYN
  retry budget against a five-second negotiation budget.
- A Composite overlay readback failure now arms the same bounded retry as an
  overlay acquire failure. Re-acquisition otherwise waited for a
  compositor-owner transition that never arrives while the same compositor
  keeps running, so one transient error downgraded the session to ungated root
  capture — and with it the XDamage gate — for the rest of the session.
- Screen recording no longer freezes the desktop. The compositor used to write
  each captured frame — 8 MB at 1080p — straight into ffmpeg's 64 KiB stdin pipe
  from its render loop, so whenever the encoder fell behind, the one thread that
  serves input and repaints for every client parked inside `write`. Frames now
  go to a writer thread through a short bounded queue and are dropped when the
  encoder cannot keep up. Stopping a recording no longer waits for ffmpeg to
  rewrite the file for `+faststart` either.
- An active recording no longer pins the compositor into a continuous
  full-screen redraw. It now composites only on the frames it actually captures,
  and the X11 and Wayland event loops sleep until the next one is due instead of
  polling at 1 ms. The capture clock advances by a whole frame interval, so a
  30 fps recording samples at 30 fps rather than drifting toward 20.
- `get_recording_status` now reports what the recording is actually achieving,
  not just what it was configured for: frames captured, frames dropped because
  the encoder could not keep up, elapsed time, and the effective capture rate.
  A recorder silently running at a third of the requested rate used to look
  identical to a healthy one until the file was played back.
- Screen recording converts to NV12 on the GPU instead of shipping RGBA to the
  encoder, on both the X11 and the Wayland backend. A fullscreen pass packs the
  composited frame into a target laid out as NV12, so the readback, the copy out of mapped memory, the pipe and ffmpeg's
  read all carry 1.5 bytes per pixel instead of 4 — exactly 62.5% less, at every
  resolution — and the encoder needs no conversion pass at all. Measured over
  twenty seconds of continuously changing 1080p content, the encoder process
  fell from 301 to 84 CPU ticks. The vertical flip moved into the same pass, so
  `-vf vflip` is gone too. Drivers that cannot hold the packed target fall back
  to the previous RGBA capture.
- The mouse cursor is drawn into recordings on the GPU rather than blended into
  every frame on the CPU. The X11 recorder re-uploads the cursor image only when
  its shape changes rather than once per frame; the Wayland recorder, which has
  no cursor image to sample because KMS scans the real pointer out on its own
  plane, draws the same synthesised arrow it always has.
- Recordings are no longer colour-shifted in most players. Frames were converted
  with BT.601 and the file was tagged with nothing, so ffmpeg-based playback
  guessed BT.601 and looked correct while mpv, VLC and browsers applied the
  usual "HD means BT.709" rule and showed pure red as (255,23,0). The GPU
  conversion uses BT.709 limited range — verified bit-exact against ffmpeg's own
  conversion across primaries, black, white and grey — and the stream is now
  tagged to match.
- Hardware video encoding actually works now. `behavior.recording_encoder`
  defaults to `auto`, but the probe that was meant to detect NVENC asked it to
  encode a 64x64 frame — below NVENC's minimum frame size — so it failed on
  every machine that had a working NVENC and `auto` silently fell back to
  libx264. The VAAPI probe failed for its own reason: it fed a software frame to
  an encoder that needs a hardware one. Both probes now use a 256x256 frame and
  build the same hardware frame the real command does. On an NVENC machine this
  cut the encoder process's CPU by 80-92% (1483 -> 301 ticks over twenty seconds
  of continuously changing 1080p content, 733 -> 82 for an ordinary desktop).
- `-pix_fmt yuv420p` is no longer forced on the hardware encoders. They convert
  from RGB on the GPU, so naming an output format only inserted a CPU
  conversion pass in front of them; removing it measured 17% off NVENC's process
  CPU with byte-identical colour. The software encoder still pins yuv420p,
  without which libx264 negotiates the far more expensive yuv444p.
- The recording capture target is now 8-bit RGBA rather than the 10-bit format
  the blur pipeline uses. Frames are read back as 8-bit bytes and encoded to an
  8-bit stream, so the extra precision was being paid for and discarded, and a
  format-matched readback stays on the driver's fast path.
- Screen recording no longer recomposites and re-encodes a screen that has not
  changed. It captures when a client draws, when an animation runs, or when the
  cursor moves, and otherwise keeps the encoded timeline alive with a 2 fps
  heartbeat instead of 30 full-screen captures a second. On a 1080p30 recording
  this cut the compositor's CPU by 82% with a moving pointer and 90% on a still
  desktop, for a file with the same frame count, duration and contents.
- The pipe carrying frames to the encoder is widened from the default 64 KiB to
  1 MiB, which turns a 1080p frame from 127 blocking writes into 8 and leaves
  the encoder more slack before the recorder has to drop a frame.
- Capturing a recording frame no longer makes a synchronous X server round-trip
  for the cursor. `XFixesGetCursorImage` — the only source for both the cursor
  image and its true root position, since motion over a client window never
  reaches the window manager — now runs on a sampler thread with a connection of
  its own, and the capture path takes the latest sample without waiting. An
  unchanged cursor shape reuses its pixel buffer instead of reallocating per
  frame.
- Screen recording competes far less with the desktop for CPU: the software
  encoder runs at `veryfast` with half the cores rather than `medium` with all
  of them, and ffmpeg is started one nice level down. Its per-frame progress
  line no longer accumulates in `/tmp`.
- Starting a recording no longer re-probes the available hardware encoders and
  the ALSA demuxer on every keypress, and querying recording status no longer
  runs `ffprobe` on the window manager's thread once the output has been
  validated.
- Bar tag glyphs no longer depend on which font fontconfig happens to hand a
  private-use code point: an installed Nerd Font is named explicitly in the
  font description (configurable as `presentation.icon_font`). The gear and
  home tags previously resolved to Arial, which draws unrelated shapes there.
- The default Tao/pixels bar now activates a control only after a matching
  press and release on the same node, and follows JWM's authoritative bar
  height instead of leaving a four-pixel layout gap.
- Installed payload ownership and modes are normalized instead of preserving
  untrusted extraction metadata; path traversal, symlink, and special-file
  payloads remain rejected.
- Production X11 session entries no longer force the optional WaterLily layer
  onto shared `/tmp` test endpoints.

## Versioning note

The root `jwm`, `jwm-bridge`, `jwm-portal`, `shared_structures`, `xbar_core`,
provider crates, and each bar are separate SemVer components. A JWM bundle
records the exact set it contains; its tag does not replace component versions.

[Unreleased]: https://github.com/beamiter/jwm/commits/master
