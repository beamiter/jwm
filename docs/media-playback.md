# Music and video playback

JWM opens local music, videos, and its own recordings through a searchable
native library picker. The separate `jwm-player` helper owns an mpv playback
window and presents it as the **JWM Player** MPRIS service. The Shell Hub,
status bar, lock screen, media keys, and track-change OSD share the existing
[media controls](media-controls.md).

## Open a file

Press **Alt+F10** to open the Shell Hub, then **U** for Music or **V** for
Videos. Music uses U because M already mutes the selected sound control.
Type to search, select a row, and press Enter to play it. The picker searches
the user Music/Videos folders and the corresponding configured recording
output directory. Scanning happens in the background, with bounded traversal
and no following of directory symlinks.

| Picker action | Key |
| --- | --- |
| Play selected file | Enter |
| Play filtered results, starting at the selection | Ctrl+Enter |
| Add selected file to the current queue | Shift+Enter |
| Refresh the library | F5 |
| Return / close | Escape |

The bindable functions `music_player` and `video_player` open these pickers:

| Global shortcut | Opens |
| --- | --- |
| Alt+Ctrl+Shift+M | Music library |
| Alt+Ctrl+Shift+V | Video library |

Older configurations gain these defaults in memory when the action has no
existing shortcut and the key is free. Custom bindings and chord leaders take
priority; the shortcut viewer shows the effective fallback bindings. Existing
**Alt+Ctrl+M** audio recording and **Alt+Ctrl+V** clipboard bindings remain available.

The installer also registers **JWM Music** and **JWM Video** in application
menus and file-manager **Open With** lists. The source installer sets them as
the defaults only in JWM sessions through `jwm-mimeapps.list`; Ubuntu/GNOME
keeps its existing associations. Release bundles register the applications
without changing the user's MIME configuration.

For files outside the library, use the file manager or the command line:

```bash
jwm-player --music -- "$HOME/Music/First track.flac" "$HOME/Music/Second track.mp3"
jwm-player --video -- "$HOME/Videos/recording.mp4"
jwm-player --enqueue -- "$HOME/Music/Next track.opus"
```

Local `file://` URIs are accepted. Quote each path and use `--` before file
arguments. Paths are passed as arguments, never evaluated as shell commands.
The helper reuses one player per D-Bus session; ordinary opens replace the
queue, while `--enqueue` appends. With no files it opens or raises the player.

## Playback controls

The player is an ordinary Wayland/X11 window, so it can be tiled, floated,
minimized, moved between tags, or made fullscreen like other applications.
On first launch, Music opens a compact window and Video fits within 80% of the
screen. Music shows embedded artwork when available. Later opens reuse the
existing window size, preserving any resizing, tiling, or fullscreen choice.
The embedded JWM interface uses a dark canvas, a blue/violet light ribbon on
the empty screen, and compact controls over the picture. Drop local files onto
the window, click the central play button or folder icon, or press **Ctrl+O**
to choose files. The optional native chooser uses **zenity**, falling back to
**kdialog** when zenity is unavailable. If neither is installed, use drag and
drop, the file manager, or the library picker. The chooser runs asynchronously
and passes local filenames as arguments without a shell. Network playback is
not added by this interface.

The bottom bar provides play/pause, mute, draggable volume and seek controls,
elapsed/total time, subtitles, always-on-top, speed selection (**0.5×–2×**),
and fullscreen. Seek hover shows the target time; dragging keeps control even
when the pointer leaves the slider. The top bar provides **Open files**,
**Queue**, **Subtitles**, **Settings**, and minimize/maximize/close controls.
Drag the unused header area to move the borderless window, subject to the
compositor's normal window policy.

Queue entries can be selected directly, with paging for large queues; arrow
keys and Enter work while a panel is open, Page Up/Down change pages, and Escape
closes it. Settings includes hardware decoding, subtitle visibility/delay,
always-on-top, and playback statistics. **Top bar** and **Bottom bar** each offer
**Auto**, **Always**, and **Hidden** modes in Settings. Auto is the default in
both windowed and fullscreen playback: after three seconds the bars hide
independently, and moving to the top or bottom edge reveals the corresponding
bar. Hovering a bar, dragging a slider, or using its panel keeps it available.
Paused playback and the empty screen keep automatic controls visible.

Press **H** or click the crossed-eye header button to hide both bars manually;
**H** again restores their last visible modes. Manual hiding takes priority
over pause, the empty screen, and edge hover, and removes the hidden controls'
click targets. **Ctrl+H** cycles both bars through Auto → Always → Hidden.
These window shortcuts work in both JWM Music and JWM Video, including while
the controls are hidden. The Lua UI and vector artwork
are embedded in `jwm-player`, so installing or packaging the binary also
installs its UI; no third-party scripts or downloaded artwork are loaded.

| In the player window | Key |
| --- | --- |
| Play / pause | Space |
| Open local files | Ctrl+O |
| Hide / restore both control bars | H |
| Cycle bars: Auto / Always / Hidden | Ctrl+H |
| Seek | Left / Right arrows |
| Volume down / up | 9 / 0 |
| Mute | M |
| Previous / next queue item | `<` / `>` |
| Fullscreen | F |
| Queue panel | L |
| Subtitle selection panel | S |
| Cycle subtitle track / toggle subtitle visibility | J / V |
| Adjust subtitle delay | Z / X |
| Quit player | Q |

