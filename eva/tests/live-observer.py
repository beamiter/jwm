#!/usr/bin/env python3
"""Read-only, privacy-filtered evidence for a human-driven Eva voice test.

No activation, recording, cloud call, desktop command, or configuration change is
performed. The user drives Eva normally. JSONL records are observations, not a
claim that a scenario passed; hearing and desktop postconditions need separate
confirmation. PulseAudio stream lists do not establish PipeWire stream release.
"""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from contextlib import nullcontext
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import threading
import time


STAGES = set("""action_dispatched action_result action_verified activation
activation_received audio_and_cloud_ready audio_ready barge_in_progress barge_in_window cloud_ready
discovery_rejected execution_started feedback_audio_queued feedback_completed
feedback_text_fallback first_feedback_playback input_committed plan_completed
plan_normalized plan_rejected plan_reserve_started plan_reserve_completed
plan_reserve_expired plan_request_sent plan_response_created plan_response_associated
plan_response_completed playback_ended resources_released session_failed
speech_interrupted speech_started speech_stopped standby_gate_summary
tool_output_rejected unowned_input_ignored usage_cleanup_failed usage_settled
validation_completed verification_completed warm_connect_failed warm_handoff
warm_lost warm_ready warm_rotated warm_stale warm_taken
wake_failed wake_stop_requested wake_stopped wake_stop_failed wake_status_failed
wake_disabled_observed wake_heartbeat_started wake_heartbeat_completed
wake_heartbeat_rejected wake_heartbeat_retired wake_snapshot_expired wake_snapshot_failed
wake_lease_expired wake_speech_started wake_speech_stopped wake_input_committed
wake_speech_unowned wake_reserve_started wake_reserve_completed
wake_detection_requested wake_detection_created wake_detection_completed""".split())
STATES = set("""idle connecting listening planning confirming executing responding
follow_up closing error disabled paused standby waiting ready stopped suppressed
open closed tail preparing verifying succeeded partial unknown failed cancelled noop
clarify execute active inactive activating deactivating reloading refreshing
maintenance masked loaded not-found enabled static running auto-restart dead
start start-pre start-post stop stop-sigterm stop-sigkill final-sigterm
final-sigkill exited failed-permanent failed-retryable retrying exhausted""".split())
RESULTS = set("""action_schema_not_loaded application_not_registered
array_length_out_of_range audio_capture_device_unavailable audio_sequence_invalid
audio_unavailable auth_failed budget_exceeded clarify_requires_question_without_actions
configuration_invalid confirmation_expired connect_timeout constant_mismatch
context_timeout duplicate_array_item execute_requires_actions_without_question
execution_deadline_expired expected_array expected_boolean expected_exactly_one_tool_call
expected_integer expected_null expected_number expected_object expected_string
feedback_failed feedback_timeout input_audio_timestamp_overflow input_out_of_order
insufficient_quota integer_out_of_range interrupt_clear_timeout invalid_call_id
invalid_credential invalid_discovery_arguments invalid_discovery_categories
invalid_header_param invalid_input_device invalid_proxy_url invalid_realtime_url
invalid_session_intent invalid_task_intent invalid_tool_arguments
latest_plan_not_reversible local_io_timeout missing_final_tool_call missing_plan_field
missing_required_argument missing_session_intent missing_task_intent model_access_denied
model_not_available model_not_found no_schema_alternative_matches
noop_cannot_contain_actions_or_question nothing_to_undo number_out_of_range
pipewire_probe_timeout plan_invalid plan_invalid_json plan_invalid_structure plan_missing
plan_not_object plan_text_too_long plan_timeout playback_drain_timeout
playback_frame_too_large playback_queue_overflow playback_queue_stalled
proxy_host_missing pulse_capture_timeout pulse_context_failed pulse_playback_device_changed
pulse_playback_underrun pulse_source_unavailable_or_monitor quota_exceeded
rate_limit_error rate_limit_exceeded rate_limited realtime_connection_failed
realtime_ping_timeout realtime_request_rejected realtime_service_unavailable
realtime_tls_configuration_failed realtime_tls_native_roots_unavailable
realtime_write_timeout realtime_write_failed response_not_completed reused_tool_call_id
service_unavailable session_control_requires_noop session_control_task_conflict
session_locked session_timeout stale_context string_length_out_of_range
target_monitor_gone target_window_gone tls_configuration_invalid too_many_actions
undo_conflict undo_disabled undo_expired undo_must_be_only_action
undo_plan_requires_new_task unexpected_tool_output unknown_argument unsupported_action
utterance_timeout utterance_too_long value_not_in_enum version_incompatible
wake_configuration_mismatch wake_setting_changed_after_stop configured warm ready
reopened expiry_known expiry_unknown handed_over remaining_steps_cancelled
placeholder_noop_action session_closing_task_metadata in_progress dropped validated drained completed monitoring
scope_dropped interrupted turn_completed turn_failed sending renewed source_changed
local_cancel permanent_error retry_exhausted native_disabled session_exiting
cache_grace_expired native_snapshot_expired native_snapshot_invalid
native_snapshot_pending native_snapshot_superseded wake_status_unavailable wake_status_timeout late_result_discarded
proof_expired lease_confirmed observed stale_result_discarded transport_unavailable
source_rejected detected not_detected sent owned unowned reserving reserved associated""".split()) | STATES
PHASES = set("plan feedback discovery schema_repair wake desktop local_tone barge_in".split())
NUMBERS = set("""generation wake_generation session_epoch turn_id utc_ms monotonic_ns cost_microusd
source_activation_ns played_samples input_frames noise_frames above_threshold_frames
echo_frames peak_level threshold_level max_voiced_frames rejected_source_frames
source_discontinuities uploaded_frames
suppressed_frames gate_openings""".split())
IDENTIFIERS = set("response_id call_id plan_hash operation_id".split())
ID_PATTERN = re.compile(r"[A-Za-z0-9_.:/-]{1,160}\Z")
STOP = False
STOP_EVENT = threading.Event()


