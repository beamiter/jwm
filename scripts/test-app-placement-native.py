#!/usr/bin/env python3
"""Real two-application, two-tag placement acceptance on an owned X display.

Default invocation needs release jwm, Xvfb, xterm (X11) or the native shell
client example (Wayland). --self-test needs no desktop and proves only harness
logic and failure propagation, not JWM placement. Python 3.11+ is required.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time
import tomllib
import traceback
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("desktop_ipc", ROOT / "eva/tests/desktop-ipc.py")
DESKTOP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DESKTOP)
# X11 normalizes WM_CLASS to lowercase; native Wayland preserves app_id.
# Use identical canonical identifiers on both protocols, not a loose assertion.
APP_IDS = {"A": "org.jwm.placementfixturea", "B": "org.jwm.placementfixtureb"}
TAG1, TAG2 = 1, 2  # tag masks, not indices


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def wait_for(probe, label, timeout=12):
    end, last = time.monotonic() + timeout, None
    while time.monotonic() < end:
        try:
            result = probe()
            if result:
                return result
        except (OSError, RuntimeError, ValueError) as error:
            last = str(error)
        time.sleep(.035)
    raise AssertionError(f"{label}: timeout (last error: {last})")


def isolate_env(work):
    keep = ("PATH", "LANG", "LC_ALL", "LC_CTYPE", "LD_LIBRARY_PATH", "TZ")
    env = {k: os.environ[k] for k in keep if k in os.environ}
    for key, name in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                      ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                      ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")):
        p = work / name
        p.mkdir(mode=0o700)
        env[key] = str(p)
    env.update(LIBGL_ALWAYS_SOFTWARE="1", WINIT_X11_SCALE_FACTOR="1",
               JWM_COMPOSITOR_API="glx",
               DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={work / 'no-system-bus'}")
    return env


def configured(base, global_memory, policies):
    text = DESKTOP.edit_setting(
        base, "behavior", "remember_closed_placement", str(global_memory).lower()
    )
    # A generated empty top-level array conflicts with array-of-tables syntax.
    text = re.sub(r"(?m)^rules\s*=\s*\[\s*\][ \t]*$", "", text)
    for letter, policy in policies.items():
        require(policy in (None, "current", "remember", "inherit"), "invalid fixture policy")
        text += (
            f'\n[[rules]]\nclass = "{APP_IDS[letter]}"\ninstance = ""\nname = ""\n'
            'tags = 0\nis_floating = false\nmonitor = -1\n'
        )
        if policy is not None:
            text += f'placement = "{policy}"\n'
    parsed = tomllib.loads(text)
    require(parsed["behavior"]["remember_closed_placement"] == global_memory, "bad global config")
    require([rule["class"] for rule in parsed["rules"] if rule["class"] in APP_IDS.values()]
            == [APP_IDS[key] for key in policies], "bad application rules")
    return text


def select_window(rows, letter, title, pid=None):
    matches = [w for w in rows if w.get("name") == title
               and (pid is None or w.get("pid") == pid)]
    require(len(matches) <= 1, f"ambiguous fixture title/PID: {title}")
    if not matches:
        return None
    row = matches[0]
    require(row.get("class") == APP_IDS[letter], f"unexpected app identity: {row}")
    require(isinstance(row.get("pid"), int) and row["pid"] > 0, "missing native client PID")
    return row


def assert_window_state(row, mask, on_view, focused=None):
    require(row["tags"] == mask and row["is_on_view"] is on_view,
            f"unexpected tag/visibility: expected {mask}/{on_view}, got {row}")
    if focused is not None:
        require(row["is_focused"] is focused, f"unexpected focus: {row}")


def run_session(args, output, work, report, children):
    env = isolate_env(work)
    native = args.backend == "wayland-winit"
    binary = args.jwm.resolve()
    client_binary = args.client_bin.resolve() if native else Path(shutil.which("xterm") or "")
    for tool in ("Xvfb", "dbus-run-session", "timeout", "xdotool"):
        require(shutil.which(tool), f"missing {tool}")
    for executable in (binary, client_binary):
        require(executable.is_file() and os.access(executable, os.X_OK),
                f"missing executable {executable}")
    for key, executable in (("jwm_sha256", binary), ("client_sha256", client_binary)):
        with executable.open("rb") as f:
            report[key] = hashlib.file_digest(f, "sha256").hexdigest()

    config_dir = work / "config/jwm"
    config_dir.mkdir(mode=0o700)
    config_path = config_dir / ("config_wayland.toml" if native else "config_x11.toml")

    def config_cli(option):
        result = subprocess.run([str(binary), "--backend", args.backend, option],
                                env=env, capture_output=True, text=True, timeout=30)
        with (output / "config-cli.log").open("a") as log:
            log.write(f"{option}: exit={result.returncode}\n{result.stdout}{result.stderr}")
        require(result.returncode == 0, f"{option} failed; see config-cli.log")

    config_cli("--gen-config")
    base = config_path.read_text()
    for section, key, setting, required in (
        ("status_bar", "show_bar", "false", True),
        ("status_bar", "name", '"/bin/true"', True),
        ("desktop_shell", "enabled", "false", False),
        ("behavior", "compositor", str(native).lower(), True),
        ("behavior", "focus_follows_new_window", "true", True),
        ("behavior", "wallpaper_effect", '"off"', False),
        ("animation", "enabled", "false", True),
    ):
        base = DESKTOP.edit_setting(base, section, key, setting, required=required)

    def write_config(name, global_memory, policies):
        text = configured(base, global_memory, policies)
        staged = config_path.with_suffix(".new")
        staged.write_text(text)
        staged.replace(config_path)
        (output / f"config-{name}.toml").write_text(text)
        config_cli("--check-config")

    write_config("seed", False, {"A": "remember", "B": "remember"})
    env["DISPLAY"] = children.display("Xvfb", env, output)
    wm = children.spawn(["dbus-run-session", "--", str(binary), "--backend", args.backend],
                        env, output / "jwm.log")
    socket = work / "runtime/jwm-ipc.sock"

    def query(name):
        require(wm.poll() is None, f"JWM exited ({wm.returncode}); see jwm.log")
        return DESKTOP.query(socket, name)

    def command(name, values=None):
        return DESKTOP.command(socket, name, values or {})

    wait_for(lambda: query("get_monitors"), "private JWM with live output", timeout=30)
    native_socket = None
    def xdotool(*argv, allow_empty=False):
        result = subprocess.run(["xdotool", *map(str, argv)], env=env,
                                capture_output=True, text=True, timeout=5)
        if allow_empty and result.returncode == 1 and not result.stdout:
            return ""
        require(result.returncode == 0, f"xdotool {argv}: {result.stderr}")
        return result.stdout

    if native:
        outer_rows = wait_for(
            lambda: xdotool("search", "--name", "JWM ", allow_empty=True).splitlines(),
            "owned winit output window",
        )
        require(len(outer_rows) == 1, "ambiguous winit outer window")
        outer = int(outer_rows[0])
        xdotool("windowsize", outer, 1280, 800)
        xdotool("windowfocus", "--sync", outer)
        report["outer_xid"] = outer
        wait_for(lambda: any(w["w"] == 1280 and w["h"] == 800 for w in query("get_monitors")),
                 "resized nested output")
        native_socket = wait_for(
            lambda: next((p.name for p in (work / "runtime").glob("wayland-*") if p.is_socket()), None),
            "owned Wayland socket",
        )
    # Pointer is parked before mapping any application; no later injected
    # pointer motion can overwrite the explicitly chosen anchor focus.
    xdotool("mousemove", "0", "0")
    report["transport"] = "native wl_shm/xdg-shell" if native else "native X11"

    def view(mask):
        command("view", {"tag": mask})
        wait_for(lambda: [r["tag_mask"] for r in query("get_workspaces") if r["focused"]] == [mask],
                 f"view {mask}")

    def focus(row):
        command("focus_window", {"id": row["id"]})
        wait_for(lambda: any(w["id"] == row["id"] and w["is_focused"] for w in query("get_windows")),
                 f"focus {row['name']}")

    serial = 0

    def spawn(letter, by_jwm=False):
        nonlocal serial
        serial += 1
        title = f"JWM Placement Fixture {letter} {serial}"
        client_env = env.copy()
        if native:
            client_env.pop("DISPLAY", None)
            client_env["WAYLAND_DISPLAY"] = native_socket
            client_env["JWM_TEST_EVENT_LOG"] = str(output / f"client-{serial}.events")
            argv = [str(client_binary), "--title", title, "--app-id", APP_IDS[letter]]
        else:
            argv = [str(client_binary), "-class", APP_IDS[letter], "-name", APP_IDS[letter],
                    "-title", title, "-e", "/bin/sleep", "600"]
        if by_jwm:
            # The client spawned by JWM must have a real child PID recorded by
            # the runtime, while the outer display remains inaccessible to it.
            if native:
                argv = ["/usr/bin/env", "-u", "DISPLAY", f"WAYLAND_DISPLAY={native_socket}",
                        f"JWM_TEST_EVENT_LOG={client_env['JWM_TEST_EVENT_LOG']}", *argv]
            # A failed assertion must not orphan a JWM-owned child if the WM
            # spawned it into a separate session. Its timeout parent is a
            # normal ancestor that JWM's launch attribution should recognize.
            command("spawn", {"cmd": [shutil.which("timeout"), "--foreground", "30s", *argv]})
            process = None
        else:
            process = children.spawn(argv, client_env, output / f"client-{serial}.log")
        row = wait_for(lambda: select_window(query("get_windows"), letter, title,
                                              process.pid if process else None),
                       f"new managed {title}")
        return row, process

    def read_history():
        path = work / "state/jwm/closed_placement.json"
        if not path.exists():
            return {}
        entries = json.loads(path.read_text())["placements"]
        return {(r["class"], r["instance"]): r["tags"] for r in entries}

    def historical_tags(row):
        return read_history().get((row["class"], row["instance"]))

    def shutdown(row, process):
        require(process is not None and process.poll() is None, "expected an owned live client")
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=5)
        wait_for(lambda: all(w["id"] != row["id"] for w in query("get_windows")),
                 f"unmanage {row['name']}")

    def change(name, global_memory, policies):
        write_config(name, global_memory, policies)
        command("reload_config")

    view(TAG1)
    for letter in APP_IDS:
        row, proc = spawn(letter)
        assert_window_state(row, TAG1, True)
        shutdown(row, proc)
        require(historical_tags(row) == TAG1, f"global-false remember did not write {letter}")
    require(len(read_history()) == 2, "both application identities need independent saved entries")
    report["cases"].append({"name": "global_false_two_independent_history_seeds", "history_tags": [1, 1]})

    change("selective", False, {"A": "current", "B": "remember"})
    view(TAG2)
    current, proc_a = spawn("A")
    assert_window_state(current, TAG2, True)
    focus(current)
    remembered, proc_b = spawn("B")
    assert_window_state(remembered, TAG1, False, False)
    assert_window_state(select_window(query("get_windows"), "A", current["name"], current["pid"]),
                        TAG2, True, True)
    require([r["tag_mask"] for r in query("get_workspaces") if r["focused"]] == [TAG2],
            "remembered app switched the active tag")
    shutdown(current, proc_a)
    require(historical_tags(current) == TAG1, "current overwrote its old tag with tag 2")
    report["cases"].append({"name": "global_false_current_and_remember", "a": current,
                            "b": remembered, "a_preserved_old_history": True})

    change("swapped", False, {"A": "remember", "B": "current"})
    assert_window_state(select_window(query("get_windows"), "B", remembered["name"], remembered["pid"]),
                        TAG1, False, False)
    shutdown(remembered, proc_b)
    swap_b, proc_b = spawn("B")
    assert_window_state(swap_b, TAG2, True)
    focus(swap_b)
    swap_a, proc_a = spawn("A")
    assert_window_state(swap_a, TAG1, False, False)
    assert_window_state(select_window(query("get_windows"), "B", swap_b["name"], swap_b["pid"]),
                        TAG2, True, True)
    shutdown(swap_b, proc_b)
    shutdown(swap_a, proc_a)
    require(historical_tags(swap_b) == TAG1, "swapped current overwrote its history")
    report["cases"].append({"name": "hot_reload_swap_no_live_relocation", "a": swap_a, "b": swap_b})

    change("default", False, {"A": None, "B": "remember"})
    default_a, proc_a = spawn("A")
    assert_window_state(default_a, TAG2, True)
    shutdown(default_a, proc_a)
    require(historical_tags(default_a) == TAG1, "omitted rule changed history while global memory is off")
    report["cases"].append({"name": "omitted_policy_legacy_default_off", "a": default_a})

    change("inherit", True, {"A": "inherit", "B": None})
    from_shell, proc_a = spawn("A")
    assert_window_state(from_shell, TAG1, False, False)
    shutdown(from_shell, proc_a)
    from_jwm, _ = spawn("A", by_jwm=True)
    assert_window_state(from_jwm, TAG2, True)
    # Let the private WM itself send a normal close request to its own child.
    focus(from_jwm)
    command("killclient")
    wait_for(lambda: all(w["id"] != from_jwm["id"] for w in query("get_windows")), "close JWM child")
    report["cases"].append({"name": "global_true_inherit_respects_launch_origin",
                            "external": from_shell, "jwm_spawn": from_jwm})


def execute(args, output, runner=run_session):
    report = {"status": "failed", "backend": args.backend, "cases": [],
              "limits": ["Private single nested output; no physical display, native DRM/KMS, or XWayland.",
                         "Rule reload is tested; compositor restart restoration is out of scope.",
                         "IPC state and history file are checked, not pixel output."]}
    children = DESKTOP.Children()
    try:
        with tempfile.TemporaryDirectory(prefix="jwm-placement-native-") as work:
            try:
                runner(args, output, Path(work), report, children)
                require(len(report["cases"]) == 5, "incomplete native matrix")
            finally:
                children.close()
        report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
        (output / "failure.log").write_text(traceback.format_exc())
    (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"{args.backend}: {report['status']} ({output / 'result.json'})")
    return 0 if report["status"] == "passed" else 1


class Offline(unittest.TestCase):
    def test_configuration_fields(self):
        base = "rules = []\n[behavior]\nremember_closed_placement = false\n"
        data = tomllib.loads(configured(base, False, {"A": None, "B": "remember"}))
        self.assertNotIn("placement", data["rules"][0])
        self.assertEqual(data["rules"][1]["placement"], "remember")
        self.assertFalse(data["behavior"]["remember_closed_placement"])

    def test_identity_requires_pid_and_class(self):
        row = {"name": "one", "pid": 42, "class": APP_IDS["A"]}
        self.assertEqual(select_window([row], "A", "one", 42), row)
        self.assertIsNone(select_window([row], "A", "one", 43))
        with self.assertRaisesRegex(AssertionError, "unexpected app identity"):
            select_window([row], "B", "one", 42)

    def test_wrong_tag_or_focus_is_failure(self):
        row = {"tags": TAG1, "is_on_view": True, "is_focused": True}
        with self.assertRaisesRegex(AssertionError, "tag/visibility"):
            assert_window_state(row, TAG2, True)
        with self.assertRaisesRegex(AssertionError, "focus"):
            assert_window_state(row, TAG1, True, False)

    def test_env_never_inherits_desktop_context(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(
            os.environ, {"DISPLAY": ":47", "WAYLAND_DISPLAY": "user",
                         "JWM_SOCKET": "/user-private", "DBUS_SESSION_BUS_ADDRESS": "user"}
        ):
            env = isolate_env(Path(tmp))
            for key in ("DISPLAY", "WAYLAND_DISPLAY", "JWM_SOCKET", "DBUS_SESSION_BUS_ADDRESS"):
                self.assertNotIn(key, env)
            self.assertTrue(env["HOME"].startswith(tmp))

    def test_failed_and_incomplete_runs_propagate(self):
        def failure(_args, _output, _work, _report, _children):
            raise AssertionError("invariant failed")
        args = argparse.Namespace(backend="x11rb")
        for runner, message in ((failure, "invariant failed"),
                                (lambda *args: None, "incomplete native matrix")):
            with tempfile.TemporaryDirectory() as tmp:
                out = Path(tmp)
                self.assertEqual(execute(args, out, runner), 1)
                report = json.loads((out / "result.json").read_text())
                self.assertEqual(report["status"], "failed")
                self.assertIn(message, report["error"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=("x11rb", "wayland-winit"), default="x11rb")
    parser.add_argument("--jwm", type=Path, default=ROOT / "target/release/jwm")
    parser.add_argument("--client-bin", type=Path, default=ROOT /
                        "crates/jwm_shell/target/debug/examples/native_shell_client")
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(Offline))
        return 0 if result.wasSuccessful() else 1
    output = args.evidence_dir or ROOT / "artifacts" / f"app-placement-{args.backend}"
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    return execute(args, output)


if __name__ == "__main__":
    raise SystemExit(main())
