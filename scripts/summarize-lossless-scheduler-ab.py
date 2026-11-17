#!/usr/bin/env python3
"""Summarize private real-pipeline scheduler evidence, never invent missing samples."""
import argparse
import json
import math
from pathlib import Path

MAX_REPORT_BYTES = 16 * 1024 * 1024
PERIOD_NS = 500_000_000
WINDOW_NS = 9_000_000_000
BENCHMARKS = {
    "static_lossless_scheduler_tcp_ab": (6, ("off", "legacy", "proposed")),
    "static_lossless_scheduler_factorial_tcp_ab": (8, (
        "quiet0_pace100", "quiet0_pace250", "quiet250_pace100", "quiet250_pace250")),
}


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


def exact_regressions(observations):
    first = min((at for at, sse in observations if sse == 0), default=None)
    if first is None:
        return {"regressed_frames_after_exact": None, "max_sse_after_exact": None}
    later = [sse for at, sse in observations if at > first]
    return {"regressed_frames_after_exact": sum(sse > 0 for sse in later),
            "max_sse_after_exact": max(later, default=0)}


def summarize(report):
    if type(report.get("schema_version")) is not int or report["schema_version"] != 1:
        raise ValueError("unsupported schema_version")
    benchmark = report.get("benchmark")
    if not isinstance(benchmark, str) or benchmark not in BENCHMARKS:
        raise ValueError("wrong benchmark")
    maximum_sessions, allowed_arms = BENCHMARKS[benchmark]
    sessions = report.get("sessions")
    if not isinstance(sessions, list) or not 1 <= len(sessions) <= maximum_sessions:
        raise ValueError("invalid bounded session count, including failed sessions")
    results = []
    orders = set()
    input_hashes = {}
    for session in sessions:
        arm = session.get("arm")
        if arm not in allowed_arms:
            raise ValueError("unknown arm")
        policy = None
        if benchmark == "static_lossless_scheduler_factorial_tcp_ab":
            supplied = session.get("policy")
            quiet, pace = arm.removeprefix("quiet").split("_pace")
            if not isinstance(supplied, dict) or set(supplied) != {"quiet_ms", "pace_ms", "supersession_guard"}:
                raise ValueError("factorial policy metadata required")
            if integer(supplied["quiet_ms"], "quiet_ms") != int(quiet) or integer(supplied["pace_ms"], "pace_ms") != int(pace) or supplied["supersession_guard"] is not True:
                raise ValueError("factorial policy does not match arm or guard")
            policy = dict(supplied)
        order = integer(session.get("order"), "order")
        if order >= maximum_sessions or order in orders:
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
        presented_quality = []
        seen_sequences = set()
        png_count = 0
        unknown_marker_frames = 0
        stage_samples = {name: [] for name in (
            "gl_readback", "validation", "validation_to_feedback", "feedback_dispatch",
            "feedback_started_to_presented")}
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
                if sequence > baseline_sequence:
                    presented_quality.append((present, sse))
                    if sse == 0:
                        exact_at.append(present)
            # All four non-overlapping fixture stages come from each same
            # post-baseline frame, whether or not its marker was identified.
            if sequence > baseline_sequence:
                times = [frame[key] for key in ("gl_started_ns", "gl_completed_ns",
                         "validation_completed_ns", "feedback_started_ns", "feedback_sent_ns")]
                if times != sorted(times):
                    raise ValueError("fixture stage timestamps moved backwards")
                for name, start, end in zip(
                        ("gl_readback", "validation", "validation_to_feedback", "feedback_dispatch"),
                        times, times[1:]):
                    stage_samples[name].append(end - start)
                if present is not None:
                    # A callback during dispatch is valid: use its start, not
                    # the return timestamp; never clamp a signed difference.
                    stage_samples["feedback_started_to_presented"].append(present - times[3])
            generation = frame.get("marker_generation")
            if generation is None:
                if sequence > baseline_sequence:
                    unknown_marker_frames += 1
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
                            "source_schedule_delay_ns": paint - tick, "source_preparation_ns": commit - paint,
                            "commit_dispatch_duration_ns": completed - commit})
        results.append({"arm": arm, "order": order, "policy": policy, "success": session["success"],
                        "failure": session.get("failure"), "planned_generation_budget": 16,
                        "submitted_generations": len(scheduled), "not_submitted": 16 - len(scheduled),
                        "presented_generations": len(matched), "missing_after_submission": len(scheduled) - len(matched),
                        "late_presented_generations": sum(row["late"] for row in samples),
                        "on_time_presented_generations": sum(row["presented_ns"] is not None and not row["late"] for row in samples),
                        "within_window_on_time_presented_generations": sum(row["presented_ns"] is not None and row["presented_ns"] <= WINDOW_NS and not row["late"] for row in samples),
                        "within_window_presented_generations": sum(row["presented_ns"] is not None and row["presented_ns"] <= WINDOW_NS for row in samples),
                        "within_window_missing_after_submission": sum(row["presented_ns"] is None or row["presented_ns"] > WINDOW_NS for row in samples),
                        "cleanup_only_presented_generations": sum(row["presented_ns"] is not None and row["presented_ns"] > WINDOW_NS for row in samples),
                        "within_window_scheduled_to_present": distribution([row["scheduled_to_present_ns"] for row in samples if row["presented_ns"] is not None and row["presented_ns"] <= WINDOW_NS]),
                        "within_window_commit_to_present": distribution([row["commit_to_present_ns"] for row in samples if row["presented_ns"] is not None and row["presented_ns"] <= WINDOW_NS]),
                        "scheduled_to_present": distribution([row["scheduled_to_present_ns"] for row in samples if row["presented_ns"] is not None]),
                        "commit_to_present": distribution([row["commit_to_present_ns"] for row in samples if row["presented_ns"] is not None]),
                        "source_schedule_delay": distribution([row["source_schedule_delay_ns"] for row in samples]),
                        "source_preparation": distribution([row["source_preparation_ns"] for row in samples]),
                        "commit_dispatch": distribution([row["commit_dispatch_duration_ns"] for row in samples]),
                        "fixture_stages": {name: distribution(values) for name, values in stage_samples.items()},
                        "first_static_exact_presented_ns": min(exact_at) if exact_at else None,
                        "within_window_first_static_exact_presented_ns": min((at for at in exact_at if at <= WINDOW_NS), default=None),
                        "static_quality_eventual": exact_regressions(presented_quality),
                        "static_quality_within_window": exact_regressions([(at, sse) for at, sse in presented_quality if at <= WINDOW_NS]),
                        "decoded_png_frames": png_count, "decoded_frames": len(frames),
                        "unknown_marker_frames": unknown_marker_frames, "samples": samples,
                        "host_stats": session.get("host_stats")})
    return {"schema_version": 1, "benchmark": report["benchmark"],
            "planned_sessions": maximum_sessions,
            "planned_generation_budget": maximum_sessions * 16,
            "reported_sessions": len(sessions),
            "unreported_session_generation_budget": (maximum_sessions - len(sessions)) * 16,
            "interpretation": "500ms scheduled deadlines; missing samples retained; nearest-rank quantiles only among observed presentations; commit means issued; host sent_bytes is payload including source envelope, excluding Noise/TCP; encode_ns and encoded codec counts include discarded attempts; host_stats otherwise unmodified",
            "sessions": results}


