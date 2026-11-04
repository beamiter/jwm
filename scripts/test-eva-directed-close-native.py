#!/usr/bin/env python3
"""Owned X11 directed-close regression; no invoking desktop/provider/mic.

Supply a frozen JWM Release and the opt-in Eva unit-test ELF containing
native_close_probe::owned_native_directed_close. Every X server, client, IPC
socket, local key approval and synthetic-provider ledger belongs to this run.
The fixture sends real WM_DELETE_WINDOW, never substitutes a mock backend.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("owned_close_desktop", ROOT / "eva/tests/desktop-ipc.py")
F = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(F)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def wait(predicate, label, timeout=20):
    until, error = time.monotonic() + timeout, None
    while time.monotonic() < until:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as caught:
            error = str(caught)
        time.sleep(.03)
    raise AssertionError(f"{label}: {error}")


def identity(pid):
    try:
        return {"pid": pid, "start_ticks": Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]}
    except (OSError, IndexError):
        return None


def events(path):
    result = []
    for line in path.read_text().splitlines() if path.exists() else []:
        try:
            result.append(json.loads(line))
        except json.JSONDecodeError:
            pass  # A concurrently written final line is retried by the next poll.
    return result


def private_env(work, library):
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TZ") if key in os.environ}
    for key, leaf in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                      ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime")):
        path = work / leaf
        path.mkdir(mode=0o700)
        env[key] = str(path)
    env.update(LIBGL_ALWAYS_SOFTWARE="1", JWM_COMPOSITOR_API="glx", WINIT_UNIX_BACKEND="x11",
               GTK_USE_PORTAL="0", NO_AT_BRIDGE="1", RUST_LOG="warn",
               JWM_SOCKET=str(work / "runtime/jwm-ipc.sock"),
               DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(work / "no-system-bus"))
    if library:
        env["LD_LIBRARY_PATH"] = str(library)
    return env


def run_case(args, backend, client):
    output = args.output / backend
    output.mkdir(mode=0o700)
    work = Path(tempfile.mkdtemp(prefix="jwm-native-close-"))
    work.chmod(0o700)
    for leaf in ("cases", "results"):
        (work / leaf).mkdir(mode=0o700)
    children = F.Children()
    report = {"status": "failed", "backend": backend, "private_directory": str(work),
              "scope": "owned_x11_native_with_synthetic_provider_and_isolated_local_ledger",
              "uses_user_desktop": False, "provider_calls": 0, "microphone": False,
              "physical_input_certified": False, "steps": [],
              "elf_sha256": {"jwm": sha(args.jwm), "eva_probe": sha(args.probe), "native_client": sha(client)},
              "harness_source_sha256":sha(Path(__file__).resolve()),"native_client_source_sha256":sha(ROOT/"scripts/fixtures/eva-owned-close-client.c")}
    stopping = threading.Event()
    heartbeat = None
    try:
        env = private_env(work, args.runtime_lib)
        (work / "config/jwm").mkdir(mode=0o700)
        subprocess.run([str(args.jwm), "--backend", backend, "--gen-config"], env=env, check=True,
                       capture_output=True, timeout=30)
        for config in (work / "config/jwm").glob("config_*.toml"):
            text = F.edit_setting(config.read_text(), "status_bar", "show_bar", "false")
            text = F.edit_setting(text, "behavior", "compositor", "true")
            config.write_text(F.edit_setting(text, "desktop_shell", "enabled", "false", required=False))
        env["DISPLAY"] = children.display("Xvfb", env, output)
        env["DISPLAY"] = children.display("Xephyr", env, output)
        wm = children.spawn(["dbus-run-session", "--", str(args.jwm), "--backend", backend], env, output / "jwm.log")
        path = Path(env["JWM_SOCKET"])
        context = wait(lambda: F.query(path, "get_execution_context"), "private WM context", 40)
        assert wm.poll() is None
        assert context["windows_complete"] is True
        assert "directed_close_window_v1" in context["assistant_protocol"]["features"]
        (work / "owned.json").write_text(json.dumps({"owner_pid": os.getpid(), "display": env["DISPLAY"],
                                                    "synthetic_provider_scope": True, "wm_identity": identity(int(context["wm_pid"])),"wm_session_wrapper_identity":identity(wm.pid)}))
        env["JWM_NATIVE_CLOSE_FIXTURE"] = str(work)

        def command(name, data=None, expect=True):
            assert path == work / "runtime/jwm-ipc.sock"
            return F.command(path, name, data or {}, expect)

        def query(name):
            return F.query(path, name)

        def xdo(*argv):
            return subprocess.check_output(["xdotool", *map(str, argv)], env=env, text=True, timeout=5).strip()

        apps = {}
        def spawn(name, tag=1):
            command("view", {"value": 1 << (tag - 1)})
            command("setlayout", {"layout": "float"})
            fifo = work / f"control-{name}"
            os.mkfifo(fifo, 0o600)
            log = output / f"client-{name}.jsonl"
            client_env = env | ({"JWM_NATIVE_CLIENT_HOLD_CLOSE":"1"} if name=="B" else {})
            proc = children.spawn([str(client), name, str(fifo)], client_env, log)
            created = wait(lambda: next((event for event in events(log) if event["event"] == "created"), None), f"native {name}")
            app = {"process": proc, "identity": identity(proc.pid), "xid": created["xid"], "fifo": fifo, "log": log}
            apps[name] = app
            wait(lambda: row(name), f"managed {name}")
            return app

        def row(name):
            return next((w for w in query("get_execution_context")["windows"]
                         if w.get("native_window_id") == apps[name]["xid"]
                         and w.get("pid") == apps[name]["process"].pid), None)

        spawn("B", 2)
        spawn("A")
        spawn("C")
        spawn("R")
        command("view", {"value": 1})
        xdo("windowactivate", "--sync", apps["A"]["xid"])
        wait(lambda: int(xdo("getwindowfocus")) == apps["A"]["xid"], "focus A")
        activation = wait(lambda: (lambda response: response.get("data") if response.get("success") else None)(
            command("assistant_control", {"action": "activate"}, False)), "real private assistant permit")
        permit = activation["assistant_permit"]
        def renew():
            while not stopping.wait(.4):
                try:
                    command("set_assistant_status", {"assistant_permit": permit, "state": "confirming",
                                                       "text": "Owned native close fixture; no provider/audio", "microphone_active": False})
                except (OSError, RuntimeError):
                    return
        heartbeat = threading.Thread(target=renew, daemon=True)
        heartbeat.start()

        def rotate_private_permit():
            # Independent fixture cases must not accidentally share a valid,
            # unconsumed approval for an identical frozen target list. Reset
            # through the real private session-control route, never by changing
            # protected confirmation state or manufacturing a token.
            nonlocal permit, heartbeat
            stopping.set()
            heartbeat.join(timeout=4)
            assert not heartbeat.is_alive(),"old private heartbeat did not stop"
            command("assistant_control",{"action":"cancel"})
            fresh=wait(lambda:(lambda response:response.get("data") if response.get("success") else None)(
                command("assistant_control",{"action":"activate"},False)),"fresh private permit for next case")
            permit=fresh["assistant_permit"]
            stopping.clear()
            heartbeat=threading.Thread(target=renew,daemon=True)
            heartbeat.start()
            report.setdefault("private_case_generations",[]).append(permit["generation"])

        def probe(case, mode, captured, refs, frozen=None, confirmation=None, on_running=None):
            semantic = {"disposition": "execute", "actions": [{"name": "killclient", "arguments": {"window_ref": ref}} for ref in refs], "question": None, "reason": None}
            payload = {"mode": mode, "captured_context": captured, "semantic_plan": semantic,
                       "frozen_plan": frozen, "permit": permit, "confirmation_token": confirmation,"settle_accepted":on_running is not None}
            input_path = work / "cases" / f"{case}-{mode}.json"
            result_path = work / "results" / f"{case}-{mode}.json"
            input_path.write_text(json.dumps(payload))
            input_path.chmod(0o600)
            child_env = env | {"JWM_NATIVE_CLOSE_INPUT": str(input_path), "JWM_NATIVE_CLOSE_OUTPUT": str(result_path)}
            child=children.spawn([str(args.probe),"--exact","native_close_probe::owned_native_directed_close","--ignored","--nocapture"],child_env,output/f"probe-{case}-{mode}.log")
            if on_running:
                on_running(child,case,captured,frozen,confirmation)
            assert child.wait(timeout=40)==0, f"native probe failed: {case}-{mode}; inspect its log"
            result = json.loads(result_path.read_text())
            shutil.copyfile(result_path, output / f"probe-{case}-{mode}.json")
            return result

        def approve(frozen):
            command("set_assistant_status", {"assistant_permit": permit, "state": "confirming", "microphone_active": False,
                    "text": "Owned native close fixture; no provider/audio", "confirmation": {"plan_hash": frozen["plan_hash"], "targets": frozen["actions"], "timeout_ms": 60000}})
            status = wait(lambda: query("get_assistant_status").get("confirmation"), "trusted local confirmation card")
            assert status["approved"] is False and status["token"] is None
            assert status["total_actions"] == len(frozen["actions"])
            # Actual local trusted-key route; no IPC/model-generated token.
            xdo("key", "--clearmodifiers", "ctrl+alt+d")
            time.sleep(.1)
            for _ in range(32):
                before = query("get_assistant_status")["confirmation"]["details_page"]
                xdo("key", "--clearmodifiers", "Next")
                time.sleep(.03)
                after = query("get_assistant_status")["confirmation"]["details_page"]
                if before == after:
                    break
            xdo("key", "--clearmodifiers", "ctrl+alt+Return")
            status = wait(lambda: (lambda s: s if s and s["approved"] else None)(query("get_assistant_status").get("confirmation")), "local key approval")
            assert status["plan_hash"] == frozen["plan_hash"]
            return status["token"]

        def protected_snapshot():
            result = {"native_focus": int(xdo("getwindowfocus")), "windows": {}}
            for name in ("A", "C"):
                app = apps[name]
                assert identity(app["process"].pid) == app["identity"]
                geometry = xdo("getwindowgeometry", "--shell", app["xid"])
                state = row(name)
                assert state
                result["windows"][name] = {"identity": app["identity"], "xid": app["xid"], "geometry": geometry,
                                          "token": state["target_token"], "internal_window_id":state["id"], "tags": state["tag_mask"]}
            return result

        def cursors():
            time.sleep(.1)
            return {name: len(events(app["log"])) for name, app in apps.items()}

        def unchanged(before, cursor, permitted_deletes=()):
            time.sleep(.15)
            after=protected_snapshot()
            evidence={"protected_before":before,"protected_after":after}
            report["last_native_comparison"]=evidence
            assert after == before, "A/C native identity, geometry, tags or focus changed"
            assert before["native_focus"] == apps["A"]["xid"]
            fresh = {}
            for name, app in apps.items():
                extra = events(app["log"])[cursor.get(name, 0):]
                deletes = [e for e in extra if e["event"] == "wm_delete_window"]
                focus = [e for e in extra if e["event"] in ("focus_in", "focus_out") and e.get("mode") == 0]
                assert not focus, f"temporary native refocus during guarded dispatch: {name}: {focus}"
                assert len(deletes) == (1 if name in permitted_deletes else 0), (name, deletes)
                fresh[name] = extra
            evidence["native_events"]=fresh
            return evidence

        def freeze_case(name, target_names, approved=True, mutate=None, incomplete=False, on_running=None):
            rotate_private_permit()
            captured = query("get_execution_context")
            refs = [row(target)["window_ref"] for target in target_names]
            frozen = probe(name, "freeze", captured, refs)["frozen_plan"]
            token = approve(frozen) if approved else None
            if mutate:
                mutate()
            (output/f"context-{name}-original.json").write_text(json.dumps(captured,indent=2)+"\n")
            if incomplete:
                captured["windows_complete"] = False  # synthetic-provider uncertainty, never forge true.
            xdo("windowactivate", "--sync", apps["A"]["xid"])
            wait(lambda: int(xdo("getwindowfocus")) == apps["A"]["xid"], "A stable before guard")
            cursor = cursors()
            before = protected_snapshot()
            (output/f"context-{name}-synthetic-provider-capture.json").write_text(json.dumps(captured,indent=2)+"\n")
            (output/f"context-{name}-actual-before-dispatch.json").write_text(json.dumps(query("get_execution_context"),indent=2)+"\n")
            result = probe(name, "execute", captured, refs, frozen, token, on_running=on_running)
            return result, before, cursor

        def zero_prefix(value,code):
            result=value["result"]
            assert result["status"]=="failed" and result["error"]==code,result
            assert result["steps"]==[] and result["dispatched"] is False and result["completed_steps"]==0,result
            assert value["requests_for_historical_replay"]==[]
            assert value["synthetic_local_ledger"]["operations"]=={}

        # No local approval: no WM_DELETE_WINDOW may reach any owned app.
        denied, before, cursor = freeze_case("unconfirmed", ["B"], approved=False)
        assert denied["result"]["status"] in ("failed","cancelled","clarify"),denied
        assert "confirm" in json.dumps(denied["result"]).lower(),denied
        report["steps"].append({"name": "unconfirmed_rejected", "native_evidence": unchanged(before, cursor), "result": denied["result"]})

        # Capture full [C,R], genuinely approve it, then declare the capture incomplete.
        incomplete, before, cursor = freeze_case("incomplete", ["C", "R"], incomplete=True)
        zero_prefix(incomplete,"unsupported_action")
        assert "complete close snapshot" in incomplete["result"]["detail"]
        report["steps"].append({"name": "incomplete_capture_zero_close_before_first_member", "native_evidence": unchanged(before, cursor), "result": incomplete["result"]})

        def change_tag_without_focus():
            current=query("get_execution_context")
            request=F.sealed(current,permit,"fixture-setup-stale-tag",{"name":"move_window_to_tag","arguments":{"window_token":row("R")["target_token"],"tag_mask":4}})
            response=command("guarded_execute",request)
            assert response["data"]["status"]=="succeeded",response
            wait(lambda: row("R")["tag_mask"]==4,"owned R tag drift")
            report.setdefault("setup_mutations",[]).append({"kind":"fixture_guarded_tag_move_before_close_dispatch","window":row("R"),"receipt":response["data"]})

        stale,before,cursor=freeze_case("stale-tag",["C","R"],mutate=change_tag_without_focus)
        zero_prefix(stale,"stale_context")
        report["steps"].append({"name":"live_target_tag_drift_zero_prefix_close","native_events":unchanged(before,cursor),"result":stale["result"]})

        def recycle():
            old = row("R")
            with apps["R"]["fifo"].open("w") as control:
                control.write("recycle\n")
            wait(lambda: (lambda new: new if new and new["target_token"] != old["target_token"] else None)(row("R")), "same native XID with fresh stable instance")
            assert row("R")["native_window_id"] == old["native_window_id"] == apps["R"]["xid"]
            assert identity(apps["R"]["process"].pid) == apps["R"]["identity"]
            report.setdefault("setup_mutations",[]).append({"kind":"fixture_recreate_same_native_xid","process":apps["R"]["identity"],"old_instance":old,"new_instance":row("R")})

        recycled, before, cursor = freeze_case("recycled", ["C", "R"], mutate=recycle)
        zero_prefix(recycled,"target_window_gone")
        report["steps"].append({"name": "recycled_native_xid_zero_prefix_close", "native_evidence": unchanged(before, cursor), "result": recycled["result"]})

        def destroy():
            with apps["R"]["fifo"].open("w") as control:
                control.write("destroy\n")
            wait(lambda: row("R") is None, "owned R native resource destroyed")

        dead, before, cursor = freeze_case("dead", ["C", "R"], mutate=destroy)
        zero_prefix(dead,"target_window_gone")
        report["steps"].append({"name": "dead_second_member_zero_prefix_close", "native_evidence": unchanged(before, cursor), "result": dead["result"]})

        pending_replay={}
        def replay_while_native_window_is_alive(child,case,captured,frozen,token):
            wait(lambda:any(event["event"]=="wm_delete_window" for event in events(apps["B"]["log"])),"real WM_DELETE_WINDOW observed by live B",2)
            assert child.poll() is None,"executor must not finish before native close completion"
            assert apps["B"]["process"].poll() is None and row("B") is not None
            ledger_path=work/"state"/f"synthetic-native-{case}-execute"/"operations.json"
            operation=wait(lambda:next(iter(json.loads(ledger_path.read_text())["operations"].values()),None),"actual owned execution intent",1)
            request={"protocol_version":1,"wm_instance_id":captured["wm_instance_id"],"assistant_permit":permit,"operation_id":operation["operation_id"],"confirmation_token":token,"expected_context":{"session_unlocked":True},"action":operation["action"]}
            request["request_digest"]=hashlib.sha256(json.dumps(request,sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()).hexdigest()
            current=F.ipc(path,{"query":"get_execution_status","args":{"wm_instance_id":captured["wm_instance_id"],"operation_id":operation["operation_id"]}})["data"]
            assert current["status"]=="accepted",current
            replay=command("guarded_execute",request)["data"]
            assert replay["status"]=="accepted",replay
            time.sleep(.1)
            assert sum(event["event"]=="wm_delete_window" for event in events(apps["B"]["log"]))==1,"same operation was natively re-dispatched while B remained alive"
            assert row("B") is not None and child.poll() is None
            pending_replay.update(request=request,receipt=replay,native_b_was_alive=True)
            # The synthetic app completes its already-received WM_DELETE after
            # the replay check; it refuses this ack without a native request.
            with apps["B"]["fifo"].open("w") as control:
                control.write("finish_close\n")

        # B stays on hidden tag2 while A retains native focus on tag1.
        assert row("B")["visible"] is False and row("B")["tag_mask"] == 2
        success, before, cursor = freeze_case("background-hidden", ["B"], on_running=replay_while_native_window_is_alive)
        wait(lambda: apps["B"]["process"].poll() == 0 and row("B") is None, "only B received native close")
        assert success["result"]["status"] == "accepted", success
        assert all(record["state"]=="succeeded" for record in success["synthetic_local_ledger"]["operations"].values()),success
        native = unchanged(before, cursor, ("B",))
        report["steps"].append({"name": "hidden_background_close_without_refocus", "native_evidence": native, "result": success["result"]})
        requests = success["requests_for_historical_replay"]
        assert len(requests) == 1
        assert requests[0]==pending_replay["request"],"live replay must be byte-equivalent canonical guarded request"
        report["steps"].append({"name":"pending_operation_replay_while_native_B_alive_did_not_resend_WM_DELETE","evidence":pending_replay})
        replay_cursor = cursors()
        replay_before = protected_snapshot()
        replay = command("guarded_execute", requests[0])
        assert replay["data"]["status"] == "succeeded"
        report["steps"].append({"name": "historical_operation_replay_no_native_redispatch", "native_evidence": unchanged(replay_before, replay_cursor), "result": replay["data"]})
        assert sha(args.jwm) == report["elf_sha256"]["jwm"]
        assert sha(args.probe) == report["elf_sha256"]["eva_probe"]
        report["wm_identity"] = identity(int(query("get_execution_context")["wm_pid"]))
        report["status"] = "passed"
    except Exception as error:
        report["error"] = repr(error)
    finally:
        stopping.set()
        if heartbeat:
            heartbeat.join(timeout=3)
        children.close()  # Only Popen-owned process groups, never user sessions.
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if report["status"] == "passed" and not args.keep:
            shutil.rmtree(work)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm", type=Path, required=True, help="frozen final core Release ELF")
    parser.add_argument("--probe", type=Path, required=True, help="unit-test ELF with native_close_probe ignored test")
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory")
    parser.add_argument("--runtime-lib", type=Path, help="same pinned PipeWire runtime lib used to link probe")
    parser.add_argument("--backends", default="xcb,x11rb")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    args.jwm, args.probe, args.output = args.jwm.resolve(strict=True), args.probe.resolve(strict=True), args.output.resolve()
    backends = args.backends.split(",")
    if not backends or len(set(backends))!=len(backends) or any(backend not in ("xcb", "x11rb") for backend in backends):
        parser.error("only owned X11 xcb/x11rb backends are supported")
    for dependency in ("cc", "Xvfb", "Xephyr", "xdotool", "dbus-run-session"):
        if not shutil.which(dependency):
            parser.error("missing fixture dependency: " + dependency)
    args.output.mkdir(parents=True, mode=0o700, exist_ok=False)
    client = args.output / "owned-native-client"
    subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", "-std=c11", str(ROOT / "scripts/fixtures/eva-owned-close-client.c"), "-lxcb", "-o", str(client)], check=True)
    reports = [run_case(args, backend, client) for backend in backends]
    summary = {"status": "passed" if all(report["status"] == "passed" for report in reports) else "failed", "runs": reports}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return 0 if summary["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
