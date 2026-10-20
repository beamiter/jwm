# JWM bundled static wallpapers

The 14 generated landscape wallpapers live **directly in this directory** as
numbered PNG files (`01-alpine-dawn.png` … `14-luminous-tide.png`).
There is intentionally no `jwm-14/` subdirectory.

Each PNG is 1672 × 941 pixels, as recorded in [manifest.json](manifest.json).
[ prompts.json ](prompts.json) records the original creative prompts; the
requested 3840 × 2160 output size in those prompts is **not** the actual pixel
size. The manifest records SHA-256 checksums and file sizes for reproducibility.

A successful source installation copies only recognized image files to
`~/.config/jwm/wallpapers/`, or `$XDG_CONFIG_HOME/jwm/wallpapers/` when
XDG_CONFIG_HOME is an absolute path. The command can also be run on its own:

```bash
bash scripts/install-wallpapers.sh
```

Already-installed JWM-managed images update when unchanged; a same-name custom
file, edited image, or symlink is left intact. The installer does not delete old
user collections or modify wallpaper selections. Open `Alt+Ctrl+W` to select
a static image. Daily rotation is opt-in via `behavior.wallpaper_rotation` and
`behavior.wallpaper_rotation_paths`; see [docs/wallpaper.md](../../docs/wallpaper.md).

Files, in the order suitable for daily rotation:

1. `01-alpine-dawn.png` — 冰湖晨曦
2. `02-ocean-aerial.png` — 浅海潮汐
3. `03-aurora-fjord.png` — 极光峡湾
4. `04-desert-silence.png` — 沙丘静语
5. `05-bamboo-mist.png` — 竹林云雾
6. `06-autumn-valley.png` — 秋山流光
7. `07-moss-waterfall.png` — 苔谷瀑声
8. `08-cloud-sea.png` — 云海日出
9. `09-starry-dunes.png` — 星河远境
10. `10-volcanic-coast.png` — 玄岩金浪
11. `11-silk-waves.png` — 流体丝光
12. `12-violet-nebula.png` — 紫境星云
13. `13-architecture-fog.png` — 雾中几何
14. `14-luminous-tide.png` — 荧海月夜
