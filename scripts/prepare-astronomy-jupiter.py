#!/usr/bin/env python3
"""Regrid NASA/JPL's Cassini Jupiter mosaic for JWM's 2:1 globe decoder.

Requires Pillow. The input is the original PIA07782 JPEG, 3601 by 1801 pixels.
The whole image is resampled once to 3600 by 1800 pixels with Lanczos and saved
as RGB JPEG. No flip, crop, tone adjustment, sharpening or synthetic detail is
applied. The source is intentionally not downloaded here or by JWM at runtime.
"""

import argparse
import hashlib
import json
from pathlib import Path

from PIL import Image, __version__ as pillow_version


SOURCE_URL = "https://d2pn8kiwq2w21t.cloudfront.net/original_images/jpegPIA07782.jpg"
SOURCE_SHA256 = "6b835ddd8036ea9efd25def04f75aa60b69b096025a37eeae5e9b6978e0a2281"


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument(
        "--output", type=Path, default=Path("assets/astronomy/jupiter-nasa-cassini.jpg")
    )
    args = parser.parse_args()
    if args.source.resolve() == args.output.resolve():
        raise SystemExit("Keep the original NASA source separate from the derived image")
    if sha256(args.source) != SOURCE_SHA256:
        raise SystemExit("NASA source checksum differs; review the source before deriving an asset")
    with Image.open(args.source) as source:
        if source.format != "JPEG" or source.mode != "RGB" or source.size != (3601, 1801):
            raise SystemExit("Expected NASA's original 3601×1801 RGB JPEG mosaic")
        result = source.resize((3600, 1800), Image.Resampling.LANCZOS)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    result.save(
        args.output, "JPEG", quality=95, subsampling=0, optimize=True, progressive=False
    )
    print(json.dumps({
        "source": SOURCE_URL,
        "source_sha256": SOURCE_SHA256,
        "source_dimensions": [3601, 1801],
        "output": str(args.output),
        "dimensions": [3600, 1800],
        "bytes": args.output.stat().st_size,
        "sha256": sha256(args.output),
        "pillow": pillow_version,
        "encoding": "RGB8, JPEG quality 95, 4:4:4, full-domain Lanczos resampling",
    }, indent=2))


if __name__ == "__main__":
    main()
