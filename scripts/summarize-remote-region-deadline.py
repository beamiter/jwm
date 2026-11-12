#!/usr/bin/env python3
"""Summarize bounded private 100ms region-deadline/priority TCP/GL measurements."""
import argparse
import copy
import json
import math
from pathlib import Path

BENCHMARK = "static_lossless_region_deadline_continuous_tcp_ab"
ARMS = ("new250", "region_deadline", "region_deadline", "new250")
PRIORITY_BENCHMARK = "static_lossless_region_priority_continuous_tcp_ab"
PRIORITY_ARMS = ("new250", "region_deadline", "region_priority",
                 "region_priority", "region_deadline", "new250")
PRESSURE_BENCHMARK = "static_lossless_region_priority_credit_pressure_tcp_ab"
FEEDBACK_DELAY_NS = 150_000_000
BENCHMARK_ARMS = {BENCHMARK: ARMS, PRIORITY_BENCHMARK: PRIORITY_ARMS,
                  PRESSURE_BENCHMARK: PRIORITY_ARMS}
QUEUE_COUNTERS = ("followup_opportunities", "priority_deferrals", "retained_followups")
GENERATIONS = 80
PERIOD_NS = 100_000_000
WINDOW_NS = 8_000_000_000
MAX_REPORT_BYTES = 16 * 1024 * 1024
COUNTERS = ("encoded_jpeg", "encoded_png", "encode_ns", "jpeg_ns", "png_ns",
            "sent_bytes", "comparisons", "comparison_ns", "stale_png_dropped") + QUEUE_COUNTERS


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
    result = {key: integer(value[key], key) for key in COUNTERS if key in value}
    present = [key in result for key in QUEUE_COUNTERS]
    if any(present):
        if not all(present):
            raise ValueError("partial queue counters")
        if result["followup_opportunities"] != result["priority_deferrals"] + result["retained_followups"]:
            raise ValueError("queue decision counters do not balance")
    return result


def build_mode(report, require_no_debug_assertions=False):
    # Historical reports lack this field. Never relabel them as optimized.
    value = report.get("debug_assertions")
    if "debug_assertions" in report and type(value) is not bool:
        raise ValueError("debug_assertions must be a boolean when present")
    if require_no_debug_assertions and value is not False:
        raise ValueError("explicit debug_assertions=false evidence required")
    return value


