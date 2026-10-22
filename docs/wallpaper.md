# Wallpaper picker

JWM uses a static 2D wallpaper by default (`behavior.wallpaper_effect = "off"`).
Celestial 3D wallpapers require selecting a scene in the picker or explicitly
setting `wallpaper_effect = "astronomy"` in `[behavior]`. To restore the 2D
wallpaper after enabling a 3D scene, set `wallpaper_effect = "off"`.

`Alt+Ctrl+W` (`wallpaper_picker`) lists the images in the wallpaper directory
and applies the one you pick. `Up`/`Down` select; `Enter` / left-click /
middle click applies an image and closes; `Esc` — or `Alt+Ctrl+W` again — closes
without applying the highlighted row. Controls apply immediately and remain in
effect when the panel closes. The wallpaper already in use is marked and starts
selected, so reopening the panel does not lose your place.

Type to search by filename, path, effect or control name. Matching ignores case,
supports Unicode, and requires every space-separated word to match. `Space` enters search
text; `Enter` or a click applies. `Backspace` edits the query, while `Home`/`End`,
`PageUp`/`PageDown`, arrows and scrolling navigate the filtered results. The title
shows the current row and match count. A directory scan completing while you
type preserves your query and a still-matching selection.
When different directories contain the same filename, those rows include their
parent paths so you can distinguish your image from a bundled namesake.

Press `F5` or choose **Refresh image library · F5** to rescan without closing
the panel. Refresh keeps the query, a still-present selection and the selected
display target. The current rows remain visible and searchable while the scan
runs; image application waits for the result. Repeated refresh requests share
the pending scan. This also retries a scan whose worker
could not start.
When that scan completes, an explicit refresh also re-reads the current static desktop
images, the highlighted preview and the wallpaper accent source. Replacing an
image at the same path therefore no longer requires switching to another image
first. Working textures and layouts remain visible until replacement loading
succeeds. Closing the picker cancels the pending refresh; merely opening it
does not force image reloads. Reapplying the current image or successfully
reloading the configuration also re-reads its contents.

The highlighted candidate is also shown: a thumbnail card sits to the right
of the list, vertically centered on it, and tracks both arrow-key and
pointer selection. Thumbnails decode asynchronously on the wallpaper
loader's worker pool, downscaled to a 480-pixel long edge. Only the latest
pending preview is retained; replaced requests are cancelled instead of
building an unbounded decode queue. The image is aspect-fit into a frame up to
480×360 and never upscaled. The frame shrinks to the available space and is
hidden only when there is too little room for a useful preview.
There is no placeholder or spinner: until the thumbnail lands — or if
the file cannot be decoded — the picker looks unchanged. A left-click or
middle click on the preview applies the highlighted wallpaper — the pointer
twin of `Enter` — and closes the picker the same way a list-row click does.

## Controls in the picker

Image controls stay open after applying and show
`Applied for this session · configuration file unchanged`. They use the live
configuration; reload or restart restores the values in your TOML file.

| Control | Result |
| --- | --- |
| **Global image sizing: Fill** | Cover the screen, cropping the image where needed. |
| **Global image sizing: Fit** | Show the whole image with margins as needed. |
| **Global image sizing: Stretch** | Match the screen dimensions, allowing distortion. |
| **Global image sizing: Center** | Keep native pixels and clip anything outside the screen. |
| **Global image transitions** | Toggle the configured image crossfade. Re-enabling a zero-duration transition restores 500 ms. |
| **Wallpaper accent colors** | Toggle future wallpaper-derived theme updates; disabling keeps the colours already applied. |
| **Rotate this image library daily · all screens** | Start daily rotation from the complete retained image list. |
| **Stop daily rotation · restore static wallpapers** | Return to the static image, monitor and tag settings. |

Global sizing supplies the default image mode; explicit monitor/tag modes keep
their own values. These image controls remain global when **Apply to** targets
one display. Choose **All screens** to apply an individual static image. Celestial
scene selection and its saved controls are described in
[Celestial 3D wallpapers](astronomy-wallpaper.md).

## Which directory

```toml
[behavior]
wallpaper_dir = "~/Pictures/Wallpapers"
```

