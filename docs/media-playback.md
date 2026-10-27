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

The bindable functions `music_player` and `video_player` open these pickers.
They have no additional global shortcut by default, so existing user bindings
and **Alt+Ctrl+M** audio recording remain available.

The installer also registers **JWM Music** and **JWM Video** in application
menus and file-manager **Open With** lists. It does not change the default
application for any audio or video format.

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
Move the pointer over the window to show mpv's on-screen controls and seek bar.

| In the player window | Key |
| --- | --- |
| Play / pause | Space |
| Seek | Left / Right arrows |
| Volume down / up | 9 / 0 |
| Mute | M |
| Previous / next queue item | `<` / `>` |
| Fullscreen | F |
| Quit player | Q |

The keyboard's media keys and the Shell Hub's current-track row work through
MPRIS. The player is exposed as `org.mpris.MediaPlayer2.jwm`; when another
application is also playing, use the existing Players picker to select **JWM
Player**. The bridge's normal active-player ranking and pinning still apply.

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
bash scripts/install_jwm_scripts.sh
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
