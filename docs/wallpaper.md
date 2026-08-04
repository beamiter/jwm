# Wallpaper picker

`Alt+Ctrl+W` (`wallpaper_picker`) lists the images in the wallpaper directory
and applies the one you pick. `Up`/`Down` select, `Enter` applies and closes,
`Esc` — or `Alt+Ctrl+W` again — closes without changing anything. The wallpaper already in use is marked
and starts selected, so reopening the panel does not lose your place.

## Which directory

```toml
[behavior]
wallpaper_dir = "~/Pictures/Wallpapers"
```

Left empty (the default), the picker looks beside the current wallpaper —
whoever set one almost certainly keeps the rest in the same place — then falls
back to `~/Pictures/Wallpapers`, then `~/Pictures`. A leading `~` is expanded.

`png`, `jpg`, `jpeg`, `webp`, `bmp`, and `gif` are listed, sorted
case-insensitively by name, and capped at 200 entries: a pictures directory
can be enormous, and reading tens of thousands of names would stall the frame
that opened the panel.

## How it applies

The choice goes through the same path a `set_config` takes — it updates
`behavior.wallpaper` in the live configuration and runs the normal
apply-config step, so both compositors pick it up exactly as they would from a
reload, crossfade included when `wallpaper_crossfade` is on.

That also means the change is **in memory only**, like every other
`set_config`: it lasts for the session and is not written back to
`config_x11.toml`. To make a wallpaper permanent, set `behavior.wallpaper` in
the file.

`behavior.wallpaper`, `behavior.wallpaper_mode`, and `behavior.wallpaper_dir`
are now settable over IPC too:

```sh
jwm-msg '{"command": "set_config", "args": {"key": "behavior.wallpaper", "value": "/srv/walls/alps.jpg"}}'
jwm-msg '{"command": "set_config", "args": {"key": "behavior.wallpaper_mode", "value": "fit"}}'
```

`wallpaper_mode` accepts `fill`, `fit`, `stretch`, or `center`, and rejects
anything else rather than silently falling back.

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
that changed something else does not decode the same picture again.

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
