#!/usr/bin/env python3
"""Build JWM's offline full-sky colour/constellation data atlas from NASA SVS.

Requires FFmpeg, NumPy and Pillow. Sources must be downloaded separately; JWM
never downloads sky imagery at runtime. PNG alpha stores inverse line intensity,
not opacity: all RGB samples, including those under zero alpha, must survive.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from PIL import Image, __version__ as pillow_version


STAR_SHA256 = "d70924422d3e0159764b16a19658784befcf73b97e06e5c16614860c1514bb58"
FIGURE_SHA256 = "acca4620586513dec05eb22920ac6ba8847a5ce8902a85fb37f386b32aa08757"
SOURCE_ROOT = "https://svs.gsfc.nasa.gov/vis/a000000/a004800/a004851/"


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stars", type=Path)
    parser.add_argument("figures", type=Path)
    parser.add_argument(
        "--output", type=Path, default=Path("assets/astronomy/galaxy-nasa-4k.png")
    )
    args = parser.parse_args()
    if sha256(args.stars) != STAR_SHA256 or sha256(args.figures) != FIGURE_SHA256:
        raise SystemExit("NASA source checksum differs; review source data before deriving the atlas")
    with Image.open(args.figures) as source:
        if source.mode != "L" or source.size != (4096, 2048):
            raise SystemExit("Expected NASA's 4096×2048 galactic constellation figure mask")
        inverse_lines = 255 - np.asarray(source, dtype=np.uint8)
    with tempfile.TemporaryDirectory(prefix="jwm-galaxy-") as directory:
        raw = Path(directory) / "stars.gbrpf32le"
        subprocess.run(
            [
                "ffmpeg", "-v", "error", "-threads", "1", "-i", str(args.stars),
                "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gbrpf32le", str(raw),
            ],
            check=True,
        )
        if raw.stat().st_size != 8192 * 4096 * 3 * 4:
            raise SystemExit("Unexpected decoded star map dimensions")
        # FFmpeg emits planes G, B, R with no display transfer. Resize one plane
        # at a time to keep peak memory bounded while preserving linear light.
        linear = np.memmap(raw, mode="r", dtype="<f4", shape=(3, 4096, 8192))
        encoded = np.empty((2048, 4096, 4), dtype=np.uint8)
        for channel, plane in enumerate([2, 0, 1]):
            resized = np.asarray(
                Image.fromarray(linear[plane]).resize((4096, 2048), Image.Resampling.LANCZOS)
            )
            resized = np.clip(resized, 0.0, 1.0)
            srgb = np.where(
                resized <= 0.0031308,
                resized * 12.92,
                1.055 * resized ** (1.0 / 2.4) - 0.055,
            )
            encoded[:, :, channel] = np.uint8(np.clip(srgb * 255.0 + 0.5, 0.0, 255.0))
        del linear
        encoded[:, :, 3] = inverse_lines
        args.output.parent.mkdir(parents=True, exist_ok=True)
        Image.fromarray(encoded).save(args.output, "PNG", compress_level=9, optimize=True)
        # Data packing must never premultiply RGB, erase RGB at zero alpha, or
        # reinterpret the inverse figure mask as transparency during export.
        with Image.open(args.output) as atlas:
            if not np.array_equal(np.asarray(atlas), encoded):
                raise SystemExit("PNG data atlas did not preserve all RGBA channels exactly")
    print(json.dumps({
        "star_source": SOURCE_ROOT + "starmap_2020_8k_gal.exr",
        "star_source_sha256": STAR_SHA256,
        "figure_source": SOURCE_ROOT + "constellation_figures_4k_gal.tif",
        "figure_source_sha256": FIGURE_SHA256,
        "output": str(args.output),
        "dimensions": [4096, 2048],
        "bytes": args.output.stat().st_size,
        "sha256": sha256(args.output),
        "pillow": pillow_version,
        "channels": "RGB: sRGB catalogue stars; A: 255 minus constellation figure intensity",
        "encoding": "Straight RGBA8 data PNG; no premultiplication; alpha is not opacity",
    }, indent=2))


if __name__ == "__main__":
    main()
