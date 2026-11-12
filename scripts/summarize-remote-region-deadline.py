#!/usr/bin/env python3
"""Summarize bounded private 100ms region-deadline TCP/GL measurements."""
import argparse
import json
import math
from pathlib import Path

BENCHMARK = "static_lossless_region_deadline_continuous_tcp_ab"
ARMS = ("new250", "region_deadline", "region_deadline", "new250")
GENERATIONS = 80
PERIOD_NS = 100_000_000
WINDOW_NS = 8_000_000_000
MAX_REPORT_BYTES = 16 * 1024 * 1024
COUNTERS = ("encoded_jpeg", "encoded_png", "encode_ns", "jpeg_ns", "png_ns",
            "sent_bytes", "comparisons", "comparison_ns", "stale_png_dropped")


def integer(value, label):
    if type(value) is not int or not 0 <= value < 2**63:
        raise ValueError(f"invalid nonnegative integer: {label}")
    return value


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"non-finite JSON value: {value}")


def distribution(values):
    if not values:
        return {"count": 0, "p50_ms": None, "p95_ms": None, "max_ms": None}
    values = sorted(values)
    return {"count": len(values),
            "p50_ms": values[math.ceil(len(values) * .5) - 1] / 1e6,
            "p95_ms": values[math.ceil(len(values) * .95) - 1] / 1e6,
            "max_ms": values[-1] / 1e6}


def quality(observations):
    first = min((at for at, error in observations if error == 0), default=None)
    later = [error for at, error in observations if first is not None and at > first]
    return {"first_static_exact_presented_ns": first,
            "regressed_frames_after_exact": None if first is None else sum(error > 0 for error in later),
            "max_sse_after_exact": None if first is None else max(later, default=0)}


def counters(value):
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ValueError("host counters must be an object")
    return {key: integer(value[key], key) for key in COUNTERS if key in value}


