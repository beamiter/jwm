# Wallpaper picker

`Alt+Ctrl+W` (`wallpaper_picker`) lists the images in the wallpaper directory
and applies the one you pick. `Up`/`Down` select, `Enter` applies and closes,
`Esc` closes without changing anything. The wallpaper already in use is marked
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
