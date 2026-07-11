#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
import tomllib
from datetime import datetime, timezone
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from runner.demo_windows import DemoWindows
    from runner.environment import preflight
    from runner.jwm_ipc import JwmIpc
    from runner.recorder import Recorder
    from runner.session_guard import SessionGuard
else:
    from .demo_windows import DemoWindows
    from .environment import preflight
    from .jwm_ipc import JwmIpc
    from .recorder import Recorder
    from .session_guard import SessionGuard

ROOT = Path(__file__).resolve().parents[2]
BASE = ROOT / "video-demo"


def _interrupt_from_signal(_signum: int, _frame: object) -> None:
    raise KeyboardInterrupt


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Record deterministic JWM scenes in a real X11 session")
    parser.add_argument("--backend", choices=("x11rb", "xcb"), default="x11rb")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--profile", default="smoke")
    selection.add_argument("--scene")
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--demo-client", type=Path)
    parser.add_argument("--build-demo-client", action="store_true")
    parser.add_argument("--allow-non-ready", action="store_true")
    parser.add_argument("--resolution", help="Expected session resolution, e.g. 1920x1080")
    parser.add_argument("--fps", type=int, default=60)
    return parser.parse_args()


def load_scenes(args: argparse.Namespace) -> list[dict]:
    data = tomllib.loads((BASE / "manifest/scenes.toml").read_text())
    scenes = {item["id"]: item for item in data["scene"]}
    ids = [args.scene] if args.scene else data["profiles"].get(args.profile)
    if not ids:
        raise RuntimeError(f"unknown or empty profile: {args.profile}")
    selected = []
    for scene_id in ids:
        if scene_id not in scenes: raise RuntimeError(f"unknown scene: {scene_id}")
        scene = scenes[scene_id]
        if scene["status"] != "ready" and not args.allow_non_ready:
            raise RuntimeError(f"scene {scene_id} is {scene['status']}; pass --allow-non-ready for manual validation")
        selected.append(scene)
    return selected


def demo_binary(args: argparse.Namespace) -> Path:
    path = args.demo_client or BASE / "demo-client/target/debug/jwm-demo-client"
    if args.build_demo_client or not path.exists():
        subprocess.run(["cargo", "build", "--manifest-path", str(BASE / "demo-client/Cargo.toml")], cwd=ROOT, check=True)
    if not path.exists(): raise RuntimeError(f"demo client not found: {path}")
    return path.resolve()


def wait_layout(ipc: JwmIpc, layout: str, timeout: float = 5.0) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        focused = next((item for item in (ipc.query("get_workspaces") or []) if item.get("focused")), None)
        if focused and layout in str(focused.get("layout", "")).lower(): return focused
        time.sleep(0.05)
    raise TimeoutError(f"workspace did not enter {layout} layout")


def timed_pause(seconds: float) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline: time.sleep(min(0.05, deadline - time.monotonic()))


def write_text_assets(scene: dict, generated: Path) -> None:
    narration = generated / "narration" / f"{scene['id']}.txt"
    subtitles = generated / "subtitles" / f"{scene['id']}.srt"
    narration.parent.mkdir(parents=True, exist_ok=True)
    subtitles.parent.mkdir(parents=True, exist_ok=True)
    text = scene["narration"].strip()
    narration.write_text(text + "\n")
    seconds = max(1, int(float(scene["duration"])))
    subtitles.write_text(f"1\n00:00:00,000 --> 00:00:{seconds:02d},000\n{text}\n")


def run_scene(scene: dict, ipc: JwmIpc, recorder: Recorder, windows: DemoWindows, generated: Path, guard: SessionGuard) -> dict:
    started = time.monotonic()
    clip = (generated / "clips" / f"{scene['id']}.mp4").resolve()
    if clip.exists():
        archive = clip.with_suffix(f".{int(time.time())}.mp4")
        clip.rename(archive)
    windows.spawn(int(scene["windows"]), "grid")
    guard.update(False, windows.pids)
    managed = windows.wait_managed(int(scene["windows"]))
    ipc.command("setlayout", {"layout": scene["layout"]})
    workspace = wait_layout(ipc, scene["layout"])
    recorder.start(clip)
    guard.update(True, windows.pids)
    timed_pause(0.75)
    ipc.command("focusstack", {"value": 1})
    timed_pause(0.75)
    ipc.command("movestack", {"value": 1})
    timed_pause(max(1.0, float(scene["duration"]) - 1.5))
    probe = recorder.stop_and_wait(clip)
    guard.update(False, windows.pids)
    workspace = wait_layout(ipc, scene["layout"])
    write_text_assets(scene, generated)
    return {
        "scene": scene["id"], "title": scene["title"], "backend": os.environ.get("JWM_BACKEND", "current-x11"),
        "success": True, "elapsed": round(time.monotonic() - started, 3), "video": str(clip),
        "assertions": {"managed_windows": len(managed), "layout": workspace.get("layout"), "ffprobe": probe},
        "manual_review_required": scene["status"] != "ready",
    }


