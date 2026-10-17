#!/usr/bin/env python3
"""Derive the offline 8K lunar detail texture from NASA SVS's sRGB TIFF.

Requires FFmpeg, NumPy and Pillow. This performs colour-depth quantization and
JPEG encoding only: no resampling, sharpening, synthetic terrain or AI imagery.
The source is intentionally not downloaded by this tool or by JWM at runtime.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from PIL import Image, __version__ as pillow_version


SOURCE_URL = (
    "https://svs.gsfc.nasa.gov/vis/a000000/a004700/a004720/"
    "lroc_color_16bit_srgb_8k.tif"
)
SOURCE_SHA256 = "db7808e878b6a55eb409bb231eab8deb477f84b5c9d7396d76ff73e5d54992d9"


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument(
        "--output", type=Path, default=Path("assets/astronomy/moon-nasa-lroc-8k.jpg")
    )
    args = parser.parse_args()
    if sha256(args.source) != SOURCE_SHA256:
        raise SystemExit("NASA source checksum differs; review the source before deriving an asset")
    with Image.open(args.source) as source:
        if source.size != (8192, 4096) or source.tag_v2.get(258) != (16, 16, 16):
            raise SystemExit("Expected NASA's 8192×4096 RGB16 sRGB colour map")
    with tempfile.TemporaryDirectory(prefix="jwm-moon-detail-") as directory:
        raw = Path(directory) / "moon.rgb48le"
        subprocess.run(
            [
                "ffmpeg", "-v", "error", "-threads", "1", "-i", str(args.source),
                "-frames:v", "1", "-pix_fmt", "rgb48le", "-f", "rawvideo", str(raw),
            ],
            check=True,
        )
        if raw.stat().st_size != 8192 * 4096 * 3 * 2:
            raise SystemExit("Unexpected decoded texture length")
        source = np.memmap(raw, mode="r", dtype="<u2", shape=(4096, 8192, 3))
        pixels = np.empty((4096, 8192, 3), dtype=np.uint8)
        # Input is already encoded in sRGB. Round 0..65535 to 0..255 directly;
        # a second transfer curve would wash out the scientific source colours.
        for row in range(0, 4096, 128):
            pixels[row:row + 128] = (
                (source[row:row + 128].astype(np.uint32) + 128) // 257
            ).astype(np.uint8)
        del source
        args.output.parent.mkdir(parents=True, exist_ok=True)
        Image.fromarray(pixels).save(
            args.output, "JPEG", quality=91, subsampling=0, optimize=False, progressive=False
        )
    print(json.dumps({
        "source": SOURCE_URL,
        "source_sha256": SOURCE_SHA256,
        "output": str(args.output),
        "dimensions": [8192, 4096],
        "bytes": args.output.stat().st_size,
        "sha256": sha256(args.output),
        "pillow": pillow_version,
        "encoding": "sRGB, RGB8, JPEG quality 91, 4:4:4, no resampling",
    }, indent=2))


if __name__ == "__main__":
    main()
