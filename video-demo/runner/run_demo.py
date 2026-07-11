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
    from runner.input_driver import XdotoolInput
    from runner.postproduction import assemble_silent, assemble_voice, generate_assets, run_tts
    from runner.jwm_ipc import JwmIpc
    from runner.recorder import Recorder
    from runner.session_guard import SessionGuard
else:
    from .demo_windows import DemoWindows
    from .environment import preflight
    from .input_driver import XdotoolInput
    from .postproduction import assemble_silent, assemble_voice, generate_assets, run_tts
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
    selection.add_argument("--scenes", help="comma-separated scene IDs")
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--demo-client", type=Path)
    parser.add_argument("--build-demo-client", action="store_true", help="build the release demo client")
    parser.add_argument("--allow-non-ready", action="store_true")
    parser.add_argument("--resolution", help="Expected session resolution, e.g. 1920x1080")
    parser.add_argument("--fps", type=int, default=60)
    parser.add_argument("--generate-assets", action="store_true", help="generate narration, subtitles, timeline and chapters without touching X11")
    parser.add_argument("--assemble", action="store_true", help="assemble selected clips into final-silent.mp4")
    parser.add_argument("--voice", action="store_true", help="also assemble final-with-voice.mp4 from generated/voice WAV files")
    parser.add_argument("--tts-command", help="external TTS command template using {text}, {output}, and {scene}")
    return parser.parse_args()


def load_scenes(args: argparse.Namespace) -> list[dict]:
    data = tomllib.loads((BASE / "manifest/scenes.toml").read_text())
    scenes = {item["id"]: item for item in data["scene"]}
    ids = ([args.scene] if args.scene else
           [item.strip() for item in args.scenes.split(",") if item.strip()] if args.scenes else
           [item["id"] for item in data["scene"] if item["status"] == "ready"] if args.profile == "production" else
           data["profiles"].get(args.profile))
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
    path = args.demo_client or BASE / "demo-client/target/release/jwm-demo-client"
    if args.build_demo_client or not path.exists():
        subprocess.run(["cargo", "build", "--release", "--manifest-path", str(BASE / "demo-client/Cargo.toml")], cwd=ROOT, check=True)
    if not path.exists(): raise RuntimeError(f"demo client not found: {path}")
    return path.resolve()


def wait_layout(ipc: JwmIpc, layout: str, timeout: float = 5.0) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        focused = next((item for item in (ipc.query("get_workspaces") or []) if item.get("focused")), None)
        if focused and layout in str(focused.get("layout", "")).lower(): return focused
        time.sleep(0.05)
    actual = focused_workspace(ipc)
    raise TimeoutError(f"workspace did not enter {layout} layout; current state: {actual}")


def layout_name(value: object) -> str:
    text = str(value).lower()
    return next((name for name in ("centeredmaster", "fibonacci", "fullscreen", "scrolling", "threecol", "monocle", "vstack", "tatami", "bstack", "grid", "deck", "float", "tile") if name in text), "")


def ensure_layout(ipc: JwmIpc, layout: str) -> dict:
    current = focused_workspace(ipc)
    if layout_name(current.get("layout")) != layout:
        ipc.command("setlayout", {"layout": layout})
    return wait_layout(ipc, layout)


def focused_workspace(ipc: JwmIpc) -> dict:
    workspace = next((item for item in (ipc.query("get_workspaces") or []) if item.get("focused")), None)
    if not workspace:
        raise RuntimeError("JWM did not report a focused workspace")
    return workspace


def demo_window_list(ipc: JwmIpc) -> list[dict]:
    return [item for item in (ipc.query("get_windows") or []) if str(item.get("class", "")).lower() == "jwmdemo"]


def focused_demo_window(ipc: JwmIpc) -> dict | None:
    return next((item for item in demo_window_list(ipc) if item.get("is_focused")), None)


def demo_window_by_id(ipc: JwmIpc, window_id: int) -> dict | None:
    return next((item for item in demo_window_list(ipc) if int(item["id"]) == window_id), None)


def select_tag(ipc: JwmIpc, tag: int) -> None:
    if int(focused_workspace(ipc)["tag_mask"]) != tag:
        ipc.command("view", {"tag": tag})
        wait_until("requested tag selected", lambda: int(focused_workspace(ipc)["tag_mask"]) == tag)