def action_names():
    # Action names are permitted only when present in the local static catalog;
    # arbitrary model strings and action arguments are never retained.
    names = {"switch_tag", "set_layout", "move_window_to_tag", "undo_last_plan"}
    path = Path(__file__).resolve().parents[2] / "crates/jwm_eva_core/src/actions/ipc_catalog.json"
    try:
        for entry in json.loads(path.read_text(encoding="utf-8")):
            if isinstance(entry, dict):
                names.update(entry[key] for key in ("name", "canonical_name")
                             if isinstance(entry.get(key), str))
    except (OSError, ValueError, TypeError):
        pass
    return names


ACTIONS = action_names()


def finite_number(value):
    if type(value) is int:
        return 0 <= value <= 2**64 - 1
    return type(value) is float and math.isfinite(value) and value >= 0


def code(value, vocabulary):
    return value if isinstance(value, str) and value in vocabulary else "unrecognized"


def observation_error(raw):
    """Only run()'s fixed error codes may cross the subprocess boundary."""
    result = {"available": False, "error": "invalid_json_shape"}
    if not isinstance(raw, dict):
        return result
    if raw.get("error") in ("command_timeout", "command_unavailable", "command_failed",
                            "invalid_json", "invalid_json_shape"):
        result["error"] = raw["error"]
    if type(raw.get("exit_code")) is int and -255 <= raw["exit_code"] <= 255:
        result["exit_code"] = raw["exit_code"]
    return result


def telemetry(raw):
    if not isinstance(raw, dict) or raw.get("component") != "jwm-eva":
        return None
    if not isinstance(raw.get("stage"), str) or raw["stage"] not in STAGES:
        return None
    value = {"component": "jwm-eva", "stage": raw["stage"]}
    for name in NUMBERS:
        if finite_number(raw.get(name)):
            value[name] = raw[name]
    for name in IDENTIFIERS:
        item = raw.get(name)
        if isinstance(item, str) and ID_PATTERN.fullmatch(item):
            value[name] = item
    if "result" in raw:
        value["result"] = code(raw["result"], RESULTS)
    if "phase" in raw:
        value["phase"] = code(raw["phase"], PHASES)
    if "action" in raw:
        value["action"] = code(raw["action"], ACTIONS)
    return value


