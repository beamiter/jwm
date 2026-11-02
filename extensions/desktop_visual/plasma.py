#!/usr/bin/env python3
"""Unrelated math-art producer: animated translucent plasma, no simulation SDK."""

from __future__ import annotations

import argparse
import math
import sys
import time

from desktop_visual import Alpha, Capabilities, ProtocolError, Publisher


def frame_size(capabilities: Capabilities, maximum: int) -> tuple[int, int]:
    scale = min(1.0, maximum / max(capabilities.output_width, capabilities.output_height),
                capabilities.max_dimension / max(capabilities.output_width, capabilities.output_height))
    width = max(1, int(capabilities.output_width * scale))
    height = max(1, int(capabilities.output_height * scale))
    if width * height * 4 > capabilities.max_frame_bytes:
        scale = math.sqrt(capabilities.max_frame_bytes / (width * height * 4))
        width, height = max(1, int(width * scale)), max(1, int(height * scale))
        # Clamping a very thin canvas to one pixel can defeat the area-based
        # scale. Apply the negotiated pixel budget exactly after rounding.
        pixel_limit = capabilities.max_frame_bytes // 4
        width = min(width, pixel_limit)
        height = min(height, max(1, pixel_limit // width))
    return width, height


def plasma(width: int, height: int, phase: float) -> bytearray:
    """Display-space top-to-bottom RGBA8, straight alpha; deterministic input."""
    pixels = bytearray(width * height * 4)
    k = 0
    for y in range(height):
        ny = (y + 0.5) / height
        for x in range(width):
            nx = (x + 0.5) / width
            field = (math.sin(nx * 12 + phase) + math.sin(ny * 9 - phase * 0.8)
                     + math.sin(math.hypot(nx - 0.5, ny - 0.5) * 18 - phase * 1.2)) / 3
            hue = field * math.pi + phase * 0.25
            pixels[k] = int(127.5 * (1 + math.sin(hue)))
            pixels[k + 1] = int(127.5 * (1 + math.sin(hue + 2.094)))
            pixels[k + 2] = int(127.5 * (1 + math.sin(hue + 4.188)))
            # Transparent edges leave windows readable around the math canvas.
            edge = max(0.0, math.sin(math.pi * nx) * math.sin(math.pi * ny))
            pixels[k + 3] = int(150 * edge * edge)
            k += 4
    return pixels


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", help="private Unix socket (otherwise SDK environment defaults)")
    parser.add_argument("--fps", type=float, default=20.0)
    parser.add_argument("--size", type=int, default=320, help="maximum source image dimension")
    parser.add_argument("--frames", type=int, default=0, help="stop after N sent frames (0: unlimited)")
    parser.add_argument("--duration", type=float, default=0.0, help="total wall seconds including reconnects (0: unlimited)")
    args = parser.parse_args(argv)
    if not math.isfinite(args.fps) or not 0 < args.fps <= 120:
        parser.error("--fps must be finite and in (0, 120]")
    if not 1 <= args.size <= 4096 or args.frames < 0:
        parser.error("--size must be 1..4096 and --frames must be nonnegative")
    if not math.isfinite(args.duration) or args.duration < 0:
        parser.error("--duration must be finite and nonnegative")
    started = time.monotonic()
    sent = 0
    next_frame = started
    last_error = None
    try:
        publisher = Publisher(args.socket, timeout=0.5)
    except ValueError as error:
        parser.error(str(error))
    try:
        while not args.frames or sent < args.frames:
            now = time.monotonic()
            if args.duration and now - started >= args.duration:
                break
            try:
                caps = publisher.capabilities if publisher.connected else publisher.connect()
                assert caps is not None
                if not caps.supports_alpha(Alpha.STRAIGHT):
                    raise ProtocolError("plasma requires straight-alpha capability")
                width, height = frame_size(caps, args.size)
                pixels = plasma(width, height, now - started)
                publisher.publish(pixels, width, height, alpha=Alpha.STRAIGHT)
                sent += 1
                last_error = None
                next_frame = max(next_frame + 1 / args.fps, time.monotonic())
                delay = next_frame - time.monotonic()
            except ProtocolError as error:
                print(f"plasma: incompatible consumer: {error}", file=sys.stderr)
                return 2
            except OSError as error:
                publisher.close()
                # Back off without a queue; redraw from current time after the
                # next hello, which may carry a resized output canvas.
                message = str(error)
                if message != last_error:
                    print(f"plasma: waiting for consumer: {message}", file=sys.stderr)
                    last_error = message
                delay = 0.25
            if args.duration:
                delay = min(delay, max(0, args.duration - (time.monotonic() - started)))
            if delay > 0:
                time.sleep(delay)
    except KeyboardInterrupt:
        pass
    finally:
        publisher.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