def tree_window_index(ipc: JwmIpc, window_id: int) -> tuple[int, int] | None:
    for monitor_index, node in enumerate(ipc.query("get_tree") or []):
        for client_index, window in enumerate(node.get("windows", [])):
            if int(window["id"]) == window_id:
                return monitor_index, client_index
    return None


def wait_until(description: str, predicate, timeout: float = 3.0) -> object:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.04)
    raise TimeoutError(f"state did not converge: {description}")


def resolve_args(value: object, demo_tag: int, alternate_tag: int) -> object:
    if value == "$demo_tag": return demo_tag
    if value == "$alternate_tag": return alternate_tag
    if isinstance(value, dict): return {key: resolve_args(item, demo_tag, alternate_tag) for key, item in value.items()}
    if isinstance(value, list): return [resolve_args(item, demo_tag, alternate_tag) for item in value]
    return value


def uses_symbol(value: object, symbol: str) -> bool:
    if value == symbol: return True
    if isinstance(value, dict): return any(uses_symbol(item, symbol) for item in value.values())
    if isinstance(value, list): return any(uses_symbol(item, symbol) for item in value)
    return False


def occupied_user_tags(windows: list[dict], tags_length: int) -> int:
    all_tags = (1 << tags_length) - 1
    occupied = 0
    for window in windows:
        if str(window.get("class", "")).lower() == "jwmdemo":
            continue
        tags = int(window.get("tags", 0))
        # Bars and shell windows intentionally appear on every tag. They do
        # not contain private per-workspace content and must not make every
        # candidate look occupied.
        if tags == all_tags and bool(window.get("is_floating")):
            continue
        occupied |= tags
    return occupied


def choose_unused_tag(ipc: JwmIpc, tags_length: int, demo_tag: int, original_tag: int | None) -> int:
    occupied = occupied_user_tags(ipc.query("get_windows") or [], tags_length)
    for index in range(tags_length - 1, -1, -1):
        candidate = 1 << index
        if candidate != demo_tag and candidate != original_tag and not occupied & candidate:
            return candidate
    raise RuntimeError("no unused secondary tag is available for safe tag automation")


def restore_workspace_baseline(ipc: JwmIpc, tag: int, baseline: dict) -> None:
    select_tag(ipc, tag)
    layout = layout_name(baseline["layout"]) or "tile"
    ensure_layout(ipc, layout)
    current = focused_workspace(ipc)
    target_mfact = float(baseline["m_fact"])
    if abs(float(current["m_fact"]) - target_mfact) > 0.0001:
        ipc.command("setmfact", {"value": 1.0 + target_mfact})
    current_nmaster = int(focused_workspace(ipc)["n_master"])
    target_nmaster = int(baseline["n_master"])
    if current_nmaster != target_nmaster:
        ipc.command("incnmaster", {"value": target_nmaster - current_nmaster})


EFFECT_CONFIG_KEYS = ("corner_radius", "shadow_enabled", "blur_enabled", "fading", "wobbly_windows", "motion_trail")


def restore_effect_baseline(ipc: JwmIpc, baseline: dict) -> None:
    current = ipc.query("get_config") or {}
    for key in EFFECT_CONFIG_KEYS:
        if key in baseline and current.get(key) != baseline[key]:
            ipc.command("set_config", {"key": f"behavior.{key}", "value": baseline[key]})
    status = ipc.query("get_effect_status") or {}
    for mode, command in (("overview", "toggle_overview"), ("magnifier", "toggle_magnifier"), ("annotation", "toggle_annotation")):
        if status.get(mode): ipc.command(command)


