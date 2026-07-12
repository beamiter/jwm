#!/usr/bin/env python3
"""Track a hand in an X11 video window and stream landmarks to JWM.

Optional runtime dependencies:

    python -m pip install -r tools/requirements-slime.txt

The compositor consumes compact JSON datagrams from
``$XDG_RUNTIME_DIR/jwm-slime.sock``. Delivery is intentionally lossy: JWM keeps
only the newest pose, so inference can never build up a latency queue.
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


class ContentRect(NamedTuple):
    x: float
    y: float
    width: float
    height: float


FULL_CONTENT = ContentRect(0.0, 0.0, 1.0, 1.0)


def parse_window_id(value: str) -> int:
    try:
        window_id = int(value, 0)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid X11 window ID: {value}") from exc
    if window_id <= 0:
        raise argparse.ArgumentTypeError("window ID must be positive")
    return window_id


def parse_content_rect(value: str) -> ContentRect:
    try:
        values = [float(part.strip()) for part in value.split(",")]
    except ValueError as exc:
        raise argparse.ArgumentTypeError(
            "content rect must be x,y,width,height"
        ) from exc
    if len(values) != 4:
        raise argparse.ArgumentTypeError("content rect must contain four values")
    x, y, width, height = values
    if width <= 0.0 or height <= 0.0:
        raise argparse.ArgumentTypeError("content width and height must be positive")
    if x < 0.0 or y < 0.0 or x + width > 1.0001 or y + height > 1.0001:
        raise argparse.ArgumentTypeError("content rect must stay inside normalized [0,1]")
    return ContentRect(x, y, width, height)


def default_socket_path() -> Path:
    override = os.environ.get("JWM_SLIME_SOCKET")
    if override:
        return Path(override)
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/jwm-{os.getuid()}")
    return Path(runtime) / "jwm-slime.sock"


def select_window_id() -> int:
    print("Click the video window to track...", file=sys.stderr)
    try:
        output = subprocess.run(
            ["xwininfo", "-int"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except FileNotFoundError as exc:
        raise RuntimeError("xwininfo is required; install the x11-utils package") from exc
    match = re.search(r"Window id:\s+(0x[0-9a-fA-F]+|\d+)", output)
    if not match:
        raise RuntimeError("could not parse the selected X11 window ID")
    return int(match.group(1), 0)


def query_geometry(window_id: int) -> Geometry:
    try:
        output = subprocess.run(
            ["xwininfo", "-id", hex(window_id)],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except FileNotFoundError as exc:
        raise RuntimeError("xwininfo is required; install the x11-utils package") from exc

    def number(label: str) -> int:
        match = re.search(
            rf"^\s*{re.escape(label)}:\s*(-?\d+)\s*$", output, re.MULTILINE
        )
        if not match:
            raise RuntimeError(f"xwininfo did not report {label!r}")
        return int(match.group(1))

    geometry = Geometry(
        number("Absolute upper-left X"),
        number("Absolute upper-left Y"),
        number("Width"),
        number("Height"),
    )
    if geometry.width <= 0 or geometry.height <= 0:
        raise RuntimeError("selected window has an empty geometry")
    return geometry


def capture_geometry(window: Geometry, content: ContentRect) -> Geometry:
    left = window.x + round(content.x * window.width)
    top = window.y + round(content.y * window.height)
    width = max(1, round(content.width * window.width))
    height = max(1, round(content.height * window.height))
    return Geometry(left, top, width, height)


def resize_for_inference(frame, cv2, max_width: int, max_height: int):
    height, width = frame.shape[:2]
    scale = min(1.0, max_width / width, max_height / height)
    if scale >= 0.999:
        return frame
    target = (max(1, round(width * scale)), max(1, round(height * scale)))
    return cv2.resize(frame, target, interpolation=cv2.INTER_AREA)


def send_packet(sock: socket.socket, socket_path: Path, packet: dict) -> None:
    payload = json.dumps(packet, separators=(",", ":"), allow_nan=False).encode("utf-8")
    sock.sendto(payload, str(socket_path))


def inactive_packet(window_id: int, seq: int) -> dict:
    return {
        "version": 1,
        "active": False,
        "window": window_id,
        "hands": [],
        "seq": seq,
        "timestamp_ns": time.monotonic_ns(),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--window",
        type=parse_window_id,
        help="X11 window ID; omit it to select a window interactively",
    )
    parser.add_argument("--socket", type=Path, default=default_socket_path())
    parser.add_argument("--fps", type=float, default=30.0)
    parser.add_argument("--refract-px", type=float, default=8.0)
    parser.add_argument(
        "--ocean-strength",
        type=float,
        default=0.32,
        help="continuous directional-ocean contribution in [0,1]",
    )
    parser.add_argument(
        "--interaction-strength",
        type=float,
        default=0.55,
        help="fingertip/palm wake strength in [0,1]",
    )
    parser.add_argument(
        "--turbulence-strength",
        type=float,
        default=0.68,
        help="horizontal-flow vorticity and micro-normal strength in [0,1]",
    )
    parser.add_argument(
        "--foam-strength",
        type=float,
        default=0.78,
        help="crest/vorticity foam generation in [0,1]",
    )
    parser.add_argument("--min-score", type=float, default=0.55)
    parser.add_argument(
        "--content-rect",
        type=parse_content_rect,
        default=FULL_CONTENT,
        metavar="X,Y,W,H",
        help="normalized video-content rectangle inside the X11 window",
    )
    parser.add_argument(
        "--max-width",
        type=int,
        default=640,
        help="maximum inference-frame width (capture coordinates stay unchanged)",
    )
    parser.add_argument("--max-height", type=int, default=640)
    parser.add_argument("--model-complexity", type=int, choices=(0, 1), default=0)
    parser.add_argument("--debug", action="store_true", help="show the inference crop")
    args = parser.parse_args()

    if args.fps <= 0:
        parser.error("--fps must be positive")
    if args.max_width <= 0 or args.max_height <= 0:
        parser.error("inference dimensions must be positive")
    if not 0.0 <= args.min_score <= 1.0:
        parser.error("--min-score must be in [0,1]")
    if args.refract_px <= 0.0:
        parser.error("--refract-px must be positive")
    for name, value in (
        ("--ocean-strength", args.ocean_strength),
        ("--interaction-strength", args.interaction_strength),
        ("--turbulence-strength", args.turbulence_strength),
        ("--foam-strength", args.foam_strength),
    ):
        if not 0.0 <= value <= 1.0:
            parser.error(f"{name} must be in [0,1]")

    try:
        import cv2  # type: ignore
        import mediapipe as mp  # type: ignore
        import mss  # type: ignore
        import numpy as np  # type: ignore
    except ImportError as exc:
        print(
            "missing dependency; run: python -m pip install "
            "opencv-python mediapipe mss numpy",
            file=sys.stderr,
        )
        print(exc, file=sys.stderr)
        return 2

    try:
        window_id = args.window if args.window is not None else select_window_id()
        geometry = query_geometry(window_id)
    except (RuntimeError, subprocess.CalledProcessError) as exc:
        print(f"cannot select video window: {exc}", file=sys.stderr)
        return 2

    if not args.socket.exists():
        print(f"JWM slime socket does not exist: {args.socket}", file=sys.stderr)
        print("Start JWM's X11 compositor first.", file=sys.stderr)
        return 2

    geometry_updated = 0.0
    seq = 0
    frame_period = 1.0 / args.fps
    sender = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    was_active = False

    hands_api = mp.solutions.hands
    drawing = mp.solutions.drawing_utils
    detector = hands_api.Hands(
        static_image_mode=False,
        max_num_hands=1,
        model_complexity=args.model_complexity,
        min_detection_confidence=args.min_score,
        min_tracking_confidence=args.min_score,
    )

    print(
        f"tracking {hex(window_id)} at {geometry.width}x{geometry.height}; "
        f"content={tuple(args.content_rect)}; socket={args.socket}; "
        f"ocean={args.ocean_strength:.2f} interaction={args.interaction_strength:.2f} "
        f"turbulence={args.turbulence_strength:.2f} "
        f"foam={args.foam_strength:.2f}",
        file=sys.stderr,
    )

    try:
        with mss.mss() as capture:
            while True:
                started = time.monotonic()
                if started - geometry_updated >= 0.5:
                    try:
                        geometry = query_geometry(window_id)
                    except (RuntimeError, subprocess.CalledProcessError) as exc:
                        print(f"video window is no longer available: {exc}", file=sys.stderr)
                        return 3
                    geometry_updated = started

                region = capture_geometry(geometry, args.content_rect)
                monitor = {
                    "left": region.x,
                    "top": region.y,
                    "width": region.width,
                    "height": region.height,
                }
                bgra = np.asarray(capture.grab(monitor))
                bgr = cv2.cvtColor(bgra, cv2.COLOR_BGRA2BGR)
                inference_bgr = resize_for_inference(
                    bgr, cv2, args.max_width, args.max_height
                )
                rgb = cv2.cvtColor(inference_bgr, cv2.COLOR_BGR2RGB)
                rgb.flags.writeable = False
                result = detector.process(rgb)
                rgb.flags.writeable = True

                if result.multi_hand_landmarks:
                    hand = result.multi_hand_landmarks[0]
                    landmarks = [
                        [float(point.x), float(point.y), float(point.z)]
                        for point in hand.landmark
                    ]
                    packet = {
                        "version": 1,
                        "active": True,
                        "window": window_id,
                        "content_rect": list(args.content_rect),
                        "refract_px": args.refract_px,
                        "ocean_strength": args.ocean_strength,
                        "interaction_strength": args.interaction_strength,
                        "turbulence_strength": args.turbulence_strength,
                        "foam_strength": args.foam_strength,
                        "hands": [{"score": 1.0, "landmarks": landmarks}],
                        "seq": seq,
                        "timestamp_ns": time.monotonic_ns(),
                    }
                    send_packet(sender, args.socket, packet)
                    was_active = True

                    if args.debug:
                        drawing.draw_landmarks(
                            inference_bgr, hand, hands_api.HAND_CONNECTIONS
                        )
                elif was_active:
                    # Send the transition once. JWM also times out stale poses, so a
                    # dropped datagram still cannot leave the effect stuck onscreen.
                    send_packet(sender, args.socket, inactive_packet(window_id, seq))
                    was_active = False

                seq += 1

                if args.debug:
                    elapsed_ms = (time.monotonic() - started) * 1000.0
                    cv2.putText(
                        inference_bgr,
                        f"{elapsed_ms:.1f} ms",
                        (10, 24),
                        cv2.FONT_HERSHEY_SIMPLEX,
                        0.65,
                        (0, 255, 0),
                        2,
                    )
                    cv2.imshow("jwm slime tracker", inference_bgr)
                    if cv2.waitKey(1) & 0xFF in (27, ord("q")):
                        break

                remaining = frame_period - (time.monotonic() - started)
                if remaining > 0:
                    time.sleep(remaining)
    except KeyboardInterrupt:
        pass
    except (FileNotFoundError, ConnectionRefusedError) as exc:
        print(f"slime socket unavailable: {exc}", file=sys.stderr)
        return 3
    finally:
        try:
            send_packet(sender, args.socket, inactive_packet(window_id, seq))
        except OSError:
            pass
        detector.close()
        sender.close()
        if args.debug:
            cv2.destroyAllWindows()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