def summarize(report):
    if type(report.get("schema_version")) is not int or report["schema_version"] != 1:
        raise ValueError("unsupported schema version")
    benchmark = report.get("benchmark")
    if benchmark not in BENCHMARK_ARMS:
        raise ValueError("wrong benchmark")
    arms = BENCHMARK_ARMS[benchmark]
    pressure = benchmark == PRESSURE_BENCHMARK
    delay = integer(report.get("presentation_feedback_delay_ns", 0), "presentation_feedback_delay_ns")
    if delay != (FEEDBACK_DELAY_NS if pressure else 0):
        raise ValueError("unexpected fixed presentation feedback delay")
    for key, expected in (("scheduled_interval_ns", PERIOD_NS), ("active_window_ns", WINDOW_NS),
                          ("generation_budget", GENERATIONS), ("drain_ns", 1_000_000_000),
                          ("width", 800), ("height", 600), ("dynamic_region_width", 32),
                          ("dynamic_region_height", 16), ("static_pixels", 479488),
                          ("maximum_png_batch_pixels", 65536)):
        if integer(report.get(key), key) != expected:
            raise ValueError(f"unexpected fixed workload: {key}")
    sessions = report.get("sessions")
    if not isinstance(sessions, list) or not 1 <= len(sessions) <= len(arms):
        raise ValueError("bounded sessions required, including failures")
    hashes, orders, results = {}, set(), []
    for session in sessions:
        order = integer(session.get("order"), "order")
        if order >= len(arms) or order in orders or session.get("arm") != arms[order]:
            raise ValueError("invalid mirrored session order or arm")
        orders.add(order)
        if type(session.get("success")) is not bool:
            raise ValueError("success must be boolean")
        total_counters = counters(session.get("host_stats"))
        active_counters = counters(session.get("active_window_host_stats"))
        for stats in (total_counters, active_counters):
            if pressure and session["success"] and (stats is None or any(key not in stats for key in QUEUE_COUNTERS)):
                raise ValueError("successful pressure session requires queue counters")
            if stats is not None and session["arm"] != "region_priority" and stats.get("priority_deferrals", 0):
                raise ValueError("non-priority arm reported priority deferrals")
        if total_counters is not None and active_counters is not None:
            for key in QUEUE_COUNTERS:
                if key in total_counters and key in active_counters and active_counters[key] > total_counters[key]:
                    raise ValueError("active queue counter exceeds session total")
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
            if pressure:
                observed = integer(frame.get("feedback_observed_ns"), "feedback_observed_ns")
                if times[0] < observed + delay:
                    raise ValueError("pressure frame lacks the fixed feedback hold")
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
                        "aggregate_host_counters": total_counters,
                        "active_window_host_counters": active_counters,
                        "samples": samples})
    return {"schema_version": 1, "benchmark": benchmark, "debug_assertions": build_mode(report),
            "presentation_feedback_delay_ns": delay, "planned_sessions": len(arms),
            "planned_generation_budget": len(arms) * GENERATIONS, "reported_sessions": len(results),
            "unreported_session_generation_budget": (len(arms) - len(results)) * GENERATIONS,
            "scope": "100ms deadlines; active window <=8s, later presentations are cleanup only; quantiles include observed samples only and missing counts retain all 80 planned generations; static equality excludes exactly 32x16 animation pixels; source commit means issued; host payload includes source envelope, excludes Noise/TCP; encode counts include discarded attempts; host counters include common startup and active counters are last sampled <=8s",
            "dynamic_non_regression": dynamic_assessment(results, arms),
            "pressure_exercised": pressure_assessment(results) if pressure else None,
            "sessions": results}



def pressure_assessment(sessions):
    """Actual selected-latest decisions, not inferred from injected feedback delay."""
    candidates = sorted((s for s in sessions if s["arm"] == "region_priority"), key=lambda s: s["order"])
    result = {"status": "no_claim", "reason": "incomplete_or_failed_sessions",
              "scope": "Both candidate sessions must select a queued latest capture instead of a retained followup within the active window; this does not prove encode/send/presentation or dynamic non-regression.",
              "candidate_observations": [{"order": s["order"], "active_priority_deferrals":
                  (s["active_window_host_counters"] or {}).get("priority_deferrals")} for s in candidates]}
    if len(sessions) != len(PRIORITY_ARMS) or len(candidates) != 2 or any(not s["success"] for s in sessions):
        return result
    counts = [row["active_priority_deferrals"] for row in result["candidate_observations"]]
    if any(count is None for count in counts):
        result["reason"] = "missing_active_counters"
    elif all(count > 0 for count in counts):
        result.update(status="observed_priority_deferrals", reason="both_candidates_selected_queued_latest")
    else:
        result.update(status="not_established", reason="at_least_one_candidate_without_active_deferral")
    return result


def public_pressure_assessment(assessment):
    if assessment is None:
        return None
    return {**{key: assessment[key] for key in ("status", "reason", "scope")},
            "candidate_observations": [{key: row[key] for key in ("order", "active_priority_deferrals")}
                                       for row in assessment["candidate_observations"]]}


def dynamic_metrics(session):
    return {**{f"{clock}_{quantile}": session[f"within_window_{clock}_to_present"][quantile]
               for clock in ("scheduled", "commit") for quantile in ("p95_ms", "max_ms")},
            "missing_planned_generations": session["within_window_missing_planned_generations"],
            "late_presented_generations": session["late_presented_generations"]}