def execute_action(action: dict, ipc: JwmIpc, windows: DemoWindows, input_driver: XdotoolInput, demo_tag: int, alternate_tag: int) -> dict:
    command = action["command"]
    args = resolve_args(action.get("args", {}), demo_tag, alternate_tag)
    before_workspace = focused_workspace(ipc)
    before_window = focused_demo_window(ipc)
    before_tree_index = tree_window_index(ipc, before_window["id"]) if before_window else None
    before_windows = demo_window_list(ipc)
    effect_before = ipc.query("get_effect_status") or {}
    dispatched_command = command
    if command == "spawn_demo":
        old_count = len(before_windows)
        windows.spawn(int(action.get("count", 1)), str(action.get("content", "grid")), float(action.get("opacity", 1.0)))
        response = {"managed": len(windows.wait_managed(old_count + int(action.get("count", 1))))}
    elif command == "drag_demo":
        if not before_window: raise RuntimeError("drag_demo requires a focused demo window")
        start = (int(before_window["x"] + before_window["w"] / 2), int(before_window["y"] + 24))
        monitor = next(item for item in (ipc.query("get_monitors") or []) if item.get("focused"))
        end = (min(start[0] + 360, int(monitor["x"] + monitor["w"] - 80)), min(start[1] + 180, int(monitor["y"] + monitor["h"] - 80)))
        input_driver.drag(start, end, float(action.get("duration", 2.0)))
        response = {"from": start, "to": end}
    elif command == "pointer_path":
        monitor = next(item for item in (ipc.query("get_monitors") or []) if item.get("focused"))
        start = (int(monitor["x"] + monitor["w"] * 0.25), int(monitor["y"] + monitor["h"] * 0.3))
        end = (int(monitor["x"] + monitor["w"] * 0.75), int(monitor["y"] + monitor["h"] * 0.7))
        response = {"points": len(input_driver.smooth(start, end, duration=float(action.get("duration", 2.0))))}
    elif command == "annotation_draw":
        monitor = next(item for item in (ipc.query("get_monitors") or []) if item.get("focused"))
        start = (int(monitor["x"] + monitor["w"] * 0.3), int(monitor["y"] + monitor["h"] * 0.4))
        end = (int(monitor["x"] + monitor["w"] * 0.7), int(monitor["y"] + monitor["h"] * 0.6))
        input_driver.drag(start, end, float(action.get("duration", 2.0)), modifier=None)
        response = {"from": start, "to": end}
    elif command == "focus_demo":
        if len(before_windows) < 2:
            raise RuntimeError("focus_demo requires at least two demo windows")
        current_index = next((index for index, item in enumerate(before_windows) if before_window and item["id"] == before_window["id"]), -1)
        target = before_windows[(current_index + 1) % len(before_windows)]
        dispatched_command = "focus_window"
        args = {"id": target["id"]}
        response = ipc.command(dispatched_command, args)
        wait_until("target demo window focused", lambda: (window := focused_demo_window(ipc)) and window["id"] == target["id"])
    else:
        response = ipc.command(command, args)

    if command in ("focusstack", "scrolling_focus_column", "scrolling_focus_window") and before_window:
        wait_until("focused demo window changed", lambda: (window := focused_demo_window(ipc)) and window["id"] != before_window["id"])
    elif command == "setmfact":
        old = float(before_workspace["m_fact"])
        wait_until("master factor changed", lambda: abs(float(focused_workspace(ipc)["m_fact"]) - old) > 0.0001)
    elif command == "incnmaster":
        old = int(before_workspace["n_master"])
        wait_until("master count changed", lambda: int(focused_workspace(ipc)["n_master"]) != old)
    elif command == "setcfact" and before_window:
        old_geometry = tuple(before_window[key] for key in ("x", "y", "w", "h"))
        wait_until(
            "focused window geometry changed after client factor adjustment",
            lambda: any(item["id"] == before_window["id"] and tuple(item[key] for key in ("x", "y", "w", "h")) != old_geometry for item in demo_window_list(ipc)),
        )
    elif command in ("togglefloating", "togglesticky", "togglepip") and before_window:
        field = {"togglefloating": "is_floating", "togglesticky": "is_sticky", "togglepip": "is_pip"}[command]
        old = bool(before_window.get(field))
        wait_until(f"{field} toggled", lambda: (window := demo_window_by_id(ipc, before_window["id"])) and bool(window.get(field)) != old)
    elif command == "view":
        target = int(args["tag"])
        wait_until("view tag changed", lambda: int(focused_workspace(ipc)["tag_mask"]) == target)
    elif command in ("tag", "toggletag") and before_window:
        target = int(args["tag"])
        wait_until("window tag changed", lambda: any(item["id"] == before_window["id"] and int(item["tags"]) & target for item in demo_window_list(ipc)))
    elif command == "movestack" and before_window:
        wait_until(
            "focused window order changed after reorder",
            lambda: tree_window_index(ipc, before_window["id"]) != before_tree_index,
        )
    elif command == "set_config":
        key = str(args["key"]).split(".")[-1]
        wait_until(f"effect config {key} applied", lambda: (ipc.query("get_effect_status") or {}).get(key) == args["value"])
    elif command in ("toggle_overview", "toggle_magnifier", "toggle_annotation"):
        field = command.removeprefix("toggle_")
        wait_until(f"{field} mode toggled", lambda: (ipc.query("get_effect_status") or {}).get(field) != effect_before.get(field))

    timed_pause(float(action.get("hold", 0.45)))
    after_window = focused_demo_window(ipc)
    return {
        "command": command, "dispatched_command": dispatched_command, "args": args, "response": response,
        "focused_before": before_window["id"] if before_window else None,
        "focused_after": after_window["id"] if after_window else None,
        "target_window_id": before_window["id"] if before_window else None,
        "window_count_before": len(before_windows),
        "workspace_before": before_workspace,
        "workspace_after": focused_workspace(ipc),
        "effect_before": effect_before, "effect_after": ipc.query("get_effect_status") or {},
    }