Left empty (the default), the picker prefers the current wallpaper's folder
if it still exists. Otherwise it opens JWM's installed bundled collection,
then `~/Pictures/Wallpapers`, then `~/Pictures`. A leading `~` is expanded.
An explicit `wallpaper_dir` remains the primary directory, but the installed
JWM collection is also included in the picker, so choosing a custom folder does
not make the bundled wallpapers disappear.

`png`, `jpg`, `jpeg`, `webp`, `bmp`, and `gif` are listed, sorted
case-insensitively by name (names that differ only in case keep a fixed
order), and capped at 200 entries. Selection keeps at most 400 candidates
in memory while reading the directory, retaining the first 200 names in
global order even when better names arrive at the end of the scan, within the
100,000 raw-entry scan limit per directory. Bundled JWM images reserve up to
their own count of those 200 slots if the preferred directory is different.
Symlink aliases are deduplicated within each retained collection and across the
two collections. A bundled picture keeps its reserved slot even when its
preferred-directory alias would otherwise fall beyond the retained cutoff.
The current wallpaper is kept even if it falls outside those names or lives in
another directory; when necessary it replaces the last retained item without
exceeding the 200-image cap.
The status distinguishes an empty folder from a
read error, reports skipped entries and warns when a cap is reached. The raw
scan limit is tracked per directory, so two complete scans do not produce a
false warning simply because their combined entry count exceeds 100,000. Search
covers the retained image list (up to 200 images), plus the built-in effects;
it does not search beyond that cap or recurse into subdirectories.
Both directories are read on one worker thread:
the panel opens at once with a `Scanning…` status line and the rows fill in
when the listing lands, so an enormous pictures directory or a slow network
mount never stalls the frame that opened the panel. Choosing the directory
checks the candidates above on disk, so it runs on the same worker. If no
worker thread can be started, the status line reads
`Could not start the wallpaper scan` instead of staying on `Scanning…` for
good; `F5` or reopening the panel tries again. A panel closed before its listing
lands drops the answer, and each opening starts its own scan, so only the
newest one fills the rows. Only an image whose type the directory entry
cannot settle (a symlink, a filesystem without entry types) costs a `stat`;
a symlink to an image is listed, one to a folder or a dangling one is not.

## Bundled static wallpapers

The source tree contains 14 ordinary images directly under
`assets/wallpapers/`, without a nested collection folder. A successful full
`scripts/install_jwm_scripts.sh` install copies them to:

```text
~/.config/jwm/wallpapers/01-alpine-dawn.png
~/.config/jwm/wallpapers/02-ocean-aerial.png
...
~/.config/jwm/wallpapers/14-luminous-tide.png
```

With an absolute `XDG_CONFIG_HOME`, replace `~/.config` with that directory.
The target belongs to the login user even when JWM is built with `sudo`.
For a lightweight, build-free install or refresh:

```bash
bash scripts/install-wallpapers.sh
```

Only changed *managed* image files are upgraded: the installer records SHA-256
hashes in `wallpapers/.jwm-bundled-sha256` and never overwrites a same-named
custom file, a symlink, or an edited installed image. It does not delete unknown
files or modify `config_x11.toml` / `config_wayland.toml`. Older user collections
under `~/.local/share/jwm/wallpapers/jwm-14/` are deliberately left untouched;
existing explicit paths pointing there continue working while the files remain.
To switch an old persistent `wallpaper` or `wallpaper_rotation_paths` entry
to the new collection, change the paths in your own config.

Bundled static images are **not** enabled as daily rotation automatically.
The picker can apply a picture immediately, or start session-only rotation with
**Rotate this image library daily · all screens**. To persist a single image,
set `behavior.wallpaper` with `wallpaper_rotation = "off"`. To persist rotation
of the bundled collection, set `wallpaper_rotation = "daily"` and supply its
14 installed paths in `wallpaper_rotation_paths`. The same ordered collection
is used on both X11 and Wayland; connected displays receive consecutive offsets.

## How it applies

