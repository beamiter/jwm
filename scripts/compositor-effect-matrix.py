#!/usr/bin/env python3
"""Prepare, record and compare compositor effect cases with jwm-tool perf."""
import argparse
import copy
import csv
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # Python 3.10; Python 3.11+ needs no package.

EFFECTS = {
    "static": (False, False, False), "astronomy": (True, False, False),
    "glow": (False, True, False), "glass": (False, False, True),
    "astronomy-glow": (True, True, False), "all": (True, True, True),
}
RESOLUTIONS = {"1080p": (1920, 1080, 1), "4k": (3840, 2160, 1), "dual-1080p": (3840, 1080, 2)}


def cases():
    result = []
    for resolution, (width, height, monitors) in RESOLUTIONS.items():
        for effect, (astronomy, glow, glass) in EFFECTS.items():
            for workload in ["idle", "animation", "tag-switch", "window-move"]:
                modes = ["none", "slide", "fade", "cube", "coverflow"] if workload == "tag-switch" else ["none"]
                for mode in modes:
                    result.append({
                        "id": f"{resolution}-{effect}-{workload}-{mode}",
                        "resolution": f"{width}x{height}", "monitors": monitors,
                        "effect": effect, "workload": workload,
                        "settings": {"behavior.wallpaper_effect": "astronomy" if astronomy else "off",
                                     "behavior.border_glow_enabled": glow,
                                     "behavior.blur_enabled": glass,
                                     "behavior.transition_mode": mode},
                    })
    return result


def case_by_id(identifier):
    return next(case for case in cases() if case["id"] == identifier)


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def update_toml(text, settings):
    """Replace selected assignments; prove the rest of the parsed TOML is unchanged."""
    before = tomllib.loads(text)
    expected = copy.deepcopy(before)
    for path, value in settings.items():
        section, field = path.split(".")
        expected.setdefault(section, {})[field] = value
        lines = text.splitlines(keepends=True)
        start = next(i for i, line in enumerate(lines) if line.strip() == f"[{section}]") + 1
        end = next((i for i in range(start, len(lines)) if lines[i].lstrip().startswith("[")), len(lines))
        index = next((i for i in range(start, end) if lines[i].split("=", 1)[0].strip() == field), None)
        replacement = f"{field} = {json.dumps(value, ensure_ascii=False, allow_nan=False)}\n"
        if index is None:
            lines.insert(end, replacement)
        else:
            # Arrays can span lines in a generated template.
            stop = index + 1
            while True:
                try:
                    tomllib.loads("".join(lines[index:stop]))
                    break
                except tomllib.TOMLDecodeError:
                    if stop >= end:
                        raise ValueError(f"cannot isolate TOML assignment {path}")
                    stop += 1
            lines[index:stop] = [replacement]
        text = "".join(lines)
    if tomllib.loads(text) != expected:
        raise ValueError("configuration edit changed unrelated data")
    return text


def query(tool, name):
    raw = subprocess.check_output([tool, "msg", name, "--raw"], timeout=15)
    message = json.loads(raw)
    if not message.get("success"):
        raise ValueError(f"{name}: {message.get('error')}")
    return message["data"]


def topology(monitors):
    keys = ["x", "y", "w", "h", "scale", "refresh_mhz", "name"]
    return sorted([{key: monitor.get(key) for key in keys} for monitor in monitors],
                  key=lambda item: (item["x"], item["y"], item["name"] or ""))


def verify_case(case, config, live, monitors, backend):
    if case["settings"]["behavior.wallpaper_effect"] == "astronomy" and backend not in ("x11rb", "xcb"):
        raise ValueError("Astronomy is currently X11-only; a static Wayland fallback is not this case")
    for path, value in case["settings"].items():
        section, field = path.split(".")
        if config.get(section, {}).get(field) != value:
            raise ValueError(f"configuration does not match {path}={value!r}")
        if field in live and live[field] != value:
            raise ValueError(f"live override differs from the recorded file: {field}")
    if len(monitors) != case["monitors"]:
        raise ValueError("monitor count differs from the matrix case")
    width = max(m["x"] + m["w"] for m in monitors) - min(m["x"] for m in monitors)
    height = max(m["y"] + m["h"] for m in monitors) - min(m["y"] for m in monitors)
    if f"{width}x{height}" != case["resolution"]:
        raise ValueError("physical output extent differs from the matrix case")
    if case["effect"] in ("glass", "all") and not config.get("behavior", {}).get("frosted_glass_rules"):
        raise ValueError("glass case needs an explicit rule matching a visible fixture window")


