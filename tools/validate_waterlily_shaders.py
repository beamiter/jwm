#!/usr/bin/env python3
"""Compile JWM's embedded WaterLily-related shaders with glslangValidator."""

from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE = (
    ROOT / "src/backend/x11/compositor/common/waterlily_shader.rs"
)
JELLY_SOURCE = DEFAULT_SOURCE.with_name("jelly_shader.rs")
JELLY_SHADER_NAMES = (
    "JELLY_MESH_VERTEX_SHADER",
    "JELLY_MESH_FRAGMENT_SHADER",
    "JELLY_SCREEN_VERTEX_SHADER",
    "JELLY_BLUR_FRAGMENT_SHADER",
    "JELLY_COMPOSITE_FRAGMENT_SHADER",
)
SHADER_NAMES = (
    "WATERLILY_FRAGMENT_SHADER",
    "WATERLILY_VOLUME_FRAGMENT_SHADER",
)


def extract_shader(source: str, name: str) -> str:
    markers = (
        f'pub const {name}: &str = r#"',
        f'pub(super) const {name}: &str = r#"',
    )
    located = [
        (source.find(marker), marker)
        for marker in markers
        if source.find(marker) >= 0
    ]
    if not located:
        raise ValueError(f"could not find {name}")
    start, marker = min(located)
    start += len(marker)
    end = source.find('"#;', start)
    if end < 0:
        raise ValueError(f"unterminated Rust raw string for {name}")
    shader = source[start:end]
    if not shader.startswith("#version"):
        raise ValueError(f"{name} does not begin with a GLSL #version directive")
    return shader


def compile_shader(validator: str, name: str, shader: str, directory: Path) -> bool:
    stage = "vert" if name.endswith("VERTEX_SHADER") else "frag"
    path = directory / f"{name.lower()}.{stage}"
    path.write_text(shader, encoding="utf-8")
    result = subprocess.run(
        [validator, "-S", stage, str(path)],
        check=False,
        capture_output=True,
        text=True,
    )
    output = "\n".join(part.strip() for part in (result.stdout, result.stderr) if part.strip())
    if result.returncode != 0:
        print(f"{name}: GLSL validation failed", file=sys.stderr)
        if output:
            print(output, file=sys.stderr)
        return False
    print(f"{name}: OK")
    if output:
        print(output)
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--jelly-source", type=Path, default=JELLY_SOURCE)
    parser.add_argument(
        "--validator",
        default=shutil.which("glslangValidator"),
        help="path to glslangValidator (auto-detected by default)",
    )
    args = parser.parse_args()

    if not args.validator:
        print(
            "glslangValidator was not found; install the glslang-tools package",
            file=sys.stderr,
        )
        return 2
    if not args.source.is_file():
        print(f"shader source does not exist: {args.source}", file=sys.stderr)
        return 2

    if not args.jelly_source.is_file():
        print(f"jelly shader source does not exist: {args.jelly_source}", file=sys.stderr)
        return 2
    source = args.source.read_text(encoding="utf-8")
    jelly_source = args.jelly_source.read_text(encoding="utf-8")
    try:
        shaders = [(name, extract_shader(source, name)) for name in SHADER_NAMES]
        shaders.extend((name, extract_shader(jelly_source, name)) for name in JELLY_SHADER_NAMES)
    except ValueError as exc:
        print(exc, file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="jwm-waterlily-glsl-") as temp:
        directory = Path(temp)
        valid = [
            compile_shader(args.validator, name, shader, directory)
            for name, shader in shaders
        ]
    return 0 if all(valid) else 1


if __name__ == "__main__":
    raise SystemExit(main())
