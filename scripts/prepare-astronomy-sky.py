#!/usr/bin/env python3
"""Offline, optional asset-maintenance tool; never used at JWM runtime.

Usage: python3 scripts/prepare-astronomy-sky.py SOURCE.exr OUTPUT.jpg
Requires FFmpeg (EXR decoder), NumPy and Pillow. Download SOURCE separately from
NASA SVS 4851; see assets/astronomy/README.md for provenance and reuse guidance.
"""
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile

import numpy as np
from PIL import Image

SOURCE_SHA256 = "fad63b36aa632c691d521cd9d8b828de9d29b4c2784276eec44e54c6cb159a49"


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    source, destination = map(Path, sys.argv[1:])
    if hashlib.sha256(source.read_bytes()).hexdigest() != SOURCE_SHA256:
        raise SystemExit("Unexpected source: require the verified NASA 4K galactic EXR")
    with tempfile.TemporaryDirectory(prefix="jwm-sky-") as tmp:
        raw = Path(tmp) / "linear.gbr"
        subprocess.run(["ffmpeg", "-v", "error", "-i", str(source), "-frames:v", "1",
                        "-f", "rawvideo", "-pix_fmt", "gbrpf32le", str(raw)], check=True)
        # FFmpeg planar float output is G, B, R; no display gamma applied here.
        linear = np.fromfile(raw, dtype="<f4").reshape(3, 2048, 4096)
        rgb = linear[[2, 0, 1]].transpose(1, 2, 0)
        # Downsample radiance before encoding, rather than averaging sRGB bytes.
        resized = np.stack([np.array(Image.fromarray(rgb[:, :, channel]).resize(
            (2048, 1024), Image.Resampling.LANCZOS)) for channel in range(3)], axis=-1)
        resized = np.clip(resized, 0.0, 1.0)
        srgb = np.where(resized <= 0.0031308, resized * 12.92,
                        1.055 * resized ** (1.0 / 2.4) - 0.055)
        encoded = np.uint8(np.clip(srgb * 255.0 + 0.5, 0.0, 255.0))
        Image.fromarray(encoded).save(destination, quality=91, subsampling=0, optimize=True)
    print(hashlib.sha256(destination.read_bytes()).hexdigest(), destination)


if __name__ == "__main__":
    main()