def summarize(report):
    if type(report.get("schema_version")) is not int or report["schema_version"] != 1:
        raise ValueError("unsupported schema version")
    if report.get("benchmark") != BENCHMARK:
        raise ValueError("wrong benchmark")
    for key, expected in (("scheduled_interval_ns", PERIOD_NS), ("active_window_ns", WINDOW_NS),
                          ("generation_budget", GENERATIONS), ("drain_ns", 1_000_000_000),
                          ("width", 800), ("height", 600), ("dynamic_region_width", 32),
                          ("dynamic_region_height", 16), ("static_pixels", 479488),
                          ("maximum_png_batch_pixels", 65536)):
        if integer(report.get(key), key) != expected:
            raise ValueError(f"unexpected fixed workload: {key}")
    sessions = report.get("sessions")
    if not isinstance(sessions, list) or not 1 <= len(sessions) <= len(ARMS):
        raise ValueError("bounded sessions required, including failures")
    hashes, orders, results = {}, set(), []
    for session in sessions:
        order = integer(session.get("order"), "order")
        if order >= len(ARMS) or order in orders or session.get("arm") != ARMS[order]:
            raise ValueError("invalid mirrored session order or arm")
        orders.add(order)
        if type(session.get("success")) is not bool:
            raise ValueError("success must be boolean")
        markers, frames = session.get("markers"), session.get("frames")
        if not isinstance(markers, list) or len(markers) > GENERATIONS:
            raise ValueError("marker budget exceeded")
        if not isinstance(frames, list) or len(frames) > 512:
            raise ValueError("frame budget exceeded")
        baseline = session.get("baseline_sequence")
        baseline = 2**63 - 1 if baseline is None and not session["success"] else integer(baseline, "baseline_sequence")
        scheduled, marker_ids = {}, set()
        for marker in markers:
            generation = integer(marker.get("generation"), "generation")
            if generation >= GENERATIONS or generation in marker_ids:
                raise ValueError("invalid or duplicate generation")
            marker_ids.add(generation)
            if integer(marker.get("scheduled_ns"), "scheduled_ns") != generation * PERIOD_NS:
                raise ValueError("source schedule changed")
            digest = marker.get("source_sha256")
            if not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
                raise ValueError("invalid source SHA256")
            if generation in hashes and hashes[generation] != digest:
                raise ValueError("input differs between arms")
            hashes[generation] = digest
            stamps = [marker.get(key) for key in ("paint_started_ns", "commit_issued_ns", "commit_completed_ns")]
            if all(value is None for value in stamps):
                continue
            paint, commit, completed = [integer(value, "source timestamp") for value in stamps]
            if generation * PERIOD_NS > paint or paint > commit or commit > completed:
                raise ValueError("source timestamps moved backwards")
            scheduled[generation] = (paint, commit, completed)
        matched, sequences, observations = {}, set(), []
        png, presented_png, unknown, unscheduled = 0, 0, 0, 0
        stages = {key: [] for key in ("gl_readback", "validation", "validation_to_feedback",
                                     "feedback_dispatch", "feedback_started_to_presented")}
        for frame in frames:
            sequence = integer(frame.get("sequence"), "sequence")
            if sequence in sequences:
                raise ValueError("duplicate frame sequence")
            sequences.add(sequence)
            if type(frame.get("is_png")) is not bool:
                raise ValueError("is_png must be boolean")
            png += frame["is_png"]
            error = integer(frame.get("static_sse"), "static_sse")
            times = [integer(frame.get(key), key) for key in ("gl_started_ns", "gl_completed_ns",
                     "validation_completed_ns", "feedback_started_ns", "feedback_sent_ns")]
            if times != sorted(times):
                raise ValueError("fixture stages moved backwards")
            at = frame.get("presented_ns")
            if at is not None:
                at = integer(at, "presented_ns")
                if at < times[3]:
                    raise ValueError("presentation preceded feedback dispatch")
            if sequence <= baseline:
                continue
            for key, start, end in zip(stages, times, times[1:]):
                stages[key].append(end - start)
            if at is not None:
                stages["feedback_started_to_presented"].append(at - times[3])
                observations.append((at, error))
                presented_png += frame["is_png"] and at <= WINDOW_NS
            generation = frame.get("marker_generation")
            if generation is None:
                unknown += 1
                continue
            generation = integer(generation, "marker_generation")
            if generation >= 2**8:
                raise ValueError("marker outside finite identity set")
            # A valid but unscheduled identity is never relabeled as expected.
            if generation >= GENERATIONS:
                unscheduled += 1
            if generation not in scheduled or at is None:
                continue
            commit = scheduled[generation][1]
            if times[0] < commit or at < commit:
                continue
            if generation not in matched or at < matched[generation][0]:
                matched[generation] = (at, sequence)
        samples = []
        for generation in range(GENERATIONS):
            tick = generation * PERIOD_NS
            source = scheduled.get(generation)
            match = matched.get(generation)
            at = match[0] if match else None
            samples.append({"generation": generation, "submitted": source is not None,
                            "sequence": match[1] if match else None, "presented_ns": at,
                            "late": at is not None and at > tick + PERIOD_NS,
                            "cleanup_only": at is not None and at > WINDOW_NS,
                            "scheduled_to_present_ns": None if at is None else at - tick,
                            "commit_to_present_ns": None if at is None else at - source[1],
                            "source_schedule_delay_ns": None if source is None else source[0] - tick,
                            "source_preparation_ns": None if source is None else source[1] - source[0],
                            "commit_dispatch_duration_ns": None if source is None else source[2] - source[1]})
        active_rows = [row for row in samples if row["presented_ns"] is not None and not row["cleanup_only"]]
        active_quality = quality([(at, error) for at, error in observations if at <= WINDOW_NS])
        eventual_quality = quality(observations)
        first = active_quality["first_static_exact_presented_ns"]
        later = first is not None and any(scheduled[row["generation"]][1] > first for row in active_rows)
        static_criterion = (first is None and any(at >= 7_000_000_000 and error > 0 for at, error in observations if at <= WINDOW_NS)
                            if session["arm"] == "new250" else first is not None and later and presented_png > 1)
        commits = [source[1] for _, source in sorted(scheduled.items())]
        if commits != sorted(commits):
            raise ValueError("source commits moved backwards across generations")
        if session["success"] and len(scheduled) != GENERATIONS:
            raise ValueError("successful session did not submit its full generation budget")
        results.append({"arm": session["arm"], "order": order, "success": session["success"],
                        "failure": session.get("failure"), "planned_generation_budget": GENERATIONS,
                        "submitted_generations": len(scheduled), "not_submitted": GENERATIONS - len(scheduled),
                        "presented_generations": len(matched), "missing_after_submission": len(scheduled) - len(matched),
                        "late_presented_generations": sum(row["late"] for row in samples),
                        "within_window_presented_generations": len(active_rows),
                        "within_window_missing_after_submission": len(scheduled) - len(active_rows),
                        "within_window_missing_planned_generations": GENERATIONS - len(active_rows),
                        "within_window_on_time_presented_generations": sum(not row["late"] for row in active_rows),
                        "cleanup_only_presented_generations": sum(row["cleanup_only"] for row in samples),
                        "scheduled_to_present": distribution([row["scheduled_to_present_ns"] for row in samples if row["presented_ns"] is not None]),
                        "commit_to_present": distribution([row["commit_to_present_ns"] for row in samples if row["presented_ns"] is not None]),
                        "within_window_scheduled_to_present": distribution([row["scheduled_to_present_ns"] for row in active_rows]),
                        "within_window_commit_to_present": distribution([row["commit_to_present_ns"] for row in active_rows]),
                        "source_schedule_delay": distribution([row["source_schedule_delay_ns"] for row in samples if row["submitted"]]),
                        "source_preparation": distribution([row["source_preparation_ns"] for row in samples if row["submitted"]]),
                        "commit_dispatch": distribution([row["commit_dispatch_duration_ns"] for row in samples if row["submitted"]]),
                        "source_commit_gap": distribution([end - start for start, end in zip(commits, commits[1:])]),
                        "fixture_stages": {key: distribution(values) for key, values in stages.items()},
                        "static_quality_within_window": active_quality, "static_quality_eventual": eventual_quality,
                        "later_animation_presented_after_exact": later, "static_criterion": static_criterion,
                        "no_exact_to_inexact_regression": eventual_quality["regressed_frames_after_exact"] in (None, 0),
                        "decoded_png_frames": png, "within_window_presented_png_frames": presented_png,
                        "decoded_frames": len(frames), "unknown_marker_frames": unknown,
                        "unscheduled_marker_frames": unscheduled,
                        "aggregate_host_counters": counters(session.get("host_stats")),
                        "active_window_host_counters": counters(session.get("active_window_host_stats")),
                        "samples": samples})
    return {"schema_version": 1, "benchmark": BENCHMARK, "planned_sessions": len(ARMS),
            "planned_generation_budget": len(ARMS) * GENERATIONS, "reported_sessions": len(results),
            "unreported_session_generation_budget": (len(ARMS) - len(results)) * GENERATIONS,
            "scope": "100ms deadlines; active window <=8s, later presentations are cleanup only; quantiles include observed samples only and missing counts retain all 80 planned generations; static equality excludes exactly 32x16 animation pixels; source commit means issued; host payload includes source envelope, excludes Noise/TCP; encode counts include discarded attempts; host counters include common startup and active counters are last sampled <=8s",
            "sessions": results}