def main() -> int:
    args = parse_args()
    signal.signal(signal.SIGTERM, _interrupt_from_signal)
    ipc = JwmIpc()
    environment = preflight(ipc)
    print(json.dumps(environment.as_dict(), ensure_ascii=False, indent=2))
    if args.preflight: return 0 if environment.ok else 2
    if not environment.ok: return 2
    version = ipc.query("get_version") or {}
    current_backend = str(version.get("backend", "x11rb"))
    if current_backend == "x11-xcb": current_backend = "xcb"
    if current_backend != args.backend:
        raise RuntimeError(f"running JWM backend is {current_backend!r}, requested {args.backend!r}; restart JWM explicitly before recording")
    if args.resolution and environment.screen != args.resolution:
        raise RuntimeError(f"session resolution is {environment.screen}, expected {args.resolution}")
    if not 1 <= args.fps <= 240: raise RuntimeError("--fps must be in 1..240")
    scenes = load_scenes(args)
    binary = demo_binary(args)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    generated = BASE / "generated"
    run_dir = generated / "runs" / stamp
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "environment.json").write_text(json.dumps(environment.as_dict(), indent=2) + "\n")
    reports: list[dict] = []
    guard = SessionGuard(ipc, ROOT, run_dir, args.backend)
    with guard:
        try:
            ipc.command("set_config", {"key": "behavior.recording_fps", "value": args.fps})
        except Exception as exc:
            print(f"warning: cannot hot-set recording FPS; using the running JWM configuration: {exc}", file=sys.stderr)
        config = ipc.query("get_config") or {}
        demo_tag = 1 << max(0, int(config.get("tags_length", 9)) - 1)
        ipc.command("view", {"tag": demo_tag})
        windows = DemoWindows(binary, ipc, run_dir)
        recorder = Recorder(ipc)
        try:
            for scene in scenes:
                try:
                    reports.append(run_scene(scene, ipc, recorder, windows, generated, guard))
                except Exception as exc:
                    try: ipc.command("stop_recording")
                    except Exception: pass
                    reports.append({"scene": scene["id"], "success": False, "error": str(exc)})
                finally:
                    windows.close()
                    guard.update(False, [])
        finally:
            windows.close()
    report = {"generated_at": datetime.now(timezone.utc).isoformat(), "requested_backend": args.backend, "results": reports}
    reports_dir = generated / "reports"
    reports_dir.mkdir(parents=True, exist_ok=True)
    (reports_dir / "run-report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    (reports_dir / "environment-report.json").write_text(json.dumps(environment.as_dict(), indent=2) + "\n")
    all_scenes = tomllib.loads((BASE / "manifest/scenes.toml").read_text())["scene"]
    result_by_id = {item["scene"]: item for item in reports}
    matrix = [{
        "id": item["id"], "title": item["title"], "status": item["status"],
        "x11rb": result_by_id.get(item["id"], {}).get("success") if args.backend == "x11rb" else None,
        "xcb": result_by_id.get(item["id"], {}).get("success") if args.backend == "xcb" else None,
        "video": result_by_id.get(item["id"], {}).get("video"),
    } for item in all_scenes]
    (reports_dir / "feature-matrix.json").write_text(json.dumps(matrix, ensure_ascii=False, indent=2) + "\n")
    rows = ["| Feature | Status | x11rb | xcb | Video |", "|---|---|---:|---:|---|"]
    for item in matrix:
        mark = lambda value: "pass" if value is True else ("fail" if value is False else "-")
        rows.append(f"| {item['title']} | {item['status']} | {mark(item['x11rb'])} | {mark(item['xcb'])} | {item['video'] or '-'} |")
    (reports_dir / "feature-matrix.md").write_text("\n".join(rows) + "\n")
    concat = generated / "editing/concat.txt"
    concat.parent.mkdir(parents=True, exist_ok=True)
    concat.write_text("".join(f"file '{item['video']}'\n" for item in reports if item.get("success")))
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if all(item.get("success") for item in reports) else 1


if __name__ == "__main__":
    try: raise SystemExit(main())
    except KeyboardInterrupt: raise SystemExit(130)
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