def status(raw):
    if not isinstance(raw, dict):
        return observation_error(raw)
    value = {"available": raw.get("success") is True}
    if not value["available"]:
        value.update(observation_error(raw))
    if type(raw.get("pid")) is int and finite_number(raw["pid"]):
        value["pid"] = raw["pid"]
    if type(raw.get("wake_requested")) is bool:
        value["wake_requested"] = raw["wake_requested"]
    if type(raw.get("wake_generation")) is int:
        value["wake_generation"] = raw["wake_generation"]
    permit = raw.get("assistant_permit")
    if isinstance(permit, dict) and type(permit.get("generation")) is int:
        value["generation"] = permit["generation"]
    for section in ("data", "wake", "warm"):
        source = raw.get(section)
        if not isinstance(source, dict):
            continue
        projected = {}
        for name in ("state", "result_status", "retry_state", "upload_gate"):
            if name in source:
                projected[name] = code(source[name], STATES)
        if "error_code" in source:
            projected["error_code"] = code(source["error_code"], RESULTS)
        for name in ("microphone_active", "retryable", "execution_enabled", "voice_fallback"):
            if type(source.get(name)) is bool:
                projected[name] = source[name]
        for name in ("level", "age_ms", "retry_attempt", "retry_limit", "retry_in_ms",
                     "uploaded_ms", "suppressed_ms"):
            if finite_number(source.get(name)):
                projected[name] = source[name]
        progress = source.get("task_progress")
        if isinstance(progress, dict):
            projected["task_progress"] = {
                name: progress[name] for name in ("completed", "total", "current_step")
                if type(progress.get(name)) is int and progress[name] >= 0}
            projected["task_progress"]["stage"] = code(progress.get("stage"), STATES)
            if "action" in progress:
                projected["task_progress"]["action"] = code(progress["action"], ACTIONS)
        value[section] = projected
    return value


def run(argv, parse_json=True):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, errors="replace", timeout=3,
                                check=False, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
        return {"available": False, "error": "command_timeout"}
    except OSError:
        return {"available": False, "error": "command_unavailable"}
    if result.returncode:
        return {"available": False, "error": "command_failed", "exit_code": result.returncode}
    if not parse_json:
        return result.stdout
    try:
        return json.loads(result.stdout)
    except (ValueError, TypeError):
        return {"available": False, "error": "invalid_json"}


def streams(raw, pid):
    if not isinstance(raw, list):
        return observation_error(raw)
    projected = []
    for stream in raw:
        if not isinstance(stream, dict):
            continue
        props = stream.get("properties", {})
        if not isinstance(props, dict):
            continue
        app = props.get("application.name")
        process = str(props.get("application.process.id", ""))
        if app not in ("JWM Eva", "jwm-eva") and (pid is None or process != str(pid)):
            continue
        item = {"application_name": "JWM Eva"}
        if type(stream.get("index")) is int:
            item["index"] = stream["index"]
        if len(process) <= 20 and process.isascii() and process.isdigit():
            item["pid"] = int(process)
        projected.append(item)
    return {"available": True, "total_stream_count": len(raw), "eva_streams": projected}


def service(raw):
    if not isinstance(raw, str):
        return observation_error(raw)
    values = dict(line.split("=", 1) for line in raw.splitlines() if "=" in line)
    if not all(name in values for name in ("ActiveState", "SubState", "MainPID", "NRestarts")):
        return observation_error(None)
    result = {"available": True}
    for name in ("ActiveState", "SubState"):
        result[name] = code(values.get(name), STATES)
    for name in ("MainPID", "NRestarts"):
        if len(values[name]) <= 20 and values[name].isascii() and values[name].isdigit():
            result[name] = int(values[name])
    return result


def private_file(path):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    return os.fdopen(descriptor, "w", encoding="utf-8", buffering=1)