def dynamic_assessment(sessions, arms):
    """No pooled quantiles: every candidate must beat the best control per metric."""
    candidate_arm = "region_priority" if "region_priority" in arms else "region_deadline"
    candidates = sorted((s for s in sessions if s["arm"] == candidate_arm), key=lambda s: s["order"])
    controls = [s for s in sessions if s["arm"] != candidate_arm]
    result = {"candidate_arm": candidate_arm,
              "control_arms": sorted(set(arms) - {candidate_arm}),
              "scope": "Both candidate sessions versus the per-metric best of all control sessions; active-window observed scheduled/commit p95 and max, active-window planned missing, and all late presentations including cleanup. Descriptive only, not statistical or hard real-time proof.",
              "status": "no_claim", "reason": None,
              "best_control": None, "candidate_comparisons": []}
    if len(sessions) != len(arms) or any(not s["success"] for s in sessions):
        result["reason"] = "incomplete_or_failed_sessions"
        return result
    if any(not s["static_criterion"] or not s["no_exact_to_inexact_regression"] for s in sessions):
        result["reason"] = "static_gate_failed"
        return result
    metrics = [dynamic_metrics(s) for s in sessions]
    if any(value is None for metric in metrics for value in metric.values()):
        result["reason"] = "missing_dynamic_observations"
        return result
    best = {key: min(dynamic_metrics(s)[key] for s in controls) for key in metrics[0]}
    result["best_control"] = best
    for session in candidates:
        values = dynamic_metrics(session)
        checks = {key: value <= best[key] for key, value in values.items()}
        result["candidate_comparisons"].append({"order": session["order"], "metrics": values,
                                                  "no_worse_than_best_control": checks,
                                                  "passed": all(checks.values())})
    passed = len(candidates) == 2 and all(c["passed"] for c in result["candidate_comparisons"])
    result["status"] = "observed_non_regression" if passed else "non_regression_not_established"
    result["reason"] = "all_candidate_metrics_no_worse" if passed else "candidate_metric_worse_than_best_control"
    return result


def public_dynamic_assessment(assessment):
    # Nested allowlists prevent later private diagnostic fields from leaking.
    result = {key: assessment[key] for key in
              ("candidate_arm", "control_arms", "scope", "status", "reason")}
    metric_keys = ("scheduled_p95_ms", "scheduled_max_ms", "commit_p95_ms", "commit_max_ms",
                   "missing_planned_generations", "late_presented_generations")
    result["best_control"] = (None if assessment["best_control"] is None else
                              {key: assessment["best_control"][key] for key in metric_keys})
    result["candidate_comparisons"] = [
        {"order": row["order"], "passed": row["passed"],
         "metrics": {key: row["metrics"][key] for key in metric_keys},
         "no_worse_than_best_control": {key: row["no_worse_than_best_control"][key] for key in metric_keys}}
        for row in assessment["candidate_comparisons"]]
    return result


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
    result = {key: summary[key] for key in ("schema_version", "benchmark", "debug_assertions", "presentation_feedback_delay_ns", "planned_sessions",
              "planned_generation_budget", "reported_sessions", "unreported_session_generation_budget", "scope")}
    result["dynamic_non_regression"] = public_dynamic_assessment(summary["dynamic_non_regression"])
    result["pressure_exercised"] = public_pressure_assessment(summary["pressure_exercised"])
    result["sessions"] = [{key: session[key] for key in allowed} for session in summary["sessions"]]
    for session in result["sessions"]:
        for key in ("aggregate_host_counters", "active_window_host_counters"):
            session[key] = counters(session[key])
    return result


