# Wallpaper picker

`Alt+Ctrl+W` (`wallpaper_picker`) lists the images in the wallpaper directory
and applies the one you pick. `Up`/`Down` select; `Enter` / left-click /
middle click applies and closes; `Esc` — or `Alt+Ctrl+W` again — closes
without changing anything. The wallpaper already in use is marked and starts
selected, so reopening the panel does not lose your place.

The highlighted candidate is also shown: a thumbnail card sits to the right
of the list, vertically centered on it, and tracks both arrow-key and
pointer selection. Thumbnails decode asynchronously on the wallpaper
loader's worker pool, downscaled to a 480-pixel long edge, so held-down
arrows never queue decodes — the latest highlight wins. The image is
aspect-fit into a 480×360 frame and never upscaled, and on outputs too
narrow to fit the frame the picker stays exactly the text list it always
was. There is no placeholder or spinner: until the thumbnail lands — or if
the file cannot be decoded — the picker looks unchanged. A left-click or
middle click on the preview applies the highlighted wallpaper — the pointer
twin of `Enter` — and closes the picker the same way a list-row click does.

## Which directory

```toml
[behavior]
wallpaper_dir = "~/Pictures/Wallpapers"
```

Left empty (the default), the picker looks beside the current wallpaper —
whoever set one almost certainly keeps the rest in the same place — then falls
back to `~/Pictures/Wallpapers`, then `~/Pictures`. A leading `~` is expanded.

`png`, `jpg`, `jpeg`, `webp`, `bmp`, and `gif` are listed, sorted
case-insensitively by name (names that differ only in case keep a fixed
order), and capped at 200 entries. Selection keeps at most 400 candidates
in memory while reading the directory, retaining the first 200 names in
global order even when better names arrive at the end of the scan.
The directory is read on a worker thread:
the panel opens at once with a `Scanning…` status line and the rows fill in
when the listing lands, so an enormous pictures directory or a slow network
mount never stalls the frame that opened the panel. Choosing the directory
checks the candidates above on disk, so it runs on the same worker. If no
worker thread can be started, the status line reads
`Could not start the wallpaper scan` instead of staying on `Scanning…` for
good; reopening the panel tries again. A panel closed before its listing
lands drops the answer, and each opening starts its own scan, so only the
newest one fills the rows. Only an image whose type the directory entry
cannot settle (a symlink, a filesystem without entry types) costs a `stat`;
a symlink to an image is listed, one to a folder or a dangling one is not.

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
that changes on disk is picked up by its mtime, and translucent PNG pixels
blend over the theme letterbox. Setup and output layout changes install that
picture as soon as the monitors exist.

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

`behavior.wallpaper`, `behavior.wallpaper_mode`, and `behavior.wallpaper_dir`
are now settable over IPC too:

```sh
jwm-tool msg set_config --args '{"key": "behavior.wallpaper", "value": "/srv/walls/alps.jpg"}'
jwm-tool msg set_config --args '{"key": "behavior.wallpaper_mode", "value": "fit"}'
```

`wallpaper_mode` accepts `fill`, `fit`, `stretch`, or `center`, and rejects
anything else rather than silently falling back.

## Daily rotation and different screens

```toml
[behavior]
wallpaper_rotation = "daily" # "off" restores static wallpaper selection
wallpaper_rotation_paths = [
  "/home/me/.local/share/jwm/wallpapers/01.png",
  "/home/me/.local/share/jwm/wallpapers/02.png",
  "/home/me/.local/share/jwm/wallpapers/03.png",
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
remains asynchronous on X11, Wayland, and compositor-off X11. Theme colors
follow the first output's selected image.

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
bars read, so the whole shell follows the picture. This is on by default:

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
moment later. The extraction is started once per wallpaper — a config apply
that changed something else does not decode the same picture again. If the
worker thread cannot be started at all, the wallpaper is not marked as
extracted: `get_wallpaper_colors` does not report `pending` forever, and the
next config apply tries again.

### Over IPC

```sh
jwm-tool msg get_wallpaper_colors
# {"enabled": true, "wallpaper": "/srv/walls/valley.jpg",
#  "accent": "#83b5d0", "secondary": "#53a63f", "pending": false}
```

`wallpaper` is the picture the colours came from, which is not always the one
on screen: switching to a wallpaper with no colour to take leaves both the
colours and this field on the last one that had some.

A `theme/colors` event carrying the same payload is broadcast on the `theme`
topic whenever the palette changes, so a status bar can match the shell without
polling.

The four colours are settable directly too, as `[r, g, b, a]` in the 0..1
range, which is also how you put your own back without a reload:

```sh
jwm-tool msg set_config --args '{"key": "behavior.border_gradient_color_a", "value": [0.24, 0.65, 1.0, 1.0]}'
```

`jwm-tool health` prints compact `wallpaper` for wallpaper operators.