The choice runs the normal apply-config step. With daily rotation off it
updates `behavior.wallpaper` in the live configuration. With daily rotation
on it replaces today's global fallback without changing the rotation setting,
collection, or saved static wallpaper. Both compositors use that effective
choice through their usual wallpaper loading path. An X11 session
running **without** the compositor still shows that picture: it is decoded off
the event loop (Triangle-filtered to each output, not nearest-neighbour) and
installed as the root pixmap (`_XROOTPMAP_ID`), laid out per output with the
same `fill` / `fit` / `stretch` / `center` rules. Startup no longer paints the
root black first, so an existing `feh` / `esetroot` pixmap stays until JWM's
own wallpaper lands. Tiling gaps then match the composited desktop instead of
a solid black root. The default status bar (`tao_glow_bar`) frosts that same
root pixmap when it cannot be translucent. A `glass.wallpaper` path that
cannot be read still falls back to that pixmap. Monitor layout changes
recompose the picture; turning the compositor off restores native borders
and the themed root cursor instead of leaving overlay leftovers. Per-tag
pictures follow `view` / `toggleview` on the native root as well; a file
that changes on disk is picked up by its source identity, and translucent PNG pixels
blend over the theme letterbox. Setup and output layout changes install that
picture as soon as the monitors exist.

Native-root loading applies embedded image orientation and the shared source
limits (64 MiB file, 16,384 pixels per dimension, 256 MiB decoded/RGBA output).
It retains content-based format detection, including extensionless pictures.
File identity and nanosecond modification/change times detect same-size edits
within a second, atomic replacements and symlink retargeting. Sources decode
on first use and are released after their last output, preserving output draw
order without retaining every distinct source throughout composition.
File checks and canvas fills run in the background. Existing maintenance ticks
probe at most once per second; unchanged files skip decoding and composition.
Output/tag changes and explicit refreshes bypass that interval and merge into
the newest pending request. A running codec finishes its current call before
obsolete conversion/resizing is skipped.
If any source or output placement fails, the installed root picture stays in
place. Temporary decode, resize and pixmap-upload failures retry with a delay
starting at one second and doubling up to 30 seconds. Slow failed work starts
its delay when it finishes. File or wallpaper/layout changes, configuration
file reload and F5 reset the delay. A source is remembered as installed only
after the X11 upload succeeds, so one failed upload cannot permanently suppress
recovery.

A manual daily choice applies to all outputs using the global fallback;
explicit monitor/tag wallpapers retain their own pictures and modes. The
picker marks the manual choice when reopened. Rotation stays enabled: at the
next local date it returns to the existing automatic schedule (including its
normal priority over static overrides). A later manual choice replaces the
earlier one. Opening, browsing, and cancelling do not change today's choice.
The daily choice is session-only and is discarded on configuration-file
reload, restart, or disabling rotation; it is never written to TOML.

That also means the change is **in memory only**, like every other
`set_config`: it lasts for the session and is not written back to
`config_x11.toml`. For a permanent static wallpaper, set `behavior.wallpaper`
and `behavior.wallpaper_rotation = "off"` in the file. Daily choices leave
the saved rotation schedule untouched.

`behavior.wallpaper`, `behavior.wallpaper_mode`, `behavior.wallpaper_dir`,
`behavior.wallpaper_crossfade` and `behavior.wallpaper_crossfade_duration_ms`
are settable over IPC too:

```sh
jwm-tool msg set_config --args '{"key": "behavior.wallpaper", "value": "/srv/walls/alps.jpg"}'
jwm-tool msg set_config --args '{"key": "behavior.wallpaper_mode", "value": "fit"}'
```

`wallpaper_mode` accepts `fill`, `fit`, `stretch`, or `center`, and rejects
anything else rather than silently falling back. The crossfade duration accepts
integer milliseconds from 0 through 30,000; zero is valid only when crossfade
is disabled.

### Image quality and transitions

The composited X11 and Wayland paths reuse recently decoded images in a bounded
64 MiB / 24-entry cache. Repeat requests share the resized pixels. Simultaneous
first loads of the same file version, mode and sampling size also share one
decode and resize, including when several outputs request that image together.
Changing the source file, mode or required sampling size obtains the appropriate
new image. Each output retains its own selection and transition.
Transparent PNG/WebP pixels are composited over black before filtering, avoiding
hidden-color fringes. These settings control successful image replacements,
including independent monitor/tag wallpapers:

```toml
[behavior]
wallpaper_crossfade = true
wallpaper_crossfade_duration_ms = 500
```

