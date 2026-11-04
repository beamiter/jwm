#!/usr/bin/env python3
"""Version-bound acceptance evidence; compilation never implies hardware support.

`record` binds an actual runner's result/logs to source, executable and config.
`audit` detects changed dependencies and missing/replaced evidence, retaining
historical results. `release` additionally enforces physical GPU and soak gates.
No command starts/stops the user's display, services, or applications.
"""

import argparse
import datetime as dt
import hashlib
import json
import platform
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
LEVELS = ("compiled", "runnable", "isolated", "physical")
STATUSES = ("pass", "fail", "skip")


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def dependency_hashes(paths, root=ROOT):
    """Directory membership is included: adding/removing code invalidates evidence."""
    result = {}
    for item in paths:
        path = root / item
        if not path.exists():
            result[item] = None
            continue
        if path.is_file():
            result[item] = digest(path)
            continue
        files = sorted(
            p
            for p in path.rglob("*")
            if p.is_file()
            and not {"target", ".git", "node_modules"}.intersection(p.parts)
        )
        content = "\n".join(f"{p.relative_to(root)}:{digest(p)}" for p in files)
        result[item] = hashlib.sha256(content.encode()).hexdigest()
    return result


def provenance(paths=()):
    return {
        "source_sha": command("git", "rev-parse", "HEAD"),
        "working_tree_dirty": bool(command("git", "status", "--porcelain")),
        "dependency_sha256": dependency_hashes(paths),
        "dependencies": list(paths),
        "toolchain": {
            "rustc": command("rustc", "--version"),
            "cargo": command("cargo", "--version"),
        },
        "kernel": platform.release(),
        "system": platform.platform(),
    }


def artifact(path):
    path = Path(path).resolve()
    return {"path": str(path), "sha256": digest(path)}


def audit(record, root=ROOT):
    reasons = []
    if record.get("format") != 2:
        reasons.append("unsupported or historical evidence format")
    prov = record.get("provenance", {})
    expected = prov.get("dependency_sha256")
    if not expected:
        reasons.append(
            "source dependencies were not fingerprinted; historical evidence"
        )
    else:
        current = dependency_hashes(prov["dependencies"], root)
        reasons.extend(
            f"source dependency changed: {name}"
            for name, value in expected.items()
            if current.get(name) != value
        )
        reasons.extend(
            f"source dependency was missing when recorded: {name}"
            for name, value in expected.items()
            if value is None
        )
    for item in [
        *record.get("binaries", []),
        *record.get("raw_evidence", []),
        *record.get("configs", []),
    ]:
        path = Path(item["path"])
        if not path.is_file() or digest(path) != item["sha256"]:
            reasons.append(f"artifact missing or changed: {path}")
    if record.get("status") not in STATUSES:
        reasons.append("invalid result status")
    if record.get("level") not in LEVELS:
        reasons.append("invalid validation level")
    if record.get("status") == "pass" and not record.get("raw_evidence"):
        reasons.append("passing claim has no raw evidence")
    if record.get("status") == "pass" and not record.get("binaries"):
        reasons.append("passing claim has no executable identity")
    return {
        "case": record.get("case"),
        "historical_status": record.get("status"),
        "current": not reasons,
        "invalidated_by": reasons,
    }


