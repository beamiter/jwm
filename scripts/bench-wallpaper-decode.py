#!/usr/bin/env python3
"""Benchmark wallpaper dispatch and opacity processing with rustc -O, without Cargo.

The pool is included directly from production by the Rust harness. The optimized
pixel loop is extracted from opaque_wallpaper and only its iterator/slice access
is adapted, so this script cannot silently benchmark a second implementation.
Numbers cover dispatch round trips and pixel processing, not decode/GPU latency.
"""

import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
source = (ROOT / "src/backend/compositor_common/wallpaper_decode.rs").read_text()
function = source.index("fn opaque_wallpaper(")
start = source.index("for pixel in rgba.pixels_mut() {", function)
brace = source.index("{", start)
depth = 1
end = brace + 1
while depth:
    depth += (source[end] == "{") - (source[end] == "}")
    end += 1
loop = source[start:end]
for original, replacement in [
    ("rgba.pixels_mut()", "pixels.chunks_exact_mut(4)"),
    ("pixel.0 = [0, 0, 0, 255];", "pixel.copy_from_slice(&[0, 0, 0, 255]);"),
    ("pixel.0[..3]", "pixel[..3]"),
]:
    if loop.count(original) != 1:
        raise RuntimeError(f"Production loop changed; review benchmark adapter: {original}")
    loop = loop.replace(original, replacement)
optimized = "#[inline(never)]\nfn after(pixels: &mut [u8]) {\n" + loop + "\n}\n"

with tempfile.TemporaryDirectory(prefix="jwm-wallpaper-decode-bench-") as temporary:
    directory = Path(temporary)
    extracted = directory / "opaque.rs"
    extracted.write_text(optimized)
    binary = directory / "benchmark"
    environment = dict(os.environ, JWM_WALLPAPER_OPAQUE_RS=str(extracted))
    subprocess.run(
        ["rustc", "--edition", "2024", "-O", str(ROOT / "scripts/bench-wallpaper-decode.rs"),
         "-o", str(binary)],
        env=environment, check=True,
    )
    subprocess.run([str(binary)], check=True)