def stop(_signum, _frame):
    global STOP
    STOP = True
    STOP_EVENT.set()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True,
                        help="new JSONL file; existing files are never overwritten")
    parser.add_argument("--duration", type=float, default=600)
    parser.add_argument("--interval", type=float, default=0.5)
    parser.add_argument("--eva", default="jwm-eva")
    parser.add_argument("--summary", type=Path, help="optional new private summary JSON file")
    args = parser.parse_args()
    if not math.isfinite(args.duration) or args.duration <= 0:
        parser.error("--duration must be finite and positive")
    if not math.isfinite(args.interval) or args.interval < 0.1:
        parser.error("--interval must be finite and at least 0.1 seconds")
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    started = time.time()
    started_monotonic = time.monotonic()
    deadline = started_monotonic + args.duration
    cursor = None
    counts, errors = Counter(), Counter()
    pids, restarts = set(), set()
    samples = 0
    journal_failures = 0
    last = None
    summary_file = private_file(args.summary) if args.summary else nullcontext(None)
    with summary_file as summary_output, private_file(args.output) as output, ThreadPoolExecutor(max_workers=5) as pool:
        def emit(value):
            value = dict(value)
            value["observed_utc"] = datetime.now(timezone.utc).isoformat()
            output.write(json.dumps(value, sort_keys=True, ensure_ascii=True) + "\n")

        emit({"kind": "observer_start", "schema_version": 1,
              "poll_interval_s": args.interval, "requested_duration_s": args.duration,
              "read_only": True, "raw_audio_persisted": False,
              "pulse_stream_evidence_only": True})
        while True:
            # A final bounded read drains telemetry and observes the current
            # resource state after a signal/deadline, rather than silently
            # ending with a snapshot from the previous polling interval.
            final_poll = STOP or time.monotonic() >= deadline
            tick = time.monotonic()
            journal = ["journalctl", "--user", "-u", "jwm-eva.service", "--no-pager",
                       "-o", "json", "-n", "1000"]
            journal += ["--after-cursor", cursor] if cursor else ["--since", "@" + str(started)]
            commands = {
                "status": ([args.eva, "status", "--json"], True),
                "source_outputs": (["pactl", "-f", "json", "list", "source-outputs"], True),
                "sink_inputs": (["pactl", "-f", "json", "list", "sink-inputs"], True),
                "service": (["systemctl", "--user", "show", "jwm-eva.service",
                             "-p", "ActiveState", "-p", "SubState", "-p", "MainPID", "-p", "NRestarts"], False),
                "journal": (journal, False),
            }
            jobs = {name: pool.submit(run, argv, parse) for name, (argv, parse) in commands.items()}
            raw = {name: job.result() for name, job in jobs.items()}
            current = status(raw["status"])
            pid = current.get("pid")
            if pid:
                pids.add(pid)
            svc = service(raw["service"])
            if "NRestarts" in svc:
                restarts.add((svc.get("MainPID", 0), svc["NRestarts"]))
            snapshot = {"kind": "snapshot", "status": current, "service": svc,
                        "source_outputs": streams(raw["source_outputs"], pid),
                        "sink_inputs": streams(raw["sink_inputs"], pid)}
            samples += 1
            if snapshot != last:
                emit(snapshot)
                last = snapshot
            if isinstance(raw["journal"], str):
                lines = raw["journal"].splitlines()
                if len(lines) >= 1000:
                    journal_failures += 1
                    emit({"kind": "observer_error", "error": "journal_batch_limit"})
                for line in lines:
                    entry = None
                    try:
                        entry = json.loads(line)
                        message = entry.get("MESSAGE")
                        event = telemetry(json.loads(message)) if isinstance(message, str) else None
                    except (ValueError, TypeError, AttributeError):
                        event = None
                    if isinstance(entry, dict) and isinstance(entry.get("__CURSOR"), str):
                        cursor = entry["__CURSOR"]
                    if event is not None:
                        emit({"kind": "telemetry", "event": event})
                        counts[event["stage"]] += 1
                        if event["stage"] in ("session_failed", "plan_rejected", "tool_output_rejected",
                                               "feedback_text_fallback", "warm_connect_failed", "warm_lost"):
                            errors[event.get("result", "unrecognized")] += 1
            else:
                journal_failures += 1
                emit({"kind": "observer_error", "error": "journal_unavailable"})
            if final_poll:
                break
            remaining = min(args.interval - (time.monotonic() - tick), deadline - time.monotonic())
            if remaining > 0:
                STOP_EVENT.wait(remaining)
        summary = {"kind": "observer_summary", "samples": samples,
                   "elapsed_s": round(time.monotonic() - started_monotonic, 3), "stopped_by_signal": STOP,
                   "daemon_pids": sorted(pids), "service_restart_observations": sorted(restarts),
                   "telemetry_counts": dict(counts), "error_counts": dict(errors),
                   "journal_observation_errors": journal_failures,
                   "acceptance_result": "requires_human_scenario_verification"}
        emit(summary)
        if summary_output:
            json.dump(summary, summary_output, indent=2, sort_keys=True)
            summary_output.write("\n")
        print(json.dumps(summary, sort_keys=True))


if __name__ == "__main__":
    main()
