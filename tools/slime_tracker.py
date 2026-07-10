#!/usr/bin/env python3
"""Track one hand in an X11 window and stream 21 landmarks to JWM.

Runtime dependencies (kept outside JWM's Rust dependency graph):

    python -m pip install opencv-python mediapipe mss

The compositor consumes newline-free JSON datagrams from
$XDG_RUNTIME_DIR/jwm-slime.sock. Datagrams are intentionally lossy; JWM drains
the socket and uses only the newest pose each frame.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time
from typing import NamedTuple


class Geometry(NamedTuple):
    x: int
    y: int
    width: int
    height: int


def parse_window_id(value: str) -> int:
    return int(value, 0)


def default_socket_path() -> Path:
    override = os.environ.get("JWM_SLIME_SOCKET")
    if override:
        return Path(override)
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/jwm-{os.getuid()}")
    return Path(runtime) / "jwm-slime.sock"


def query_geometry(window_id: int) -> Geometry:
    result = subprocess.run(
        ["xwininfo", "-id", hex(window_id)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout

    def number(label: str) -> int:
        match = re.search(rf"^\s*{re.escape(label)}:\s*(-?\d+)\s*$", result, re.MULTILINE)
        if not match:
            raise RuntimeError(f"xwininfo did not report {label!r}")
        return int(match.group(1))

    return Geometry(
        number("Absolute upper-left X"),
        number("Absolute upper-left Y"),
        number("Width"),
        number("Height"),
    )


def send_packet(sock: socket.socket, socket_path: Path, packet: dict) -> None:
    payload = json.dumps(packet, separators=(",", ":"), allow_nan=False).encode("utf-8")
    sock.sendto(payload, str(socket_path))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--window", required=True, type=parse_window_id, help="X11 window ID")
    parser.add_argument("--socket", type=Path, default=default_socket_path())
    parser.add_argument("--fps", type=float, default=30.0)
    parser.add_argument("--refract-px", type=float, default=12.0)
    parser.add_argument("--min-score", type=float, default=0.55)
    parser.add_argument("--debug", action="store_true", help="show the captured frame")
    args = parser.parse_args()

    if args.fps <= 0:
        parser.error("--fps must be positive")

    try:
        import cv2  # type: ignore
        import mediapipe as mp  # type: ignore
        import mss  # type: ignore
        import numpy as np  # type: ignore
    except ImportError as exc:
        print(
            "missing dependency; run: python -m pip install opencv-python mediapipe mss",
            file=sys.stderr,
        )
        print(exc, file=sys.stderr)
        return 2

    if not args.socket.exists():
        print(f"JWM slime socket does not exist: {args.socket}", file=sys.stderr)
        print("Start JWM's X11 compositor first.", file=sys.stderr)
        return 2

    geometry = query_geometry(args.window)
    geometry_updated = 0.0
    seq = 0
    frame_period = 1.0 / args.fps
    sender = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)

    hands_api = mp.solutions.hands
    drawing = mp.solutions.drawing_utils
    detector = hands_api.Hands(
        static_image_mode=False,
        max_num_hands=1,
        model_complexity=0,
        min_detection_confidence=args.min_score,
        min_tracking_confidence=args.min_score,
    )

    print(
        f"tracking window {hex(args.window)} at {geometry.width}x{geometry.height}; "
        f"sending to {args.socket}",
        file=sys.stderr,
    )

    try:
        with mss.mss() as capture:
            while True:
                started = time.monotonic()
                if started - geometry_updated >= 0.5:
                    geometry = query_geometry(args.window)
                    geometry_updated = started

                monitor = {
                    "left": geometry.x,
                    "top": geometry.y,
                    "width": max(1, geometry.width),
                    "height": max(1, geometry.height),
                }
                bgra = np.asarray(capture.grab(monitor))
                bgr = cv2.cvtColor(bgra, cv2.COLOR_BGRA2BGR)
                rgb = cv2.cvtColor(bgr, cv2.COLOR_BGR2RGB)
                result = detector.process(rgb)

                packet: dict = {
                    "version": 1,
                    "active": False,
                    "window": args.window,
                    "refract_px": args.refract_px,
                    "hands": [],
                    "seq": seq,
                }
                seq += 1

                if result.multi_hand_landmarks:
                    hand = result.multi_hand_landmarks[0]
                    landmarks = [[float(point.x), float(point.y)] for point in hand.landmark]
                    packet["active"] = True
                    packet["hands"] = [{"score": 1.0, "landmarks": landmarks}]

                    if args.debug:
                        drawing.draw_landmarks(bgr, hand, hands_api.HAND_CONNECTIONS)

                try:
                    send_packet(sender, args.socket, packet)
                except (FileNotFoundError, ConnectionRefusedError) as exc:
                    print(f"slime socket unavailable: {exc}", file=sys.stderr)
                    return 3

                if args.debug:
                    cv2.imshow("jwm slime tracker", bgr)
                    if cv2.waitKey(1) & 0xFF in (27, ord("q")):
                        break

                remaining = frame_period - (time.monotonic() - started)
                if remaining > 0:
                    time.sleep(remaining)
    except KeyboardInterrupt:
        pass
    finally:
        try:
            send_packet(
                sender,
                args.socket,
                {"version": 1, "active": False, "window": args.window, "hands": []},
            )
        except OSError:
            pass
        detector.close()
        sender.close()
        if args.debug:
            cv2.destroyAllWindows()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
