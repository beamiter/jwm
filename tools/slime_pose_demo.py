#!/usr/bin/env python3
"""Send an animated synthetic hand pose to JWM without ML dependencies.

Use this first to verify the compositor shader and IPC path independently from
MediaPipe, screen capture, and model latency.
"""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import socket
import sys
import time


BASE_HAND = [
    (0.50, 0.84),  # wrist
    (0.43, 0.73), (0.35, 0.64), (0.28, 0.55), (0.22, 0.48),  # thumb
    (0.43, 0.61), (0.40, 0.44), (0.39, 0.28), (0.39, 0.14),  # index
    (0.50, 0.59), (0.50, 0.39), (0.50, 0.21), (0.50, 0.08),  # middle
    (0.57, 0.62), (0.60, 0.44), (0.62, 0.29), (0.63, 0.17),  # ring
    (0.64, 0.67), (0.70, 0.55), (0.74, 0.44), (0.77, 0.35),  # pinky
]


def parse_window_id(value: str) -> int:
    return int(value, 0)


def default_socket_path() -> Path:
    override = os.environ.get("JWM_SLIME_SOCKET")
    if override:
        return Path(override)
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/jwm-{os.getuid()}")
    return Path(runtime) / "jwm-slime.sock"


def transform_hand(t: float, center_x: float, center_y: float, scale: float):
    angle = math.sin(t * 0.75) * 0.16
    pulse = 1.0 + math.sin(t * 1.35) * 0.035
    cx = center_x + math.sin(t * 0.63) * 0.12
    cy = center_y + math.sin(t * 0.91) * 0.055
    cos_a = math.cos(angle)
    sin_a = math.sin(angle)

    points = []
    for index, (x, y) in enumerate(BASE_HAND):
        local_x = (x - 0.50) * scale * pulse
        local_y = (y - 0.50) * scale * pulse
        # Negative MediaPipe-style Z moves the surface toward the viewer.
        depth = (
            -0.055
            - max(0.0, 0.50 - y) * 0.11
            + math.sin(t * 1.15 + index * 0.37) * 0.018
        )
        points.append(
            [
                cx + local_x * cos_a - local_y * sin_a,
                cy + local_x * sin_a + local_y * cos_a,
                depth,
            ]
        )
    return points


def send(sock: socket.socket, path: Path, packet: dict) -> None:
    payload = json.dumps(packet, separators=(",", ":"), allow_nan=False).encode()
    sock.sendto(payload, str(path))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--window", type=parse_window_id)
    parser.add_argument("--socket", type=Path, default=default_socket_path())
    parser.add_argument("--fps", type=float, default=60.0)
    parser.add_argument("--duration", type=float, default=0.0, help="0 means run until Ctrl+C")
    parser.add_argument("--center-x", type=float, default=0.50)
    parser.add_argument("--center-y", type=float, default=0.50)
    parser.add_argument("--scale", type=float, default=0.72)
    parser.add_argument("--refract-px", type=float, default=14.0)
    parser.add_argument("--ocean-strength", type=float, default=0.42)
    parser.add_argument("--turbulence-strength", type=float, default=0.72)
    parser.add_argument("--foam-strength", type=float, default=0.82)
    args = parser.parse_args()

    if args.fps <= 0.0 or args.scale <= 0.0 or args.refract_px <= 0.0:
        parser.error("fps, scale, and refract-px must be positive")
    for name, value in (
        ("--ocean-strength", args.ocean_strength),
        ("--turbulence-strength", args.turbulence_strength),
        ("--foam-strength", args.foam_strength),
    ):
        if not 0.0 <= value <= 1.0:
            parser.error(f"{name} must be in [0,1]")
    if not args.socket.exists():
        print(f"JWM slime socket does not exist: {args.socket}", file=sys.stderr)
        return 2

    sender = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    started = time.monotonic()
    period = 1.0 / args.fps
    seq = 0
    print(
        f"sending synthetic pose to {args.socket}"
        + (f" for window {hex(args.window)}" if args.window else " in screen coordinates"),
        file=sys.stderr,
    )

    try:
        while args.duration <= 0.0 or time.monotonic() - started < args.duration:
            frame_started = time.monotonic()
            packet = {
                "version": 1,
                "active": True,
                "refract_px": args.refract_px,
                "ocean_strength": args.ocean_strength,
                "turbulence_strength": args.turbulence_strength,
                "foam_strength": args.foam_strength,
                "hands": [
                    {
                        "score": 1.0,
                        "landmarks": transform_hand(
                            frame_started - started,
                            args.center_x,
                            args.center_y,
                            args.scale,
                        ),
                    }
                ],
                "seq": seq,
                "timestamp_ns": time.monotonic_ns(),
            }
            if args.window is not None:
                packet["window"] = args.window
            send(sender, args.socket, packet)
            seq += 1
            remaining = period - (time.monotonic() - frame_started)
            if remaining > 0.0:
                time.sleep(remaining)
    except KeyboardInterrupt:
        pass
    except (FileNotFoundError, ConnectionRefusedError) as exc:
        print(f"slime socket unavailable: {exc}", file=sys.stderr)
        return 3
    finally:
        packet = {"version": 1, "active": False, "hands": [], "seq": seq}
        if args.window is not None:
            packet["window"] = args.window
        try:
            send(sender, args.socket, packet)
        except OSError:
            pass
        sender.close()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