def timed_pause(seconds: float) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline: time.sleep(min(0.05, deadline - time.monotonic()))


def action_time_budget(actions: list[dict]) -> float:
    return sum(float(action.get("hold", 0.45)) + float(action.get("duration", 0.0)) for action in actions)


def write_text_assets(scene: dict, generated: Path) -> None:
    narration = generated / "narration" / f"{scene['id']}.txt"
    subtitles = generated / "subtitles" / f"{scene['id']}.srt"
    narration.parent.mkdir(parents=True, exist_ok=True)
    subtitles.parent.mkdir(parents=True, exist_ok=True)
    text = scene["narration"].strip()
    narration.write_text(text + "\n")
    seconds = max(1, int(float(scene["duration"])))
    subtitles.write_text(f"1\n00:00:00,000 --> 00:00:{seconds:02d},000\n{text}\n")


def run_scene(scene: dict, ipc: JwmIpc, recorder: Recorder, windows: DemoWindows, input_driver: XdotoolInput, generated: Path, guard: SessionGuard, demo_tag: int, alternate_tag: int) -> dict:
    started = time.monotonic()
    clip = (generated / "clips" / f"{scene['id']}.mp4").resolve()
    if clip.exists():
        archive = clip.with_suffix(f".{int(time.time())}.mp4")
        clip.rename(archive)
    select_tag(ipc, demo_tag)
    windows.spawn(int(scene["windows"]), scene.get("content", "grid"), float(scene.get("opacity", 1.0)))
    guard.update(False, windows.pids)
    managed = windows.wait_managed(int(scene["windows"]))
    workspace = ensure_layout(ipc, scene["layout"])
    recorder.start(clip)
    guard.update(True, windows.pids)
    initial_focus = focused_demo_window(ipc)
    traces = []
    timed_pause(0.6)
    for action in scene.get("actions", [{"command": "focusstack", "args": {"value": 1}, "hold": 0.6}]):
        traces.append(execute_action(action, ipc, windows, input_driver, demo_tag, alternate_tag))
    used = 0.6 + action_time_budget(scene.get("actions", []))
    timed_pause(max(0.8, float(scene["duration"]) - used))
    probe = recorder.stop_and_wait(clip)
    guard.update(False, windows.pids)
    select_tag(ipc, demo_tag)
    workspace = wait_layout(ipc, scene["layout"])
    final_focus = focused_demo_window(ipc)
    assertions = {
        "managed_windows": len(managed), "layout": workspace.get("layout"), "ffprobe": probe,
        "actions_executed": len(traces),
    }
    for assertion in scene.get("assertions", []):
        if assertion == "focus_changed":
            assertions[assertion] = bool(initial_focus and final_focus and initial_focus["id"] != final_focus["id"])
        elif assertion == "m_fact_changed":
            assertions[assertion] = any(trace["command"] == "setmfact" and abs(float(trace["workspace_after"]["m_fact"]) - float(trace["workspace_before"]["m_fact"])) > 0.0001 for trace in traces)
        elif assertion == "n_master_changed":
            assertions[assertion] = any(trace["command"] == "incnmaster" and int(trace["workspace_after"]["n_master"]) != int(trace["workspace_before"]["n_master"]) for trace in traces)
        elif assertion == "focused_floating":
            target = next((trace["target_window_id"] for trace in traces if trace["command"] == "togglefloating"), None)
            assertions[assertion] = bool(target and (window := demo_window_by_id(ipc, target)) and window.get("is_floating"))
        elif assertion == "focused_sticky":
            target = next((trace["target_window_id"] for trace in traces if trace["command"] == "togglesticky"), None)
            assertions[assertion] = bool(target and (window := demo_window_by_id(ipc, target)) and window.get("is_sticky"))
        elif assertion == "focused_pip":
            target = next((trace["target_window_id"] for trace in traces if trace["command"] == "togglepip"), None)
            assertions[assertion] = bool(target and (window := demo_window_by_id(ipc, target)) and window.get("is_pip"))
        elif assertion == "window_on_alternate_tag":
            assertions[assertion] = any(int(item.get("tags", 0)) & alternate_tag for item in demo_window_list(ipc))
        elif assertion == "effect_changed":
            assertions[assertion] = any(trace["effect_before"] != trace["effect_after"] for trace in traces)
        if assertion in assertions and not assertions[assertion]:
            raise AssertionError(f"scene assertion failed: {assertion}")
    write_text_assets(scene, generated)
    return {
        "scene": scene["id"], "title": scene["title"], "backend": os.environ.get("JWM_BACKEND", "current-x11"),
        "success": True, "elapsed": round(time.monotonic() - started, 3), "video": str(clip),
        "assertions": assertions, "action_trace": traces,
        "manual_review_required": bool(scene.get("manual_review", scene["status"] != "ready")),
    }