The old image keeps its own layout until the replacement is ready. Smoothstep
timing and complementary layer weights fade both the image and Fit/Center
margins. Failed decodes retain the working image. See
[Wallpaper pipeline and validation](wallpaper-pipeline.md) for cache limits,
source identity, resource lifetime and reproducible rendering checks.

## Daily rotation and different screens

Choose **Rotate this image library daily · all screens** after the library scan
finishes to start rotation immediately. It uses the full retained library,
including bundled images and the retained current image, up to 200 entries;
the search query does not narrow the collection. Starting it disables a
celestial effect and clears today's manual daily choice. Refreshing the library
later does not change the rotation collection until you choose the action
again. **Stop daily rotation · restore static wallpapers** restores the static
selection while keeping the collection available in the live configuration.
Both actions are session-only. For a persistent schedule, edit TOML:

```toml
[behavior]
wallpaper_rotation = "daily" # "off" restores static wallpaper selection
wallpaper_rotation_paths = [
  "/home/me/.config/jwm/wallpapers/01-alpine-dawn.png",
  "/home/me/.config/jwm/wallpapers/02-ocean-aerial.png",
  "/home/me/.config/jwm/wallpapers/03-aurora-fjord.png",
]
```

The ordered collection advances by one image on each local calendar date.
Screens use consecutive offsets, so up to the collection's image count are
different at the same time. With 14 images the cycle repeats every 14 days;
more than 14 screens necessarily reuse images. The assignment uses current
output order, so connecting or removing a screen may shift its image.

Selection is applied on startup, configuration changes, and output changes.
The existing maintenance tick notices midnight or a clock/date correction
within about a second; no external timer or daemon is needed. Image loading
remains asynchronous on X11, Wayland, and compositor-off X11. During automatic
daily rotation, theme colors follow the first output's scheduled image.

Daily selection takes precedence over static `wallpaper`, `wallpaper_monitors`
and `wallpaper_tags`, leaving those values intact for when rotation is off.
The collection must be nonempty, contain unique nonempty paths, and have at
most 366 entries (each at most 4096 bytes). These two fields support atomic
IPC updates through `set_config_batch`; file edits are needed for persistence.

Inspect today's selected paths with:

```sh
jwm-tool msg get_config --args '{"keys":["wallpaper_rotation","wallpaper_rotation_paths","wallpaper_rotation_local_date","wallpaper_rotation_assignments"]}'
```

`wallpaper_rotation_assignments` reports the selected path and display mode
for each screen. It describes selection, rather than decoder completion.

## Colours from the wallpaper

Changing the wallpaper also retints what JWM draws in the accent colour: the
focused window border, both ends of the gradient border, and the client glow —
which are the same colours the launcher ring, the toasts, and the OSD progress
bars read, so the whole shell follows the picture.

The source is the first valid display in JWM's monitor order, using that
display's current tags, monitor override and daily/manual selection. Switching
its tags or changing the output layout re-resolves the source; moving focus to
another display does not retint the shell. With no displays, slot zero supplies
the fallback. Results from an extraction whose source was superseded are
discarded before they can update colours.

Wallpaper-derived colours are enabled by default. To disable them:

```toml
[behavior]
wallpaper_colors = false  # keep the colours set in this file
```

Each colour keeps its own alpha. The glow is deliberately translucent and the
border deliberately opaque; that is a taste the wallpaper has no business
overriding.

Like every other runtime change, the retint is **in memory only** — the config
file is never rewritten, so setting `wallpaper_colors = false` and reloading
puts your own colours back. Turning it off at runtime only stops *future*
wallpapers from retinting and leaves the current colours alone: undoing a
retint would mean remembering what you had, which the file already does.

### Which colours it picks

The image is decoded, scaled so its longest edge is 160 pixels, and its pixels
are counted into 4096 buckets (four bits per channel — fine enough to keep a
sunset apart from the sand under it, coarse enough that a gradient sky counts
as one colour rather than ten thousand).

For images with transparency, RGB is multiplied by alpha before shrinking and
recovered afterwards. Invisible RGB therefore cannot tint the sample. Pixels
below half coverage are skipped; the remaining pixels are weighted by their
alpha as well as saturation. This extracts the visible source hue without
assuming a fixed desktop background colour.