def public_summary(summary):
    # Explicit allowlist, never a denylist: future raw fields remain private.
    allowed = ("arm", "order", "success", "planned_generation_budget", "submitted_generations", "not_submitted",
               "presented_generations", "missing_after_submission", "late_presented_generations",
               "within_window_presented_generations", "within_window_missing_after_submission",
               "within_window_missing_planned_generations", "within_window_on_time_presented_generations",
               "cleanup_only_presented_generations", "scheduled_to_present", "commit_to_present",
               "within_window_scheduled_to_present", "within_window_commit_to_present", "source_schedule_delay",
               "source_preparation", "commit_dispatch", "source_commit_gap", "fixture_stages",
               "static_quality_within_window", "static_quality_eventual", "later_animation_presented_after_exact",
               "static_criterion", "no_exact_to_inexact_regression", "decoded_png_frames",
               "within_window_presented_png_frames", "decoded_frames", "unknown_marker_frames", "unscheduled_marker_frames",
               "aggregate_host_counters", "active_window_host_counters")
    result = {key: summary[key] for key in ("schema_version", "benchmark", "planned_sessions",
              "planned_generation_budget", "reported_sessions", "unreported_session_generation_budget", "scope")}
    result["sessions"] = [{key: session[key] for key in allowed} for session in summary["sessions"]]
    return result