def fnv_file(data):
    value = 0xCBF29CE484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def record(args):
    case = case_by_id(args.case)
    config_bytes = args.config.read_bytes()
    if len(config_bytes) > 4 * 1024 * 1024:
        raise ValueError("configuration exceeds the project's 4 MiB limit")
    config = tomllib.loads(config_bytes.decode())
    status, live, monitors = (query(args.tool, name) for name in ("get_status", "get_config", "get_monitors"))
    verify_case(case, config, live, monitors, status["backend"])
    command = json.loads(args.workload_json) if args.workload_json else None
    if case["workload"] != "idle" and not command:
        raise ValueError("non-idle cases require --workload-json with a bounded fixture command")
    if command is not None and (not isinstance(command, list) or not command or
                                not all(isinstance(arg, str) for arg in command)):
        raise ValueError("workload command must be a nonempty JSON argv array")
    args.out.mkdir(parents=True, exist_ok=False)
    report_path = args.out / "baseline.json"
    (args.out / "configuration.toml").write_bytes(config_bytes)
    metadata = {"schema_version": 1, "case": case, "status": "incomplete",
                "source_revision": args.revision, "effective_config_sha256": digest(live),
                "topology": topology(monitors), "workload_command": command,
                "settle_seconds": args.settle_seconds,
                "sampling": {"frames": args.frames, "warmup": args.warmup, "idle_seconds": args.idle_seconds},
                "gpu_timing": {"status": "unavailable", "reason": "jwm-tool perf does not measure GPU timer queries"}}
    worker = None
    try:
        with (args.out / "workload.log").open("wb") as log:
            if command:
                worker = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            time.sleep(args.settle_seconds)
            if worker and worker.poll() is not None:
                raise ValueError("workload exited before measurement")
            # Asset decoding/theme extraction can legitimately finish during
            # warmup. Start the invariant at the measurement boundary, and
            # keep both snapshots so a refused run is diagnosable.
            live = query(args.tool, "get_config")
            metadata["effective_config_sha256"] = digest(live)
            metadata["effective_config_before"] = live
            config_bytes = args.config.read_bytes()
            config = tomllib.loads(config_bytes.decode())
            verify_case(case, config, live, query(args.tool, "get_monitors"), status["backend"])
            (args.out / "configuration.toml").write_bytes(config_bytes)
            with (args.out / "record.log").open("wb") as log:
                subprocess.run([args.tool, "perf", "record", "--out", str(report_path),
                                "--frames", str(args.frames), "--warmup", str(args.warmup),
                                "--idle-seconds", str(args.idle_seconds)],
                               stdout=log, stderr=subprocess.STDOUT, check=True, timeout=330)
            baseline = json.loads(report_path.read_text())
            if baseline["label"]["config_fingerprint"] != fnv_file(config_bytes):
                raise ValueError("perf record used a different configuration file")
            after = query(args.tool, "get_config")
            metadata["effective_config_after"] = after
            if digest(after) != metadata["effective_config_sha256"]:
                raise ValueError("effective configuration changed during measurement")
            if topology(query(args.tool, "get_monitors")) != metadata["topology"]:
                raise ValueError("output topology changed during measurement")
            if args.config.read_bytes() != config_bytes:
                raise ValueError("configuration file changed during measurement")
            if worker and worker.poll() is not None:
                raise ValueError("workload stopped before the measurement ended")
            required = ["idle", "steady_frame", "damage_redraw"]
            missing = [key for key in required if baseline["scenarios"][key]["status"] != "recorded"]
            metadata["missing_scenarios"] = missing
            metadata["status"] = "incomplete" if missing else "recorded"
            metadata["software_renderer"] = any(name in baseline["label"]["gpu"].lower() for name in ("llvmpipe", "softpipe"))
    except Exception as error:
        metadata["error"] = str(error)
        raise
    finally:
        if worker:
            try:
                os.killpg(worker.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                worker.wait(3)
            except subprocess.TimeoutExpired:
                os.killpg(worker.pid, signal.SIGKILL)
                worker.wait(3)
            finally:
                try:
                    os.killpg(worker.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        (args.out / "matrix.json").write_text(json.dumps(metadata, indent=2) + "\n")
    return 0 if metadata["status"] == "recorded" else 2


def require_same_case(left, right):
    for key in ["case", "effective_config_sha256", "topology", "workload_command", "sampling", "settle_seconds"]:
        if left.get(key) != right.get(key):
            raise ValueError(f"incomparable matrix metadata: {key}")
    if left.get("status") != "recorded" or right.get("status") != "recorded":
        raise ValueError("incomplete measurements cannot pass a regression gate")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    plan = sub.add_parser("plan")
    plan.add_argument("--out", type=Path, required=True)
    prepare = sub.add_parser("prepare")
    prepare.add_argument("--case", required=True)
    prepare.add_argument("--template", type=Path, required=True)
    prepare.add_argument("--out", type=Path, required=True)
    prepare.add_argument("--wallpaper", type=Path, required=True)
    prepare.add_argument("--earth-texture", type=Path)
    prepare.add_argument("--window-class", default="Xmessage")
    capture = sub.add_parser("record")
    capture.add_argument("--case", required=True)
    capture.add_argument("--config", type=Path, required=True)
    capture.add_argument("--out", type=Path, required=True)
    capture.add_argument("--tool", default="jwm-tool")
    capture.add_argument("--revision", required=True)
    capture.add_argument("--workload-json")
    capture.add_argument("--frames", type=int, default=300)
    capture.add_argument("--warmup", type=int, default=60)
    capture.add_argument("--idle-seconds", type=int, default=10)
    capture.add_argument("--settle-seconds", type=float, default=5)
    compare = sub.add_parser("compare")
    compare.add_argument("baseline", type=Path)
    compare.add_argument("candidate", type=Path)
    compare.add_argument("--tool", default="jwm-tool")
    summary = sub.add_parser("summary")
    summary.add_argument("root", type=Path)
    summary.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.action == "plan":
        args.out.write_text(json.dumps({"schema_version": 1, "cases": cases()}, indent=2) + "\n")
    elif args.action == "prepare":
        case = case_by_id(args.case)
        settings = dict(case["settings"])
        settings["behavior.wallpaper"] = str(args.wallpaper.resolve(strict=True))
        if settings["behavior.wallpaper_effect"] == "astronomy":
            if not args.earth_texture:
                parser.error("Astronomy requires --earth-texture")
            settings["behavior.wallpaper_earth_texture"] = str(args.earth_texture.resolve(strict=True))
        settings["behavior.frosted_glass_rules"] = [f"1:{args.window_class}"] if case["effect"] in ("glass", "all") else []
        text = update_toml(args.template.read_text(), settings)
        with args.out.open("x") as out:
            out.write(text)
    elif args.action == "record":
        if args.frames < 1 or args.warmup < 0 or args.idle_seconds < 1 or args.settle_seconds < 0:
            parser.error("invalid sampling limits")
        return record(args)
    elif args.action == "compare":
        require_same_case(json.loads((args.baseline / "matrix.json").read_text()), json.loads((args.candidate / "matrix.json").read_text()))
        return subprocess.call([args.tool, "perf", "compare", str(args.baseline / "baseline.json"), str(args.candidate / "baseline.json")])
    else:
        fields = ["case", "status", "renderer", "cpu_percent", "rss_mb", "frame_avg_ms", "frame_p95_ms", "frame_p99_ms", "damage_percent", "gpu_time_ms"]
        with args.out.open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=fields)
            writer.writeheader()
            for path in sorted(args.root.rglob("matrix.json")):
                meta = json.loads(path.read_text())
                baseline_path = path.with_name("baseline.json")
                baseline = json.loads(baseline_path.read_text()) if baseline_path.exists() else {}
                scenarios = baseline.get("scenarios", {})
                metric = lambda section, key: scenarios.get(section, {}).get("metrics", {}).get(key)
                writer.writerow({"case": meta["case"]["id"], "status": meta["status"], "renderer": baseline.get("label", {}).get("gpu"),
                                 "cpu_percent": metric("idle", "cpu_percent_avg"), "rss_mb": metric("idle", "rss_mb"),
                                 "frame_avg_ms": metric("steady_frame", "frame_time_avg_ms"), "frame_p95_ms": metric("steady_frame", "frame_time_p95_ms"),
                                 "frame_p99_ms": metric("steady_frame", "frame_time_p99_ms"), "damage_percent": metric("damage_redraw", "dirty_fraction_avg_percent"),
                                 "gpu_time_ms": None})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