def public_summary(summary):
    """Explicit aggregate allowlist: never publish raw traces or failure strings."""
    allowed = ("arm", "order", "policy", "success", "planned_generation_budget", "submitted_generations",
               "not_submitted", "presented_generations", "missing_after_submission",
               "late_presented_generations", "scheduled_to_present", "commit_to_present",
               "first_static_exact_presented_ns", "decoded_png_frames", "decoded_frames",
               "source_schedule_delay", "source_preparation", "commit_dispatch", "fixture_stages",
               "unknown_marker_frames", "within_window_presented_generations",
               "within_window_missing_after_submission", "cleanup_only_presented_generations",
               "within_window_scheduled_to_present", "within_window_commit_to_present",
               "within_window_first_static_exact_presented_ns", "on_time_presented_generations",
               "within_window_on_time_presented_generations", "static_quality_eventual",
               "static_quality_within_window")
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
            "planned_sessions": summary["planned_sessions"],
            "planned_generation_budget": summary["planned_generation_budget"],
            "reported_sessions": summary["reported_sessions"],
            "unreported_session_generation_budget": summary["unreported_session_generation_budget"],
            "scope": "aggregate synthetic CI results; within_window is <=9s experiment time; unprefixed presentation metrics include cleanup; exact time is relative to post-first-JPEG setup baseline, not first capture; host counters include setup/shutdown, are not 9s throughput; sent_bytes is source-envelope payload; codec counts include discarded attempts",
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
    aggregate = public["sessions"][0]
    assert aggregate["source_schedule_delay"]["p50_ms"] == 10 / 1e6
    assert aggregate["source_preparation"]["p50_ms"] == 9 / 1e6
    assert aggregate["commit_dispatch"]["p50_ms"] == 1 / 1e6
    assert aggregate["fixture_stages"]["gl_readback"]["p50_ms"] == 10 / 1e6
    assert aggregate["fixture_stages"]["validation"]["p50_ms"] == 10 / 1e6
    assert aggregate["fixture_stages"]["feedback_dispatch"]["p50_ms"] == 5 / 1e6
    frame["presented_ns"] = 58  # callback may complete before dispatch returns
    during = summarize(report)["sessions"][0]
    assert during["presented_generations"] == 1
    assert during["fixture_stages"]["feedback_started_to_presented"]["p50_ms"] == 3 / 1e6
    frame["presented_ns"] = WINDOW_NS
    boundary = public_summary(summarize(report))["sessions"][0]
    assert boundary["within_window_presented_generations"] == 1
    frame["presented_ns"] = WINDOW_NS + 1
    cleanup = public_summary(summarize(report))["sessions"][0]
    assert cleanup["presented_generations"] == 1
    assert cleanup["within_window_presented_generations"] == 0
    assert cleanup["within_window_missing_after_submission"] == 1
    assert cleanup["cleanup_only_presented_generations"] == 1
    assert cleanup["within_window_commit_to_present"]["count"] == 0
    frame["static_sse"] = 0
    cleanup_exact = public_summary(summarize(report))["sessions"][0]
    assert cleanup_exact["first_static_exact_presented_ns"] == WINDOW_NS + 1
    assert cleanup_exact["within_window_first_static_exact_presented_ns"] is None
    frame["static_sse"] = 9
    frame["marker_generation"] = None
    unknown = public_summary(summarize(report))["sessions"][0]
    assert unknown["unknown_marker_frames"] == 1
    assert unknown["missing_after_submission"] == 1
    assert unknown["first_static_exact_presented_ns"] is None
    assert unknown["fixture_stages"]["gl_readback"]["count"] == 1
    frame["marker_generation"] = 0
    frame["presented_ns"] = PERIOD_NS + 1
    frame["gl_started_ns"] = 18
    assert summarize(report)["sessions"][0]["missing_after_submission"] == 1
    frame["gl_started_ns"] = 30
    report["sessions"][0]["baseline_sequence"] = 1
    before = summarize(report)["sessions"][0]
    assert before["missing_after_submission"] == 1
    assert before["fixture_stages"]["gl_readback"]["count"] == 0
    report["sessions"][0]["baseline_sequence"] = 0
    frame["presented_ns"] = None
    assert summarize(report)["sessions"][0]["missing_after_submission"] == 1
    marker.update(paint_started_ns=None, commit_issued_ns=None, commit_completed_ns=None)
    absent = summarize(report)["sessions"][0]
    assert absent["not_submitted"] == 16
    assert absent["source_schedule_delay"]["count"] == 0
    assert absent["source_schedule_delay"]["p50_ms"] is None
    marker.update(paint_started_ns=10, commit_issued_ns=19, commit_completed_ns=20)
    for invalid in (True, -1, 16):
        marker["generation"] = invalid
        try:
            summarize(report)
        except ValueError:
            pass
        else:
            raise AssertionError("invalid generation accepted")
    assert exact_regressions([(1, 9)])["max_sse_after_exact"] is None
    assert exact_regressions([(1, 0), (2, 3), (3, 0), (4, 7)]) == {
        "regressed_frames_after_exact": 2, "max_sse_after_exact": 7}
    assert exact_regressions([(1, 0), (2, 0)]) == {
        "regressed_frames_after_exact": 0, "max_sse_after_exact": 0}
    marker["generation"] = 0
    factorial = json.loads(json.dumps(report))
    factorial["benchmark"] = "static_lossless_scheduler_factorial_tcp_ab"
    template = factorial["sessions"][0]
    arms = BENCHMARKS[factorial["benchmark"]][1]
    factorial["sessions"] = []
    for order, arm in enumerate((*arms, *reversed(arms))):
        session = json.loads(json.dumps(template))
        quiet, pace = arm.removeprefix("quiet").split("_pace")
        session.update(order=order, arm=arm, policy={"quiet_ms":int(quiet), "pace_ms":int(pace), "supersession_guard":True})
        factorial["sessions"].append(session)
    aggregate = public_summary(summarize(factorial))
    assert aggregate["planned_generation_budget"] == 128
    assert aggregate["reported_sessions"] == 8
    assert aggregate["unreported_session_generation_budget"] == 0
    assert len(aggregate["sessions"]) == 8  # do not pool repeated arms
    factorial["sessions"].pop()
    assert public_summary(summarize(factorial))["unreported_session_generation_budget"] == 16
    factorial["sessions"][0]["policy"]["supersession_guard"] = False
    try:
        summarize(factorial)
    except ValueError:
        pass
    else:
        raise AssertionError("factorial guard disabled")
    factorial["sessions"][0]["policy"]["supersession_guard"] = True
    factorial["sessions"][0]["arm"] = "legacy"
    try:
        summarize(factorial)
    except ValueError:
        pass
    else:
        raise AssertionError("three-arm policy accepted as factorial policy")
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