Buckets are ranked by area **weighted heavily towards saturation**. This is the
part that matters: the largest area in a photograph is usually sky, snow, or
asphalt, and almost never the colour anyone would name the picture by. In the
Yosemite valley shot in this repository, grey rock and pale sky cover most of
the frame and the accent still comes out sky blue with a forest-green second
stop.

Two more rules earn their keep:

- **Near-black and near-white pixels are dropped**, and the accent's lightness
  is clamped into a middle band. A border has to be visible against a dark
  panel and a light one; a wallpaper that is almost black must not produce a
  border that cannot be seen.
- **A monochrome wallpaper stays monochrome.** A black-and-white photograph
  quantises to hues that are pure rounding noise, and turning that noise into a
  red or green accent looks like a bug, not a theme. Below a saturation floor
  the palette keeps its neutrality and only the lightness is normalised.

The second gradient stop is the highest-ranked colour whose hue is at least 25°
away from the accent. A wallpaper with only one real colour gets a stop derived
by rotating the accent 45°, so the gradient is still a gradient — and a grey
wallpaper, which has no hue to rotate, gets one separated by lightness instead.

When an image yields nothing usable at all — an all-black splash screen, a
fully transparent PNG — the configured colours are kept rather than replaced
with something arbitrary.

### Cost

Decoding is the expensive part: roughly 90 ms for a 2560×1920 JPEG. It runs on
a worker thread and the result is adopted on a later frame, exactly like the
Wi-Fi scan, so the wallpaper appears immediately and the colours follow a
moment later. A successfully extracted wallpaper is remembered, so an unrelated
config apply does not decode the same picture again. F5, reapplying the current
image and configuration-file reload explicitly invalidate that memory and
extract the current file contents, including when its path has not changed.
If the worker cannot
start, decoding fails, or the image has no usable colour, the request is cleared
and the next config apply can retry the same path. Failure itself starts no
retry loop and keeps the existing palette. Clearing the wallpaper also discards
its pending extraction, so a late result cannot retint an empty selection.
Colour extraction uses the same regular-file, source-byte, dimension and
decoded-output limits as wallpaper loading, including orientation handling.
FIFO inputs are rejected without waiting for a writer.

### Over IPC

```sh
jwm-tool msg get_wallpaper_colors
# {"enabled": true, "wallpaper": "/srv/walls/valley.jpg",
#  "accent": "#83b5d0", "secondary": "#53a63f", "pending": false}
```

`wallpaper` is the picture the colours came from, which is not always the one
on screen: switching to a wallpaper with no colour to take leaves both the
colours and this field on the last one that had some.

`enabled` retains its historical meaning of wallpaper-derived accent extraction;
`colors_enabled` makes that meaning explicit. `image_enabled` separately reports
whether any output requests a static image, even when accent extraction is off.
`rotation` reports `off` or `daily`. `current_wallpaper` is the first output's
resolved image path; `current_wallpapers` lists each output's `monitor`, `path`
and `mode`, including tag overrides and daily offsets. With no outputs, the
singular path falls back to the slot-zero selection. These are resolved requests,
not confirmation of successful decoding or of visibility beneath a 3D scene.
The same fields appear in `get_status.wallpaper`.
`source_wallpaper` and `source_monitor` identify the currently resolved colour
source and its configured monitor index (`null` when no display exists). They
remain separate from `wallpaper`, which describes the last adopted palette.

A `theme/colors` event carrying the same payload is broadcast on the `theme`
topic when extraction starts, completes, fails or is cancelled. Failure ends
`pending` while preserving the previous palette, so subscribers can finish
their loading state without polling.
Successful re-extraction also emits the completion event when its colours are
unchanged.
Unrelated configuration applications that keep the same source do not emit
duplicate extraction-state events.

The four colours are settable directly too, as `[r, g, b, a]` in the 0..1
range, which is also how you put your own back without a reload:

```sh
jwm-tool msg set_config --args '{"key": "behavior.border_gradient_color_a", "value": [0.24, 0.65, 1.0, 1.0]}'
```

`jwm-tool health` prints separate `images`, `colors`, `rotation` and `pending`
values in its compact `wallpaper` line, and remains compatible with older JWM
responses.