def self_test():
    for value in (True, False):
        assert build_mode({"debug_assertions": value}) is value
    assert build_mode({}) is None
    for value in (None, 0, 1, "false", [], {}):
        try:
            build_mode({"debug_assertions": value})
        except ValueError:
            pass
        else:
            raise AssertionError("invalid build evidence accepted")
    for report_mode in ({}, {"debug_assertions": True}):
        try:
            build_mode(report_mode, require_no_debug_assertions=True)
        except ValueError:
            pass
        else:
            raise AssertionError("unknown/debug report accepted by optimized gate")
    assert build_mode({"debug_assertions": False}, require_no_debug_assertions=True) is False
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
    assert public_summary(summarize(report))["debug_assertions"] is None
    report["debug_assertions"] = False
    assert public_summary(summarize(report))["debug_assertions"] is False
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
    priority = copy.deepcopy(report)
    priority["benchmark"] = PRIORITY_BENCHMARK
    priority["sessions"] = [dict(copy.deepcopy(session), arm=arm, order=order)
                            for order, arm in enumerate(PRIORITY_ARMS)]
    priority_summary = summarize(priority)
    assert priority_summary["planned_sessions"] == 6
    assert priority_summary["planned_generation_budget"] == 480
    assert priority_summary["unreported_session_generation_budget"] == 0
    assert priority_summary["dynamic_non_regression"]["status"] == "no_claim"
    assert [row["arm"] for row in priority_summary["sessions"]] == list(PRIORITY_ARMS)
    for benchmark, arms in BENCHMARK_ARMS.items():
        valid = copy.deepcopy(report)
        valid["benchmark"] = benchmark
        if benchmark == PRESSURE_BENCHMARK:
            valid["presentation_feedback_delay_ns"] = FEEDBACK_DELAY_NS
        valid["sessions"] = [dict(copy.deepcopy(session), arm=arm, order=order)
                             for order, arm in enumerate(arms)]
        if benchmark == PRESSURE_BENCHMARK:
            for pressure_session in valid["sessions"]:
                for pressure_frame in pressure_session["frames"]:
                    pressure_frame["feedback_observed_ns"] = 0
                    for key in ("gl_started_ns", "gl_completed_ns", "validation_completed_ns",
                                "feedback_started_ns", "feedback_sent_ns"):
                        pressure_frame[key] += FEEDBACK_DELAY_NS
        assert summarize(valid)["planned_sessions"] == len(arms)
        bad_reports = []
        wrong_arm = copy.deepcopy(valid)
        wrong_arm["sessions"][0]["arm"] = "region_priority"
        bad_reports.append(wrong_arm)
        wrong_order = copy.deepcopy(valid)
        wrong_order["sessions"][0]["order"] = len(arms)
        bad_reports.append(wrong_order)
        duplicate = copy.deepcopy(valid)
        duplicate["sessions"][1] = copy.deepcopy(duplicate["sessions"][0])
        bad_reports.append(duplicate)
        extra = copy.deepcopy(valid)
        extra["sessions"].append(copy.deepcopy(extra["sessions"][0]))
        bad_reports.append(extra)
        for bad_report in bad_reports:
            try:
                summarize(bad_report)
            except ValueError:
                pass
            else:
                raise AssertionError("wrong arm/order schema accepted")
    pressure = copy.deepcopy(priority)
    pressure["benchmark"] = PRESSURE_BENCHMARK
    pressure["presentation_feedback_delay_ns"] = FEEDBACK_DELAY_NS
    for row in pressure["sessions"]:
        row["host_stats"] = {"followup_opportunities": 7, "priority_deferrals": 2 if row["arm"] == "region_priority" else 0,
                             "retained_followups": 5 if row["arm"] == "region_priority" else 7,
                             "private_trace": "do not publish"}
        row["active_window_host_stats"] = {"followup_opportunities": 3, "priority_deferrals": 1 if row["arm"] == "region_priority" else 0,
                                           "retained_followups": 2 if row["arm"] == "region_priority" else 3}
        for observed_frame in row["frames"]:
            observed_frame["feedback_observed_ns"] = 0
            for key in ("gl_started_ns", "gl_completed_ns", "validation_completed_ns",
                        "feedback_started_ns", "feedback_sent_ns"):
                observed_frame[key] += FEEDBACK_DELAY_NS
    pressure_summary = summarize(pressure)
    public = public_summary(pressure_summary)
    assert public["presentation_feedback_delay_ns"] == FEEDBACK_DELAY_NS
    assert public["sessions"][2]["aggregate_host_counters"]["priority_deferrals"] == 2
    assert public["dynamic_non_regression"]["status"] == "no_claim"
    assert "feedback_observed_ns" not in json.dumps(public)
    assert "do not publish" not in json.dumps(public)
    assert "followup_opportunities" not in (summarize(report)["sessions"][0]["aggregate_host_counters"] or {})
    assert public["pressure_exercised"]["status"] == "no_claim"
    assert public_summary(summarize(report))["pressure_exercised"] is None
    pressure_rows = copy.deepcopy(pressure_summary["sessions"])
    for row in pressure_rows:
        row["success"] = True
    exercised = pressure_assessment(pressure_rows)
    assert exercised["status"] == "observed_priority_deferrals"
    assert pressure_assessment(pressure_rows[:-1])["status"] == "no_claim"
    for order in (2, 3):
        changed = copy.deepcopy(pressure_rows)
        changed[order]["active_window_host_counters"]["priority_deferrals"] = 0
        assert pressure_assessment(changed)["status"] == "not_established"
        changed[order]["active_window_host_counters"] = None
        assert pressure_assessment(changed)["reason"] == "missing_active_counters"
        changed[order]["success"] = False
        assert pressure_assessment(changed)["status"] == "no_claim"
    exercised["private_trace"] = "do not publish"
    exercised["candidate_observations"][0]["private_trace"] = "do not publish"
    assert "do not publish" not in json.dumps(public_pressure_assessment(exercised))
    bad_pressure_reports = []
    for delay in (None, True, -1, 0, FEEDBACK_DELAY_NS - 1, FEEDBACK_DELAY_NS + 1):
        bad = copy.deepcopy(pressure)
        bad["presentation_feedback_delay_ns"] = delay
        bad_pressure_reports.append(bad)
    bad = copy.deepcopy(pressure)
    del bad["presentation_feedback_delay_ns"]
    bad_pressure_reports.append(bad)
    for observed in (None, True, -1, 31):  # first GL readback is delay + 30 ns
        bad = copy.deepcopy(pressure)
        bad["sessions"][0]["frames"][0]["feedback_observed_ns"] = observed
        bad_pressure_reports.append(bad)
    bad = copy.deepcopy(pressure)
    del bad["sessions"][0]["frames"][0]["feedback_observed_ns"]
    bad_pressure_reports.append(bad)
    bad = copy.deepcopy(priority)
    bad["presentation_feedback_delay_ns"] = FEEDBACK_DELAY_NS
    bad_pressure_reports.append(bad)
    for key in QUEUE_COUNTERS:
        for value in (None, True, -1, 2**63):
            bad = copy.deepcopy(pressure)
            bad["sessions"][2]["host_stats"][key] = value
            bad_pressure_reports.append(bad)
        bad = copy.deepcopy(pressure)
        del bad["sessions"][2]["host_stats"][key]
        bad_pressure_reports.append(bad)
    bad = copy.deepcopy(pressure)
    bad["sessions"][2]["host_stats"]["followup_opportunities"] += 1
    bad_pressure_reports.append(bad)
    bad = copy.deepcopy(pressure)
    bad["sessions"][0]["host_stats"].update(priority_deferrals=1, retained_followups=6)
    bad_pressure_reports.append(bad)
    bad = copy.deepcopy(pressure)
    bad["sessions"][2]["active_window_host_stats"].update(followup_opportunities=8, retained_followups=7)
    bad_pressure_reports.append(bad)
    for field in ("host_stats", "active_window_host_stats"):
        bad = copy.deepcopy(pressure)
        bad["sessions"][0]["success"] = True
        bad["sessions"][0][field] = None
        bad_pressure_reports.append(bad)
    for bad in bad_pressure_reports:
        try:
            summarize(bad)
        except ValueError:
            pass
        else:
            raise AssertionError("invalid pressure evidence accepted")
    pressure_summary["sessions"][0]["aggregate_host_counters"]["private_trace"] = "do not publish"
    assert "do not publish" not in json.dumps(public_summary(pressure_summary))

    wrong_benchmark = copy.deepcopy(priority)
    wrong_benchmark["benchmark"] = BENCHMARK
    try:
        summarize(wrong_benchmark)
    except ValueError:
        pass
    else:
        raise AssertionError("priority sessions accepted as deadline benchmark")

    # Exercise the assessment independently with explicit aggregate fixtures.
    rows = copy.deepcopy(priority_summary["sessions"])
    for row in rows:
        row.update(success=True, static_criterion=True, no_exact_to_inexact_regression=True,
                   within_window_missing_planned_generations=5, late_presented_generations=20)
        for clock in ("scheduled", "commit"):
            row[f"within_window_{clock}_to_present"].update(p95_ms=90, max_ms=100)
    assessment = dynamic_assessment(rows, PRIORITY_ARMS)
    assert assessment["status"] == "observed_non_regression"
    assert len(assessment["candidate_comparisons"]) == 2
    # Every metric in either candidate must pass, even when its peer passes.
    for order in (2, 3):
        for clock in ("scheduled", "commit"):
            for quantile in ("p95_ms", "max_ms"):
                changed = copy.deepcopy(rows)
                changed[order][f"within_window_{clock}_to_present"][quantile] += 1
                assert dynamic_assessment(changed, PRIORITY_ARMS)["status"] == "non_regression_not_established"
        for key in ("within_window_missing_planned_generations", "late_presented_generations"):
            changed = copy.deepcopy(rows)
            changed[order][key] += 1
            assert dynamic_assessment(changed, PRIORITY_ARMS)["status"] == "non_regression_not_established"
    # A faster second control defeats a comparison against only the first control.
    for order in (1, 4, 5):
        changed = copy.deepcopy(rows)
        changed[order]["within_window_scheduled_to_present"]["p95_ms"] = 89
        assert dynamic_assessment(changed, PRIORITY_ARMS)["status"] == "non_regression_not_established"
    assert dynamic_assessment(rows[:-1], PRIORITY_ARMS)["status"] == "no_claim"
    for key in ("success", "static_criterion", "no_exact_to_inexact_regression"):
        changed = copy.deepcopy(rows)
        changed[3][key] = False
        assert dynamic_assessment(changed, PRIORITY_ARMS)["status"] == "no_claim"
    changed = copy.deepcopy(rows)
    changed[2]["within_window_scheduled_to_present"]["p95_ms"] = None
    assert dynamic_assessment(changed, PRIORITY_ARMS)["reason"] == "missing_dynamic_observations"
    assessment["private_trace"] = "do not publish"
    assessment["best_control"]["private_trace"] = "do not publish"
    assessment["candidate_comparisons"][0]["metrics"]["private_trace"] = "do not publish"
    assessment["candidate_comparisons"][0]["private_trace"] = "do not publish"
    priority_summary["dynamic_non_regression"] = assessment
    assert "do not publish" not in json.dumps(public_summary(priority_summary))
    report["scheduled_interval_ns"] = 500_000_000
    try:
        summarize(report)
    except ValueError:
        pass
    else:
        raise AssertionError("500ms workload accepted by the continuous parser")
    print("region-deadline/priority summary self-test passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", nargs="?", type=Path)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--public-summary", action="store_true")
    parser.add_argument("--require-no-debug-assertions", action="store_true",
                        help="reject unknown/debug builds; intended for the fixed profiling CI command")
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
        build_mode(report, args.require_no_debug_assertions)
        summary = summarize(report)
        print(json.dumps(public_summary(summary) if args.public_summary else summary, indent=2, allow_nan=False))
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