def release_gate(records, root=ROOT, waivers=None):
    reports = [audit(item, root) for item in records]
    physical = [
        r
        for r, checked in zip(records, reports)
        if checked["current"]
        and r.get("status") == "pass"
        and r.get("level") == "physical"
    ]
    qualified = [
        r
        for r in physical
        if r.get("hardware", {}).get("gpu_vendor", "").lower()
        in ("intel", "amd", "nvidia")
        and r.get("hardware", {}).get("outputs")
        and r.get("hardware", {}).get("driver")
        and not r.get("hardware", {}).get("software_renderer", False)
    ]
    vendors = {r["hardware"]["gpu_vendor"].lower() for r in qualified}
    failures = []
    if len(vendors) < 2:
        failures.append(
            "two physical GPU vendors required (software adapters do not count)"
        )
    required = {
        "login-upgrade",
        "bad-binary",
        "rollback",
        "crash-recovery",
        "active-exit",
    }
    missing = required - {r.get("case") for r in qualified}
    if missing:
        failures.append(
            "physical login lifecycle cases missing: " + ", ".join(sorted(missing))
        )
    # Seven days is elapsed time with explicit completed events, never a future plan.
    soak = [r for r in qualified if r.get("case") == "daily-soak"]
    now = dt.datetime.now(dt.timezone.utc)
    valid_soak = False
    for record in soak:
        try:
            start = dt.datetime.fromisoformat(record["hardware"]["soak_started_at"])
            end = dt.datetime.fromisoformat(record["hardware"]["soak_ended_at"])
            valid_soak |= (
                start.tzinfo is not None
                and end.tzinfo is not None
                and (now >= end >= start and (end - start).total_seconds() >= 7 * 86400)
            )
        except (KeyError, TypeError, ValueError):
            pass
    if not valid_soak:
        failures.append("completed seven-day physical daily-soak evidence required")
    if any(
        r.get("status") == "fail" or not check["current"]
        for r, check in zip(records, reports)
    ):
        failures.append("failed or stale candidate evidence remains")
    waiver_checks = {
        "second_gpu_vendor": "two physical GPU vendors required (software adapters do not count)",
        "seven_day_soak": "completed seven-day physical daily-soak evidence required",
    }
    waivers = waivers or {}
    waived_reasons = {
        waiver_checks[key]
        for key, reason in waivers.items()
        if key in waiver_checks and reason.strip()
    }
    blocking = [reason for reason in failures if reason not in waived_reasons]
    return {
        "release_ready": not failures,
        "hardware_certified": not failures,
        "candidate_ready": not blocking,
        "reasons": failures,
        "blocking_reasons": blocking,
        "user_waivers": {
            key: reason
            for key, reason in waivers.items()
            if key in waiver_checks and reason.strip()
        },
        "cases": reports,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    record = sub.add_parser("record")
    record.add_argument("--case", required=True)
    record.add_argument("--status", choices=STATUSES, required=True)
    record.add_argument("--level", choices=LEVELS, required=True)
    record.add_argument("--reason")
    record.add_argument("--backend", required=True)
    record.add_argument("--binary", type=Path, action="append", default=[])
    record.add_argument("--config", type=Path, action="append", default=[])
    record.add_argument("--evidence", type=Path, action="append", default=[])
    record.add_argument("--dependency", action="append", default=[])
    record.add_argument(
        "--hardware",
        type=Path,
        help="JSON: GPU/driver/output topology/scenario, optional soak timestamps",
    )
    record.add_argument("--output", type=Path, required=True)
    for action in ("audit", "release"):
        child = sub.add_parser(action)
        child.add_argument("records", type=Path, nargs="+")
        if action == "release":
            child.add_argument(
                "--waive",
                action="append",
                default=[],
                choices=("second_gpu_vendor", "seven_day_soak"),
            )
            child.add_argument(
                "--waiver-reason",
                help="Exact user authorization; waived gates never become passes",
            )
    args = parser.parse_args()
    if args.action == "record":
        if args.status == "pass" and (
            not args.evidence or not args.binary or not args.dependency
        ):
            parser.error("pass requires --evidence, --binary and --dependency")
        if args.status == "pass" and any(
            value is None for value in dependency_hashes(args.dependency).values()
        ):
            parser.error(
                "passing evidence cannot fingerprint a missing source dependency"
            )
        if args.status != "pass" and not args.reason:
            parser.error("fail/skip require --reason")
        if args.level == "physical" and not args.hardware:
            parser.error(
                "physical evidence requires --hardware describing actual devices/topology"
            )
        data = {
            "format": 2,
            "case": args.case,
            "status": args.status,
            "level": args.level,
            "reason": args.reason,
            "backend": args.backend,
            "recorded_at": dt.datetime.now(dt.timezone.utc).isoformat(),
            "provenance": provenance(args.dependency),
            "binaries": [artifact(p) for p in args.binary],
            "configs": [artifact(p) for p in args.config],
            "raw_evidence": [artifact(p) for p in args.evidence],
            "hardware": json.loads(args.hardware.read_text()) if args.hardware else {},
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(data, indent=2) + "\n")
        return 0
    records = [json.loads(path.read_text()) for path in args.records]
    if args.action == "release":
        if args.waive and not args.waiver_reason:
            parser.error(
                "--waive requires --waiver-reason with actual user authorization"
            )
        result = release_gate(
            records, waivers={key: args.waiver_reason for key in args.waive}
        )
        print(json.dumps(result, indent=2))
        return int(not result["candidate_ready"])
    result = [audit(r) for r in records]
    print(json.dumps(result, indent=2))
    return int(any(not r["current"] for r in result))


if __name__ == "__main__":
    sys.exit(main())
