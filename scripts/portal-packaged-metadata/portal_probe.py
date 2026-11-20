#!/usr/bin/python3
"""Isolated, routing-only JWM Portal probe. Run with /usr/bin/python3 -I."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time
from xml.sax.saxutils import escape

BASE = "0631f2639676e6dc1cc4d1c81cdad8d0a2804e17"
OBJ = "/org/freedesktop/portal/desktop"
FRONT = "org.freedesktop.portal.Desktop"
BUS = "org.freedesktop.DBus"
BUSOBJ = "/org/freedesktop/DBus"
IMPL = "org.freedesktop.impl.portal"
PORTAL = "org.freedesktop.portal"
ENV_KEYS = ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP")
DAEMON = Path("/usr/bin/dbus-daemon")
BROKERS = tuple(Path(p) for p in ("/usr/libexec/xdg-desktop-portal",
                                  "/usr/lib/xdg-desktop-portal/xdg-desktop-portal"))
TRACK = "org.jwm.PortalProbe"
TIMEOUT_MS = 3500


def gio():
    global Gio, GLib
    import gi
    gi.require_version("Gio", "2.0")
    from gi.repository import Gio, GLib


def var(sig, values):
    return GLib.Variant(sig, values)


def call(conn, name, path, iface, method, args=None, expected=None):
    return conn.call_sync(name, path, iface, method, args,
                          GLib.VariantType.new(expected) if expected else None,
                          Gio.DBusCallFlags.NONE, TIMEOUT_MS, None).unpack()


def dbus(conn, method, args=None, expected=None):
    return call(conn, BUS, BUSOBJ, BUS, method, args, expected)


def connect(address):
    flags = (Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT |
             Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION)
    return Gio.DBusConnection.new_for_address_sync(address, flags, None, None)


def env_sample(source):
    return {k: source.get(k) for k in ENV_KEYS}


def phase_env(name):
    if name == "foreign":
        return {**phase_env("wayland"), "XDG_CURRENT_DESKTOP": "FixtureOther"}
    if name == "wayland":
        return {"XDG_CURRENT_DESKTOP": "jwm", "XDG_SESSION_TYPE": "wayland",
                "WAYLAND_DISPLAY": "wayland-fixture"}
    if name == "x11":
        return {"XDG_CURRENT_DESKTOP": "jwm", "XDG_SESSION_TYPE": "x11", "DISPLAY": ":199"}
    raise ValueError(name)


def service_text(name, role, journal):
    command = [sys.executable, "-I", str(Path(__file__).resolve()), "--fixture", role,
               "--name", name, "--journal", str(journal)]
    # GKeyFile Exec uses shell-style lexical quoting, not a command interpreter.
    import shlex
    return "[D-BUS Service]\nName=" + name + "\nExec=" + shlex.join(command) + "\n"


def fixture(role, name, journal):
    os.umask(0o077)
    if role == "broker":
        broker = next(p for p in BROKERS if os.access(p, os.X_OK))
        with journal.open("a") as fp:
            fp.write(json.dumps({"role": role, "pid": os.getpid(), "name": name,
                                 "phase": "activated", "env": env_sample(os.environ)}) + "\n")
        log = os.open(journal.with_name("broker.log"), os.O_CREAT | os.O_APPEND | os.O_WRONLY, 0o600)
        os.dup2(log, 1)
        os.dup2(log, 2)
        os.close(log)
        os.execve(broker, [str(broker), "--verbose"], os.environ)
    conn = connect(os.environ["DBUS_SESSION_BUS_ADDRESS"])
    def emit(entry):
        line = {"role": role, "pid": os.getpid(), "name": name, **entry}
        with journal.open("a") as fp:
            fp.write(json.dumps(line) + "\n")

    if role == "observe":
        iface, xml = TRACK, '<method name="Ready"><arg type="u" direction="out"/></method>'
    elif role == "jwm":
        iface = IMPL + ".ScreenCast"
        xml = (
            '<property name="version" type="u" access="read"/>'
            '<property name="AvailableSourceTypes" type="u" access="read"/>'
            '<property name="AvailableCursorModes" type="u" access="read"/>'
            '<method name="CreateSession"><arg type="o" direction="in"/><arg type="o" direction="in"/>'
            '<arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>'
            '<arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>'
        )
    else:
        iface = IMPL + ".FileChooser"
        xml = ('<method name="OpenFile"><arg type="o" direction="in"/><arg type="s" direction="in"/>'
               '<arg type="s" direction="in"/><arg type="s" direction="in"/>'
               '<arg type="a{sv}" direction="in"/><arg type="u" direction="out"/>'
               '<arg type="a{sv}" direction="out"/></method>')

    def handle(_conn, _sender, _path, _iface, method, args, invocation):
        values = args.unpack()
        emit({"phase": "method", "method": method, "path": values[0] if values else None,
              "title": values[3] if method == "OpenFile" else None})
        result = var("(u)", (os.getpid(),)) if role == "observe" else var("(ua{sv})", (1, {}))
        invocation.return_value(result)

    def prop(_conn, _sender, _path, _iface, key):
        return var("u", {"version": 4, "AvailableSourceTypes": 1,
                         "AvailableCursorModes": 1}[key])

    info = Gio.DBusNodeInfo.new_for_xml(f'<node><interface name="{iface}">{xml}</interface></node>')
    conn.register_object(OBJ, info.interfaces[0], handle, prop if role == "jwm" else None, None)
    emit({"phase": "activated", "env": env_sample(os.environ)})
    acquired, = dbus(conn, "RequestName", var("(su)", (name, 4)), "(u)")
    if acquired != 1:
        raise RuntimeError(f"private fixture name not acquired: {name}")
    loop = GLib.MainLoop()
    conn.connect("closed", lambda *_: loop.quit())
    loop.run()


def read_address(process):
    ready, _, _ = select.select([process.stdout], [], [], 4)
    if not ready:
        raise RuntimeError("private bus did not print its address within 4s")
    address = process.stdout.readline().strip()
    if not address:
        raise RuntimeError("private bus exited before reporting its address")
    return address


def request(conn, iface, method, options, prefix=()):
    sender = conn.get_unique_name()[1:].replace(".", "_")
    token = "fixture_" + method
    path = f"/org/freedesktop/portal/desktop/request/{sender}/{token}"
    owner, = dbus(conn, "GetNameOwner", var("(s)", (FRONT,)), "(s)")
    loop = GLib.MainLoop()
    responses = []

    def signal(_conn, _sender, _object, _iface, _name, params, _data):
        responses.append(params.unpack())
        loop.quit()

    sub = conn.signal_subscribe(owner, PORTAL + ".Request", "Response", path, None,
                                Gio.DBusSignalFlags.NONE, signal, None)
    timer = GLib.timeout_source_new(TIMEOUT_MS)
    timer.set_callback(lambda *_: (loop.quit(), False)[1])
    timer.attach(None)
    try:
        options = {**options, "handle_token": var("s", token)}
        actual, = call(conn, FRONT, OBJ, iface, method,
                       var("(" + "s" * len(prefix) + "a{sv})", (*prefix, options)), "(o)")
        if actual != path:
            raise RuntimeError(f"unexpected request path {actual!r}, expected {path!r}")
        if not responses:
            loop.run()
        if not responses or responses[0][0] != 1:
            raise RuntimeError(f"expected deliberate cancellation for {method}: {responses!r}")
        return path
    finally:
        timer.destroy()
        conn.signal_unsubscribe(sub)


def journal_read(path):
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


class Session:
    def __init__(self, root, output, initial):
        self.root, self.output, self.processes = root, output, []
        for d in ("home", "runtime", "data", "config", "system-data", "system-config",
                  "services", "portal-override"):
            (root / d).mkdir(mode=0o700)
        self.env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": str(root / "home"),
                    "XDG_RUNTIME_DIR": str(root / "runtime"), "XDG_DATA_HOME": str(root / "data"),
                    "XDG_CONFIG_HOME": str(root / "config"),
                    "XDG_DATA_DIRS": str(root / "system-data"),
                    "XDG_CONFIG_DIRS": str(root / "system-config"), **initial}
        self.journal = root / "receipts.jsonl"

    def start(self, args, tag, env, stdout=None):
        with (self.output / (tag + ".log")).open("w") as log:
            p = subprocess.Popen(args, env=env, stdin=subprocess.DEVNULL,
                                 stdout=stdout if stdout is not None else log, stderr=log,
                                 text=True, start_new_session=True)
        self.processes.append(p)
        return p

    def bus_start(self):
        services = escape(str(self.root / "services"))
        bus_path = self.root / "runtime" / "bus"
        config = self.root / "bus.conf"
        config.write_text(f"""<busconfig><type>session</type><auth>EXTERNAL</auth>
