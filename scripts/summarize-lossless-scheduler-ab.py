#!/usr/bin/env python3
"""Summarize private real-pipeline scheduler evidence, never invent missing samples."""
import argparse
import json
import math
from pathlib import Path

MAX_REPORT_BYTES = 16 * 1024 * 1024
PERIOD_NS = 500_000_000


def integer(value, label):
    if type(value) is not int or value < 0 or value > 2**63 - 1:
        raise ValueError(f"invalid nonnegative integer: {label}")
    return value


def unique_object(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError(f"duplicate JSON key: {key}")
        out[key] = value
    return out


def reject_constant(value):
    raise ValueError(f"non-finite JSON value: {value}")


def distribution(values):
    if not values:
        return {"count": 0, "p50_ms": None, "p95_ms": None, "max_ms": None}
    ordered = sorted(values)
    return {"count": len(values), "p50_ms": ordered[math.ceil(len(values) * .5) - 1] / 1e6,
            "p95_ms": ordered[math.ceil(len(values) * .95) - 1] / 1e6,
            "max_ms": ordered[-1] / 1e6}


def summarize(report):
    if type(report.get("schema_version")) is not int or report["schema_version"] != 1:
        raise ValueError("unsupported schema_version")
    if report.get("benchmark") != "static_lossless_scheduler_tcp_ab":
        raise ValueError("wrong benchmark")
    sessions = report.get("sessions")
    if not isinstance(sessions, list) or not 1 <= len(sessions) <= 6:
        raise ValueError("expected 1..6 bounded sessions, including failed sessions")
    results = []
    orders = set()
    input_hashes = {}
    for session in sessions:
        arm = session.get("arm")
        if arm not in ("off", "legacy", "proposed"):
            raise ValueError("unknown arm")
        order = integer(session.get("order"), "order")
        if order in orders:
            raise ValueError("duplicate session order")
        orders.add(order)
        if type(session.get("success")) is not bool:
            raise ValueError("success must be boolean")
        markers, frames = session.get("markers"), session.get("frames")
        if not isinstance(markers, list) or len(markers) > 16:
            raise ValueError("marker budget")
        if not isinstance(frames, list) or len(frames) > 4096:
            raise ValueError("frame budget")
        baseline = session.get("baseline_sequence")
        if baseline is None and not session["success"]:
            baseline_sequence = 2**63 - 1  # infrastructure failed before baseline
        else:
            baseline_sequence = integer(baseline, "baseline_sequence")
        scheduled = {}
        marker_ids = set()
        for marker in markers:
            generation = integer(marker.get("generation"), "generation")
            if generation >= 16 or generation in marker_ids:
                raise ValueError("invalid or duplicate generation")
            marker_ids.add(generation)
            digest = marker.get("source_sha256")
            if not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
                raise ValueError("invalid source_sha256")
            if generation in input_hashes and input_hashes[generation] != digest:
                raise ValueError("input differs between arms")
            input_hashes[generation] = digest
            scheduled_ns = integer(marker.get("scheduled_ns"), "scheduled_ns")
            if all(marker.get(key) is None for key in ("paint_started_ns", "commit_issued_ns", "commit_completed_ns")):
                continue  # planned but never submitted; keep the denominator 16
            paint = integer(marker.get("paint_started_ns"), "paint_started_ns")
            commit = integer(marker.get("commit_issued_ns"), "commit_issued_ns")
            completed = integer(marker.get("commit_completed_ns"), "commit_completed_ns")
            if commit < paint or completed < commit:
                raise ValueError("commit before preparation")
            scheduled[generation] = (scheduled_ns, paint, commit, completed)
        matched = {}
        exact_at = []
        seen_sequences = set()
        png_count = 0
        for frame in frames:
            sequence = integer(frame.get("sequence"), "sequence")
            if sequence in seen_sequences:
                raise ValueError("duplicate frame sequence")
            seen_sequences.add(sequence)
            if type(frame.get("is_png")) is not bool:
                raise ValueError("is_png must be boolean")
            png_count += frame["is_png"]
            sse = integer(frame.get("static_sse"), "static_sse")
            for key in ("gl_started_ns", "gl_completed_ns", "validation_completed_ns", "feedback_started_ns", "feedback_sent_ns"):
                integer(frame.get(key), key)
            present = frame.get("presented_ns")
            if present is not None:
                present = integer(present, "presented_ns")
                if sse == 0 and sequence > baseline_sequence:
                    exact_at.append(present)
            generation = frame.get("marker_generation")
            if generation is None:
                continue
            generation = integer(generation, "marker_generation")
            if generation >= 16:
                raise ValueError("marker outside finite identity set")
            if generation not in scheduled or present is None:
                continue
            # A pre-experiment/pre-commit frame may resemble the initial marker;
            # never credit it even if its presentation completed later.
            if sequence <= baseline_sequence or frame["gl_started_ns"] < scheduled[generation][2] or present < scheduled[generation][2]:
                continue
            if generation not in matched or present < matched[generation][0]:
                matched[generation] = (present, sequence)
        samples = []
        for generation, (tick, paint, commit, completed) in sorted(scheduled.items()):
            pair = matched.get(generation)
            present = pair[0] if pair else None
            samples.append({"generation": generation, "sequence": pair[1] if pair else None,
                            "presented_ns": present, "late": present is not None and present > tick + PERIOD_NS,
                            "scheduled_to_present_ns": None if present is None else present - tick,
                            "commit_to_present_ns": None if present is None else present - commit,
                            "source_schedule_delay_ns": paint - tick, "commit_dispatch_duration_ns": completed - commit})
        results.append({"arm": arm, "order": order, "success": session["success"],
                        "failure": session.get("failure"), "planned_generation_budget": 16,
                        "submitted_generations": len(scheduled), "not_submitted": 16 - len(scheduled),
                        "presented_generations": len(matched), "missing_after_submission": len(scheduled) - len(matched),
                        "late_presented_generations": sum(row["late"] for row in samples),
                        "scheduled_to_present": distribution([row["scheduled_to_present_ns"] for row in samples if row["presented_ns"] is not None]),
                        "commit_to_present": distribution([row["commit_to_present_ns"] for row in samples if row["presented_ns"] is not None]),
                        "first_static_exact_presented_ns": min(exact_at) if exact_at else None,
                        "decoded_png_frames": png_count, "decoded_frames": len(frames), "samples": samples,
                        "host_stats": session.get("host_stats")})
    return {"schema_version": 1, "benchmark": report["benchmark"],
            "interpretation": "500ms scheduled deadlines; missing samples retained; nearest-rank quantiles only among observed presentations; commit means issued; host sent_bytes is payload including source envelope, excluding Noise/TCP; encode_ns and encoded codec counts include discarded attempts; host_stats otherwise unmodified",
            "sessions": results}


def public_summary(summary):
    """Explicit aggregate allowlist: never publish raw traces or failure strings."""
    allowed = ("arm", "order", "success", "planned_generation_budget", "submitted_generations",
               "not_submitted", "presented_generations", "missing_after_submission",
               "late_presented_generations", "scheduled_to_present", "commit_to_present",
               "first_static_exact_presented_ns", "decoded_png_frames", "decoded_frames")
    counters = ("encoded_jpeg", "encoded_png", "encode_ns", "jpeg_ns", "png_ns", "sent_bytes",
                "comparisons", "comparison_ns", "stale_png_dropped")
    sessions = []
    for row in summary["sessions"]:
        item = {key: row[key] for key in allowed}
        stats = row.get("host_stats")
        if isinstance(stats, dict):
            item["aggregate_host_counters"] = {key: integer(stats[key], key) for key in counters if key in stats}
        sessions.append(item)
    return {"schema_version": 1, "benchmark": summary["benchmark"],
            "scope": "aggregate synthetic CI results; not physical latency; sent_bytes is source-envelope payload; codec counts include discarded attempts",
            "sessions": sessions}


def self_test():
    marker = {"source_sha256": "a" * 64, "generation": 0, "scheduled_ns": 0, "paint_started_ns": 10, "commit_issued_ns": 19, "commit_completed_ns": 20}
    frame = {"sequence": 1, "is_png": False, "marker_generation": 0, "gl_started_ns": 30,
             "gl_completed_ns": 40, "validation_completed_ns": 50, "feedback_started_ns": 55, "feedback_sent_ns": 60,
             "presented_ns": PERIOD_NS + 1, "static_sse": 9}
    report = {"schema_version": 1, "benchmark": "static_lossless_scheduler_tcp_ab", "sessions": [
        {"arm": "legacy", "order": 0, "baseline_sequence": 0, "success": False, "failure": "retained failure", "markers": [marker], "frames": [frame]}]}
    result = summarize(report)["sessions"][0]
    assert result["late_presented_generations"] == 1 and result["not_submitted"] == 15
    public = public_summary(summarize(report))
    assert "retained failure" not in json.dumps(public)
    assert "samples" not in public["sessions"][0] and "failure" not in public["sessions"][0]
    frame["presented_ns"] = 58  # callback may complete before dispatch returns
    assert summarize(report)["sessions"][0]["presented_generations"] == 1
    frame["presented_ns"] = PERIOD_NS + 1
    frame["gl_started_ns"] = 18
    assert summarize(report)["sessions"][0]["missing_after_submission"] == 1
    frame["gl_started_ns"] = 30
    report["sessions"][0]["baseline_sequence"] = 1
    assert summarize(report)["sessions"][0]["missing_after_submission"] == 1
    report["sessions"][0]["baseline_sequence"] = 0
    frame["presented_ns"] = None
    assert summarize(report)["sessions"][0]["missing_after_submission"] == 1
    marker.update(paint_started_ns=None, commit_issued_ns=None, commit_completed_ns=None)
    assert summarize(report)["sessions"][0]["not_submitted"] == 16
    marker.update(paint_started_ns=10, commit_issued_ns=19, commit_completed_ns=20)
    for invalid in (True, -1, 16):
        marker["generation"] = invalid
        try:
            summarize(report)
        except ValueError:
            pass
        else:
            raise AssertionError("invalid generation accepted")
    print("scheduler summary self-test passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", nargs="?", type=Path)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--public-summary", action="store_true", help="emit only explicitly allowlisted aggregate metrics")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.report is None:
        parser.error("report path required")
    with args.report.open("rb") as source:
        raw = source.read(MAX_REPORT_BYTES + 1)
    if len(raw) > MAX_REPORT_BYTES:
        parser.error("report exceeds 16MiB")
    try:
        report = json.loads(raw, object_pairs_hook=unique_object, parse_constant=reject_constant)
        summary = summarize(report)
        print(json.dumps(public_summary(summary) if args.public_summary else summary, indent=2, allow_nan=False))
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
