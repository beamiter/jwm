#!/usr/bin/env python3
"""Compare archived native runs with the fixed shared-shell performance budget."""
import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path, nargs="+")
    parser.add_argument("--budget", type=Path, default=ROOT / "docs/shared-shell-performance-budget.json")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    budget = json.loads(args.budget.read_text())
    maximum = budget["maximum"]
    report = {"budget": str(args.budget), "baseline": budget["baseline"], "runs": []}
    for path in args.results:
        result = json.loads(path.read_text())
        row = {"result": str(path), "binary_sha256": result["binary_sha256"], "checks": []}

        def check(name, value, limit):
            row["checks"].append({"metric": name, "actual": value, "maximum": limit,
                                  "passed": value <= limit})

        if result["status"] != "passed":
            raise ValueError(f"runtime acceptance did not pass: {path}")
        host = result["kernel_performance"]["host"]
        if host["sample_seconds"] < budget["sample_seconds"] or not host["wakeups"]["available"]:
            raise ValueError(f"full-duration kernel measurement required: {path}")
        check("idle_cpu_percent_one_core", host["cpu_percent_one_core"], maximum["idle_cpu_percent_one_core"])
        check("rss_bytes", host["rss_bytes"], maximum["rss_bytes"])
        check("wakeups_per_second", host["wakeups"]["wakeups_per_second"], maximum["wakeups_per_second"])
        latencies = result["latency_ms"]
        check("workspace_roundtrip_ms", latencies["workspace_click_roundtrip"], maximum["workspace_roundtrip_ms"])
        restores = {key: value for key, value in latencies.items() if key.startswith("task_restore_")}
        if len(restores) < 2:
            raise ValueError(f"both actual task restore measurements required: {path}")
        for key, value in restores.items():
            check(key + "_ms", value, maximum["restore_roundtrip_ms"])
        switch = result["frontend_switch_resources"]
        if switch["host_before"]["pid"] != switch["host_after"]["pid"]:
            raise ValueError(f"host changed during frontend switch: {path}")
        row["frontend_switch"] = {"roundtrip_ms": latencies["frontend_switch_roundtrip"],
            "host_peak_rss_bytes": switch["host_after"]["peak_process_lifetime_rss_kib"] * 1024,
            "top_before_peak_rss_bytes": switch["top_before"]["peak_process_lifetime_rss_kib"] * 1024,
            "top_after_peak_rss_bytes": switch["top_after"]["peak_process_lifetime_rss_kib"] * 1024,
            "peak_scope": "per-process lifetime high-water mark; host PID stays constant"}
        row["passed"] = all(check["passed"] for check in row["checks"])
        report["runs"].append(row)
    report["passed"] = all(row["passed"] for row in report["runs"])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"{'PASS' if report['passed'] else 'FAIL'}: {len(report['runs'])} measured runs; {args.output}")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