The keyboard's media keys and the Shell Hub's current-track row work through
MPRIS. The player is exposed as `org.mpris.MediaPlayer2.jwm`; when another
application is also playing, use the existing Players picker to select **JWM
Player**. The bridge's normal active-player ranking and pinning still apply.
Discovery queries at most eight players concurrently, with a one-second budget
per player and a three-second budget for the scan. An unresponsive pinned
player keeps its pin, so a delayed media-key request is not sent to another
application. Transport calls also stop waiting after one second.

IPC can open files without constructing a command string:

```bash
jwm-tool msg open_media --args '{"path":"/home/me/Music/track.flac","mode":"music"}'
jwm-tool msg open_media --args '{"paths":["/home/me/Videos/one.mp4","/home/me/Videos/two.mkv"],"mode":"video"}'
jwm-tool msg open_media --args '{"path":"/home/me/Music/next.opus","enqueue":true}'
jwm-tool msg media_control --args '{"action":"play_pause"}'
jwm-tool msg media_control --args '{"action":"seek","position_us":30000000}'
jwm-tool msg get_media_status
```

An open acknowledgement confirms that the launch request was queued, not that
a decoder has successfully played the file. Startup and playback errors are
reported through JWM notifications when its IPC endpoint is available.
Playback state and position come from mpv's reported state through MPRIS.
Unsupported or damaged media can still fail to decode even when its filename
appears in the library.

## Installation and requirements

The source installer builds and installs `jwm-player` together with
`jwm-bridge`, including debug builds and custom Cargo target directories:

```bash
bash scripts/install-jwm.sh
```

`scripts/package-release.sh` includes the helper and both desktop entries in
release bundles. The executable is installed to `/usr/local/bin/jwm-player`.
`scripts/git-update-all.sh` treats a missing player helper as an incomplete JWM
installation, so an existing compositor binary does not hide a missing player.

Playback requires **mpv** and a **D-Bus session**. Debian/Ubuntu's bootstrap
script includes mpv; for an existing installation:

```bash
sudo apt-get install mpv
```

mpv is a runtime executable, so it is not needed to compile JWM or the helper.
Missing mpv produces a playback diagnostic and an installer hint. JWM's window
management, screenshot, and recording features continue to operate.

Common MP3, FLAC, WAV, Ogg/Opus, M4A, MP4, MKV, and WebM files are discoverable;
actual codec support depends on the installed mpv/FFmpeg build. The helper
keeps media decoding and playback outside the compositor process. This first
version focuses on local files; it does not add streaming-service accounts,
network playlists, DRM playback, or library metadata editing.

## Checking a problem

Run `jwm-player --help` and `mpv --version` to check the installed executables.
Start a known local file in a terminal to see playback errors. If the window
plays but JWM shows no current track, check that `jwm-bridge` is running on the
same session bus and select JWM Player in the Shell Hub's Players list. A
request to raise an existing window remains subject to the Wayland
compositor's activation policy.

## Subtitles and decoding

Embedded subtitle tracks and matching local sidecar files (for example
`movie.mkv` plus `movie.en.srt` or `movie.ass`) appear in the Subtitles panel.
Select a track or Off; mpv handles subtitle timing and rendering. Sidecar
matching uses mpv's `sub-auto=fuzzy` in the media directory. No subtitle download
service is contacted.

Hardware decode starts with `hwdec=auto-safe`; mpv falls back to software when
no compatible decoder is available. In Settings, **Hardware decoding · Off**
forces software decoding for troubleshooting, and **Auto** retries automatic
selection. This label is the requested policy, **not proof that hardware
decoding is active**. Use mpv's
statistics (`I`) to inspect the active decoder and frame statistics. Decode and
all player UI rendering remain in the mpv child, outside JWM's compositor.

## Regression checks and desktop acceptance

```bash
RUN_JWM_MPV_TESTS=1 cargo test --manifest-path bridge/Cargo.toml --bin jwm-player
mpv --no-config --vo=null --ao=null --idle=yes --script=bridge/src/player/ui_test.lua
python3 scripts/test_player_ui.py
```

The Lua test checks paging, scaled hit regions, transport controls, slider
capture, speed selection, independent bar modes, automatic deadlines, edge
reveal, manual hide/restore, file chooser fallback/validation,
subtitle selection, decoder switching, UTF-8 labels, and ASS escaping.
The Python test starts a private Xvfb, a generated local video and SRT, and real
mpv; it checks queue
selection, subtitles, seeking/volume dragging, pause/mute, speed, fullscreen,
automatic/manual bar visibility in the real framebuffer, async file chooser
fallback, and continued playback after decoder-policy
changes. Chooser stand-ins run on a private PATH; no desktop dialog is opened.
It requires permission to create Unix sockets, plus Xvfb, ffmpeg,
libX11, and libXtst. It never connects to the user's display or sound output.
The Rust decoder tests cover play/pause, seek, volume, stop/resume, queue advance,
MPRIS ownership, and crash cleanup. Null sinks and Xvfb cannot establish GPU
compatibility, audible synchronization, or compositor integration quality.

On actual Wayland and X11 sessions, verify a long audio/video synchronization
sample, 1080p/4K playback with the statistics overlay, seeking while playing and
paused, HW Auto/Off, embedded and sidecar subtitles, fullscreen exit, window
moves/minimize/tag changes, and the Hub/status bar/media-key/OSD path. Kill the
mpv child during playback and confirm JWM remains responsive and a fresh open
restarts playback. Record GPU/driver, codec, dropped frames and observed sync;
do not substitute headless test success for these observations.