def self_test():
    marker = {"generation": 0, "source_sha256": "a" * 64, "scheduled_ns": 0,
              "paint_started_ns": 10, "commit_issued_ns": 19, "commit_completed_ns": 20}
    frame = {"sequence": 1, "is_png": False, "marker_generation": 0, "gl_started_ns": 30,
             "gl_completed_ns": 40, "validation_completed_ns": 50, "feedback_started_ns": 55,
             "feedback_sent_ns": 60, "presented_ns": PERIOD_NS + 1, "static_sse": 9}
    session = {"arm": ARMS[0], "order": 0, "success": False, "failure": "private failure",
               "baseline_sequence": 0, "markers": [marker], "frames": [frame]}
    report = {"schema_version": 1, "benchmark": BENCHMARK, "scheduled_interval_ns": PERIOD_NS,
              "active_window_ns": WINDOW_NS, "generation_budget": GENERATIONS, "drain_ns": 1_000_000_000,
              "width": 800, "height": 600, "dynamic_region_width": 32, "dynamic_region_height": 16,
              "static_pixels": 479488, "maximum_png_batch_pixels": 65536, "sessions": [session]}
    row = summarize(report)["sessions"][0]
    assert len(row["samples"]) == 80 and row["not_submitted"] == 79
    assert row["late_presented_generations"] == 1 and row["scheduled_to_present"]["count"] == 1
    assert row["within_window_missing_planned_generations"] == 79
    assert row["source_preparation"]["max_ms"] == 9 / 1e6
    assert "private failure" not in json.dumps(public_summary(summarize(report)))
    assert "samples" not in public_summary(summarize(report))["sessions"][0]
    assert summarize(report)["unreported_session_generation_budget"] == 240
    frame["presented_ns"] = 58
    assert summarize(report)["sessions"][0]["fixture_stages"]["feedback_started_to_presented"]["max_ms"] == 3 / 1e6
    frame["presented_ns"] = WINDOW_NS
    assert summarize(report)["sessions"][0]["within_window_presented_generations"] == 1
    frame["presented_ns"] = WINDOW_NS + 1
    frame["static_sse"] = 0
    row = summarize(report)["sessions"][0]
    assert row["cleanup_only_presented_generations"] == 1
    assert row["within_window_missing_after_submission"] == 1
    assert row["static_quality_within_window"]["first_static_exact_presented_ns"] is None
    assert row["static_quality_eventual"]["first_static_exact_presented_ns"] == WINDOW_NS + 1
    frame["marker_generation"] = None
    row = summarize(report)["sessions"][0]
    assert row["unknown_marker_frames"] == 1 and row["missing_after_submission"] == 1
    frame["marker_generation"] = 0
    frame["gl_started_ns"] = 18
    assert summarize(report)["sessions"][0]["missing_after_submission"] == 1
    frame["gl_started_ns"] = 30
    frame["presented_ns"] = None
    assert summarize(report)["sessions"][0]["scheduled_to_present"]["p95_ms"] is None
    marker.update(paint_started_ns=None, commit_issued_ns=None, commit_completed_ns=None)
    assert summarize(report)["sessions"][0]["not_submitted"] == 80
    assert quality([(1, 0), (2, 4), (3, 0)])["regressed_frames_after_exact"] == 1
    assert quality([(1, 9)])["regressed_frames_after_exact"] is None
    for bad in (True, -1, 80):
        marker["generation"] = bad
        try:
            summarize(report)
        except ValueError:
            pass
        else:
            raise AssertionError("bad marker accepted")
    marker["generation"] = 0
    frame["marker_generation"] = 255
    assert summarize(report)["sessions"][0]["unscheduled_marker_frames"] == 1
    candidate = json.loads(json.dumps(report))
    candidate_session = candidate["sessions"][0]
    candidate_session.update(order=1, arm="region_deadline")
    candidate_session["markers"] = [dict(marker, paint_started_ns=10, commit_issued_ns=19,
                                         commit_completed_ns=20),
        dict(marker, generation=1, scheduled_ns=PERIOD_NS, paint_started_ns=PERIOD_NS + 10,
             commit_issued_ns=PERIOD_NS + 19, commit_completed_ns=PERIOD_NS + 20)]
    candidate_session["frames"] = [dict(frame, marker_generation=0, is_png=True,
                                        presented_ns=50_000_000, static_sse=0),
        dict(frame, sequence=2, marker_generation=1, is_png=True,
             gl_started_ns=PERIOD_NS + 30, gl_completed_ns=PERIOD_NS + 40,
             validation_completed_ns=PERIOD_NS + 50, feedback_started_ns=PERIOD_NS + 55,
             feedback_sent_ns=PERIOD_NS + 60, presented_ns=PERIOD_NS + 58, static_sse=0)]
    row = summarize(candidate)["sessions"][0]
    assert row["static_criterion"] and row["later_animation_presented_after_exact"]
    assert row["no_exact_to_inexact_regression"]
    candidate_session["frames"][1]["static_sse"] = 1
    assert not summarize(candidate)["sessions"][0]["no_exact_to_inexact_regression"]
    for raw in ('{"x":1,"x":2}', '{"x":NaN}', '{"x":Infinity}'):
        try:
            json.loads(raw, object_pairs_hook=unique_object, parse_constant=reject_constant)
        except ValueError:
            pass
        else:
            raise AssertionError("ambiguous or non-finite JSON accepted")
    report["scheduled_interval_ns"] = 500_000_000
    try:
        summarize(report)
    except ValueError:
        pass
    else:
        raise AssertionError("500ms workload accepted by the continuous parser")
    print("region-deadline summary self-test passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", nargs="?", type=Path)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--public-summary", action="store_true")
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