def main() -> int:
    args = parse_args()
    signal.signal(signal.SIGTERM, _interrupt_from_signal)
    if args.voice and not args.assemble:
        raise RuntimeError("--voice requires --assemble")
    if args.generate_assets or args.assemble or args.tts_command or args.voice:
        scenes = load_scenes(args)
        generated = BASE / "generated"
        assets = generate_assets(scenes, generated)
        if args.tts_command: run_tts(assets, generated, args.tts_command)
        outputs = {"scenes": len(assets), "assets": str((generated / "editing").resolve())}
        if args.assemble:
            outputs["final_silent"] = str(assemble_silent(generated).resolve())
            if args.voice: outputs["final_with_voice"] = str(assemble_voice(assets, generated).resolve())
        print(json.dumps(outputs, ensure_ascii=False, indent=2))
        return 0
    ipc = JwmIpc()
    environment = preflight(ipc)
    print(json.dumps(environment.as_dict(), ensure_ascii=False, indent=2))
    if args.preflight: return 0 if environment.ok else 2
    if not environment.ok: return 2
    version = ipc.query("get_version") or {}
    if version.get("build_profile") != "release":
        raise RuntimeError(
            f"running JWM is not a release build (reported {version.get('build_profile', 'unknown')!r}); "
            "run: jwm-tool rebuild --jwm-dir \"$PWD\""
        )
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
        select_tag(ipc, demo_tag)
        demo_baseline = focused_workspace(ipc)
        effect_baseline = ipc.query("get_config") or {}
        needs_alternate_tag = any(uses_symbol(scene.get("actions", []), "$alternate_tag") for scene in scenes)
        alternate_tag = (
            choose_unused_tag(ipc, int(config.get("tags_length", 9)), demo_tag, guard.original_tag)
            if needs_alternate_tag else demo_tag
        )
        windows = DemoWindows(binary, ipc, run_dir)
        input_driver = XdotoolInput()
        recorder = Recorder(ipc)
        try:
            for scene in scenes:
                try:
                    restore_workspace_baseline(ipc, demo_tag, demo_baseline)
                    restore_effect_baseline(ipc, effect_baseline)
                    reports.append(run_scene(scene, ipc, recorder, windows, input_driver, generated, guard, demo_tag, alternate_tag))
                except Exception as exc:
                    try: ipc.command("stop_recording")
                    except Exception: pass
                    reports.append({"scene": scene["id"], "success": False, "error": str(exc)})
                finally:
                    windows.close()
                    guard.update(False, [])
                    try: restore_workspace_baseline(ipc, demo_tag, demo_baseline)
                    except Exception as exc: print(f"warning: scene baseline restore failed: {exc}", file=sys.stderr)
                    try: restore_effect_baseline(ipc, effect_baseline)
                    except Exception as exc: print(f"warning: effect baseline restore failed: {exc}", file=sys.stderr)
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