<listen>unix:path={escape(str(bus_path))}</listen><servicedir>{services}</servicedir>
<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/>
<allow own="*"/></policy></busconfig>""")
        process = self.start([str(DAEMON), "--nofork", "--print-address=1",
                              "--config-file=" + str(config)], "bus", self.env,
                             subprocess.PIPE)
        address = read_address(process)
        if not address.startswith(f"unix:path={bus_path},guid="):
            raise RuntimeError(f"private bus returned unexpected address: {address!r}")
        self.env["DBUS_SESSION_BUS_ADDRESS"] = address
        import socket, struct
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as peer:
            peer.settimeout(2)
            peer.connect(str(bus_path))
            pid, uid, _gid = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if (pid, uid) != (process.pid, os.getuid()):
            raise RuntimeError("private bus peer PID/UID mismatch")
        self.conn = connect(address)

    def register(self, name, role):
        (self.root / "services" / (name + ".service")).write_text(service_text(name, role, self.journal))

    def activate_observer(self, suffix):
        name = TRACK + "." + suffix
        pid, = call(self.conn, name, OBJ, TRACK, "Ready", expected="(u)")
        actual, = dbus(self.conn, "GetConnectionUnixProcessID", var("(s)", (name,)), "(u)")
        entries = [e for e in journal_read(self.journal)
                   if e["name"] == name and e["phase"] == "activated"]
        if pid != actual or len(entries) != 1 or entries[0]["pid"] != pid:
            raise RuntimeError("observer not identified on private bus")
        return entries[0]["env"]

    def teardown(self):
        failures = []
        # D-Bus activated fixtures stop on bus EOF; manual children are reaped here.
        for p in reversed(self.processes):
            try:
                if p.poll() is None:
                    p.terminate()
                    try:
                        p.wait(2)
                    except subprocess.TimeoutExpired:
                        p.kill()
                        p.wait(2)
            except (OSError, subprocess.TimeoutExpired) as err:
                failures.append(str(err))
            finally:
                if p.stdout is not None:
                    p.stdout.close()
        if (broker_log := self.root / "broker.log").is_file():
            (self.output / "broker.log").write_bytes(broker_log.read_bytes())
        if self.journal.exists():
            (self.output / "receipts.jsonl").write_bytes(self.journal.read_bytes())
        if failures:
            raise RuntimeError("; ".join(failures))


def git(repo, *args):
    return subprocess.check_output(["/usr/bin/git", "-C", str(repo), *args], timeout=4)


def exact_files(repo):
    extra = ("portal/dist/jwm.portal", "portal/dist/jwm-portals.conf",
             "jwm-x11rb.desktop", "jwm-wayland.desktop")
    return {path: git(repo, "show", BASE + ":" + path) for path in extra}


def stable_hash(body):
    return hashlib.sha256(body).hexdigest()


def activation_observation(s, phase, switch):
    s.register(TRACK + ".First", "observe")
    if switch:
        s.register(TRACK + ".Second", "observe")
        s.register(TRACK + ".Third", "observe")
    dbus(s.conn, "ReloadConfig", expected="()")
    first = s.activate_observer("First")
    if first != env_sample(phase_env(phase)):
        raise RuntimeError(f"cold activation mismatch: {first!r}")
    evidence = {"first": first}
    if switch:
        # The next caller's process-local login values alone do not update the bus.
        # Activate once without the update, then activate another name afterwards.
        inherited = s.activate_observer("Second")
        if inherited != first:
            raise RuntimeError(f"activation environment changed without import: {inherited!r}")
        # Model a surviving user bus and import ONLY present variables.
        # This does not execute a display-manager or compositor startup.
        dbus(s.conn, "UpdateActivationEnvironment", var("(a{ss})", (phase_env(switch),)), "()")
        second = s.activate_observer("Third")
        model = env_sample({**phase_env(phase), **phase_env(switch)})
        if second != model:
            raise RuntimeError(f"transition observer mismatch: {second!r}")
        evidence.update(before_import=inherited, after_import=second, stale_wayland_display=(
            switch == "x11" and second["WAYLAND_DISPLAY"] is not None))
    return evidence


SYSTEM_METADATA = Path("/usr/share/xdg-desktop-portal/portals/jwm.portal")


def check_package_routes(routes, expected_state):
    expected_screencast = "routed-to-fixture" if expected_state == "present" else "not-exported"
    actual = {name: row["status"] for name, row in routes.items()}
    expected = {"FileChooser": "routed-to-fixture", "ScreenCast": expected_screencast}
    if actual != expected:
        raise RuntimeError(f"unexpected normal-discovery routes: {actual!r}, expected {expected!r}")


def routing(s, files, broker_path, mode):
    cfg = files["portal/dist/jwm-portals.conf"]
    jwm = files["portal/dist/jwm.portal"]
    cfg_dir = Path(s.env["XDG_CONFIG_HOME"]) / "xdg-desktop-portal"
    data_dir = Path(s.env["XDG_DATA_HOME"]) / "xdg-desktop-portal" / "portals"
    cfg_dir.mkdir()
    data_dir.mkdir(parents=True)
    (cfg_dir / "jwm-portals.conf").write_bytes(cfg)
    (cfg_dir / "JWM-portals.conf").write_bytes(cfg)
    (data_dir / "jwm.portal").write_bytes(jwm)

    # Native implementation is deliberately not used here. In the discovery
    # case the system broker must find the private descriptor unaided.
    s.register(IMPL + ".desktop.jwm", "jwm")
    s.register(IMPL + ".desktop.gtk", "gtk")
    if mode != "discover":
        dest = s.root / "portal-override"
        (dest / "jwm.portal").write_bytes(jwm)
        controlled = (cfg.replace(b"default=jwm\n", b"default=jwm\n"
                                  b"org.freedesktop.impl.portal.FileChooser=gtk\n")
                      if mode == "explicit" else cfg)
        (dest / "jwm-portals.conf").write_bytes(controlled)
        (dest / "gtk.portal").write_text(
            "[portal]\nDBusName=" + IMPL + ".desktop.gtk\nInterfaces=" + IMPL + ".FileChooser\n"
            "UseIn=GNOME\n")
        dbus(s.conn, "UpdateActivationEnvironment",
             var("(a{ss})", ({"XDG_DESKTOP_PORTAL_DIR": str(dest)},)), "()")
    s.register(FRONT, "broker")
    dbus(s.conn, "ReloadConfig", expected="()")
    xml, = call(s.conn, FRONT, OBJ, "org.freedesktop.DBus.Introspectable", "Introspect",
                expected="(s)")
    pid, = dbus(s.conn, "GetConnectionUnixProcessID", var("(s)", (FRONT,)), "(u)")
    entry = [e for e in journal_read(s.journal) if e["role"] == "broker"]
    if len(entry) != 1 or entry[0]["pid"] != pid or not os.path.samefile(f"/proc/{pid}/exe", broker_path):
        raise RuntimeError("actual broker and activation witness ownership mismatch")
    # Inspect the real broker process, not the launcher shell; never persist
    # its full environment. Subsequent route checks bind receipts to owner PIDs.
    if mode == "discover":
        broker_env = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
        if any(row.startswith(b"XDG_DESKTOP_PORTAL_DIR=") for row in broker_env):
            raise RuntimeError("normal-discovery broker inherited the test-only metadata override")
        entry[0]["portal_metadata_override_absent"] = True
    (s.output / "introspection.xml").write_text(xml)
    (s.output / "broker-identity.json").write_text(json.dumps(entry[0], indent=2) + "\n")
    result = {}
    specs = (
        ("FileChooser", "OpenFile", ("", "JWM fixture OpenFile"), {}, "gtk"),
        ("ScreenCast", "CreateSession", (), {"session_handle_token": var("s", "fixture_session")}, "jwm"),
    )
    for interface, method, prefix, options, role in specs:
        name = PORTAL + "." + interface
        if f'name="{name}"' not in xml:
            result[interface] = {"status": "not-exported"}
            continue
        try:
            path = request(s.conn, name, method, options, prefix)
            matches = [e for e in journal_read(s.journal)
                       if e["phase"] == "method" and e["role"] == role
                       and e["method"] == method and e["path"] == path
                       and (role != "gtk" or e["title"] == prefix[1])]
            if len(matches) != 1:
                raise RuntimeError(f"expected exactly one {role} receipt, got {matches}")
            pid, = dbus(s.conn, "GetConnectionUnixProcessID",
                        var("(s)", (IMPL + ".desktop." + role,)), "(u)")
            if pid != matches[0]["pid"]:
                raise RuntimeError(f"backend receipt PID mismatch for {role}")
            result[interface] = {"status": "routed-to-fixture", "receipt": matches[0]}
        except (RuntimeError, GLib.Error) as err:
            result[interface] = {"status": "call-not-confirmed", "error": str(err)}
    return result


def run(args):
    if os.geteuid() == 0:
        raise RuntimeError("use an unprivileged disposable account, not root")
    os.umask(0o077)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {"base": BASE, "status": "setup-error", "stages": {},
              "not_covered": ["production display-manager login", "systemd user manager",
                              "native compositor capture", "legacy X11 pixels", "OpenURI"]}
    code = 2
    inconclusive = False
    try:
        files = exact_files(args.repo)
        report["source_sha256"] = {k: stable_hash(v) for k, v in files.items()}
        broker = next((p for p in BROKERS if os.access(p, os.X_OK)), None)
        report["broker"] = str(broker) if broker else None
        if broker:
            report["broker_sha256"] = stable_hash(broker.read_bytes())
            report["broker_version"] = subprocess.check_output(
                [str(broker), "--version"], text=True,
                env={"PATH": "/usr/bin:/bin", "LANG": "C"}, timeout=4).strip()
        phases = [("cold-x11", "x11", None, None), ("cold-wayland", "wayland", None, None),
                  ("x11-to-wayland", "x11", "wayland", None),
                  ("wayland-to-x11", "wayland", "x11", None),
                  ("foreign-desktop-to-x11", "foreign", "x11", None)]
        if broker and not args.activation_only:
            phases += [("controlled-explicit", "wayland", None, "explicit"),
                       ("controlled-default", "wayland", None, "default"),
                       ("discover-x11", "x11", None, "discover"),
                       ("discover-wayland", "wayland", None, "discover"),
                       ("discover-wayland-to-x11", "wayland", "x11", "discover"),
                       ("discover-foreign-to-x11", "foreign", "x11", "discover")]
        else:
            report["stages"]["actual-broker"] = {"status": "not-run", "reason":
                "activation-only requested" if args.activation_only else "frontend executable not installed"}
        if args.package_state:
            if not broker or report["broker_version"] != "xdg-desktop-portal 1.18.4":
                raise RuntimeError("expected actual Ubuntu 24.04 broker 1.18.4")
            checked_in = args.repo / "portal/dist/jwm.portal"
            descriptor = files["portal/dist/jwm.portal"]
            if checked_in.read_bytes() != descriptor:
                raise RuntimeError("checked-in descriptor differs from pinned baseline")
            if args.package_state == "present":
                if SYSTEM_METADATA.is_symlink() or not SYSTEM_METADATA.is_file() or SYSTEM_METADATA.read_bytes() != descriptor:
                    raise RuntimeError("package descriptor missing, not regular, or different from checked-in bytes")
            elif SYSTEM_METADATA.exists() or SYSTEM_METADATA.is_symlink():
                raise RuntimeError("system descriptor exists in an absent phase")
            report["package_state"] = args.package_state
            report["descriptor_sha256"] = stable_hash(descriptor)
            phases = [("discover-wayland", "wayland", None, "discover")]
        for label, _, _, _ in phases:
            report["stages"][label] = {"status": "not-run"}
        for label, phase, switch, mode in phases:
            out = args.output / label
            out.mkdir()
            row = {"status": "started"}
            report["stages"][label] = row
            with tempfile.TemporaryDirectory(prefix="jwm-p-", dir="/tmp") as tmp:
                s = Session(Path(tmp), out, phase_env(phase))
                try:
                    row["stage"] = "private-bus"
                    s.bus_start()
                    row["stage"] = "activation-environment"
                    row["activation"] = activation_observation(s, phase, switch)
                    if mode is not None:
                        row["stage"] = "broker-routing"
                        row["routing"] = routing(s, files, broker, mode)
                        if args.package_state:
                            check_package_routes(row["routing"], args.package_state)
                        inconclusive |= any(v["status"] == "call-not-confirmed"
                                            for v in row["routing"].values())
                        if mode == "explicit" and not all(v["status"] == "routed-to-fixture"
                                                for v in row["routing"].values()):
                            raise RuntimeError("actual broker controlled positive did not confirm both fixture routes")
                    row["status"] = "observed"
                except (RuntimeError, OSError, GLib.Error) as err:
                    row.update(status="setup-error", error=str(err))
                    raise
                finally:
                    s.teardown()
        if inconclusive:
            report["status"] = "routing-inconclusive"
        elif broker is not None or args.activation_only:
            report["status"] = "observed-activation-only" if args.activation_only else "observed-routing"
            code = 0
        else:
            report["status"] = "setup-error-missing-broker"
    except KeyboardInterrupt:
        report["status"] = "interrupted"
    except (RuntimeError, OSError, GLib.Error, subprocess.SubprocessError) as err:
        report["error"] = str(err)
    finally:
        (args.output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
    return code


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repo", type=Path)
    p.add_argument("--output", type=Path)
    p.add_argument("--activation-only", action="store_true")
    p.add_argument("--package-state", choices=("absent", "present"))
    p.add_argument("--fixture", choices=("observe", "gtk", "jwm", "broker"), help=argparse.SUPPRESS)
    p.add_argument("--name", help=argparse.SUPPRESS)
    p.add_argument("--journal", type=Path, help=argparse.SUPPRESS)
    a = p.parse_args()
    gio()
    if a.fixture:
        return fixture(a.fixture, a.name, a.journal)
    if a.package_state and a.activation_only:
        p.error("--package-state requires the actual broker")
    if not a.repo or not a.output:
        p.error("--repo and --output (new directory) are required")
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    return run(a)


if __name__ == "__main__":
    sys.exit(main())
