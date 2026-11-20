#!/usr/bin/env python3
"""Version-bound acceptance evidence; compilation never implies hardware support.

`record` binds an actual runner's result/logs to source, executable and config.
`audit` detects changed dependencies and missing/replaced evidence, retaining
historical results. `release` additionally enforces physical GPU and soak gates.
`daily` checks a separate daily-driver matrix without waivers or promotion of
isolated evidence to physical evidence.
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

DAILY_CASES = (
    ("compositor-isolated", "isolated", "wayland-nested", 0,
     "Owned compositor: map, focus, switch tag, capture, and cleanly exit."),
    ("session-host-isolated", "isolated", "host", 0,
     "Private session bus and portal host: request/reply, cancel, cleanup."),
    ("login-upgrade", "physical", "wayland-udev", 1,
     "Fresh isolated login on the installed candidate; verify actual executable and config."),
    ("rollback", "physical", "wayland-udev", 1,
     "Return to the known-good install and login again; verify the new session's identity."),
    ("dual-output-hotplug", "physical", "wayland-udev", 2,
     "Two physical outputs; reconnect one and verify geometry, focus and input recovery."),
    ("dual-output-suspend", "physical", "wayland-udev", 2,
     "Suspend/resume with two outputs; verify layout, keyboard, pointer and lock recovery."),
    ("ime", "physical", "wayland-udev", 1,
     "Real IME: candidate choice and committed text across native apps and a focus change."),
    ("portal", "physical", "wayland-udev", 1,
     "Real desktop portal client: share only the selected source, cancel and close."),
    ("basic-app", "physical", "wayland-udev", 1,
     "Launch, focus, type, copy/paste, switch tag, minimize/restore and close a real app."),
)
DAILY_DEPENDENCIES = {"src", "Cargo.toml", "Cargo.lock"}


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


def daily_gate(records, root=ROOT):
    """Require each case at its own level; evidence metadata is not attestation."""
    known = {case for case, *_ in DAILY_CASES}
    rows = []
    for case, level, backend, output_count, checklist in DAILY_CASES:
        attempts = []
        for record in records:
            if record.get("case") != case:
                continue
            status = record.get("status")
            scope = (record.get("level"), record.get("backend"))
            reasons = []
            if scope != (level, backend):
                state = "wrong-scope"
                reasons.append(f"requires {level}/{backend}")
            elif status == "skip" and isinstance(record.get("reason"), str) and record["reason"].strip():
                state = "skipped"
                reasons.append(record["reason"])
            elif status == "fail":
                state = "failed"
                reasons.append(record.get("reason") or "failure had no reason")
            else:
                try:
                    reasons.extend(audit(record, root)["invalidated_by"])
                    provenance = record.get("provenance", {})
                    dependencies = provenance.get("dependency_sha256", {})
                    if not DAILY_DEPENDENCIES.issubset(dependencies):
                        reasons.append("must fingerprint src, Cargo.toml and Cargo.lock")
                    if set(provenance.get("dependencies", ())) != set(dependencies):
                        reasons.append("dependency list and fingerprint keys must agree")
                    if not record.get("configs"):
                        reasons.append("no tested config identity")
                    if output_count:
                        hardware = record.get("hardware", {})
                        outputs = hardware.get("outputs", [])
                        names = {
                            item["name"].strip()
                            for item in outputs
                            if isinstance(item, dict)
                            and isinstance(item.get("name"), str)
                            and item["name"].strip()
                            and isinstance(item.get("mode"), str)
                            and item["mode"].strip()
                        }
                        vendor = hardware.get("gpu_vendor", "")
                        driver = hardware.get("driver", "")
                        if (
                            not isinstance(vendor, str)
                            or vendor.lower() not in ("intel", "amd", "nvidia")
                            or not isinstance(driver, str)
                            or not driver.strip()
                            or hardware.get("software_renderer") is not False
                            or not isinstance(outputs, list)
                            or len(names) < output_count
                        ):
                            reasons.append(
                                "requires a physical GPU/driver, software_renderer=false, "
                                f"and {output_count} distinct physical output(s) with modes"
                            )
                except (AttributeError, KeyError, OSError, TypeError, ValueError):
                    reasons.append("malformed or unreadable evidence")
                if status != "pass":
                    reasons.append("expected pass or fail/skip with a reason")
                state = "invalid" if reasons else "passed"
            attempts.append({
                "reported_status": status, "level": scope[0], "backend": scope[1],
                "state": state, "reasons": reasons,
            })
        states = {attempt["state"] for attempt in attempts}
        # A pass cannot hide a failure or stale pass supplied for the same scope.
        state = next(
            (value for value in ("failed", "invalid", "passed", "skipped", "wrong-scope")
             if value in states),
            "missing",
        )
        rows.append({
            "case": case, "level": level, "backend": backend, "checklist": checklist,
            "state": state, "attempts": attempts,
        })
    isolated_ready = all(r["state"] == "passed" for r in rows if r["level"] == "isolated")
    physical_ready = all(r["state"] == "passed" for r in rows if r["level"] == "physical")
    return {
        "daily_ready": isolated_ready and physical_ready,
        "isolated_ready": isolated_ready,
        "physical_ready": physical_ready,
        "cases": rows,
        "ignored_cases": sorted({
            str(r.get("case")) for r in records
            if not isinstance(r.get("case"), str) or r["case"] not in known
        }),
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
    for action in ("audit", "release", "daily"):
        child = sub.add_parser(action)
        child.add_argument("records", type=Path, nargs="*" if action == "daily" else "+")
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
    try:
        records = [json.loads(path.read_text()) for path in args.records]
    except (OSError, ValueError) as error:
        parser.error(f"cannot read evidence: {error}")
    if any(not isinstance(item, dict) for item in records):
        parser.error("evidence records must be JSON objects")
    if args.action == "daily":
        result = daily_gate(records)
        print(json.dumps(result, indent=2))
        return int(not result["daily_ready"])
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
