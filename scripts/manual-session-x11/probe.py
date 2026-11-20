#!/usr/bin/env python3
"""Manual X11 session acceptance, single owned Xvfb output; Linux runner only."""
import argparse
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import select
import signal
import socket
import struct
import subprocess
import tempfile
import time
import tomllib
import traceback

BASELINE = "9b3974b31566a2ae1bb6fe33cfb9642762434bf6"
COMBINED_SHA = "c2fec065c4a891712e7a4f34eb5dfbb8a6893cb6"
COMBINED_TREE = "3e3a3ccd47b62de19702dcc5774cfca2fe1391ea"
SAVED = {"A": 2, "B": 4, "C": 8}
CURRENT = {"A": 16, "B": 32, "C": 64}
TITLE = {name: "JwmManualRestoreFixture" + name for name in SAVED}

class ProbeDeadline(BaseException):
    pass

def check(value, note):
    if not value:
        raise AssertionError(note)

def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")

def verify_tags(role, scene, before, after, saved_order):
    check(isinstance(before, dict) and before and before == {k: CURRENT[k] for k in before},
          "bad pre-restore tag precondition")
    check(set(before) == set(after) == ({"B", "C"} if scene == "survivor" else set(SAVED)),
          "missing or extra live fixture client")
    order = [SAVED[name] for name in saved_order if name in ("A", "B")]
    check(len(order) == 2 and set(saved_order) == set(SAVED) and len(saved_order) == 3, "bad save order")
    check(after["C"] == SAVED["C"], "unique control did not restore")
    if role == "candidate":
        expected = {k: SAVED[k] if scene == "distinct" or k == "C" else CURRENT[k] for k in before}
        check(after == expected, f"unexpected candidate tags: actual {after}, expected {expected}")
    elif scene == "distinct":
        check(after == SAVED, "baseline distinct positive control failed")
    elif scene == "survivor":
        check(after["B"] == order[0], "baseline survivor did not claim first ambiguous saved slot")
    else:
        check({after["A"], after["B"]} == set(order), "baseline duplicates did not claim ambiguous slots")

def ipc(path, key, name, args=None):
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(3)
        conn.connect(str(path))
        conn.sendall(json.dumps({key: name, "args": args or {}}).encode() + b"\n")
        buf = b""
        while b"\n" not in buf:
            chunk = conn.recv(8192)
            check(chunk and len(buf) + len(chunk) <= 1024*1024, "incomplete or oversized IPC reply")
            buf += chunk
        r = json.loads(buf.split(b"\n", 1)[0])
    check(r.get("success") is True, (name, r))
    return r.get("data")

def wait(check_value, label, transient=()):
    until = time.monotonic() + 12
    last = None
    while time.monotonic() < until:
        try:
            value = check_value()
            if value:
                return value
        except transient as e:
            last = repr(e)
        time.sleep(.05)
    raise RuntimeError(f"{label} timed out: {last}")

def auth(path):
    parts = (b"", b"", b"MIT-MAGIC-COOKIE-1", secrets.token_bytes(16))
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as f:
        f.write(struct.pack("!H", 65535) + b"".join(struct.pack("!H", len(x)) + x for x in parts))

def attest_xid(xid):
    class Spec(C.Structure):
        _fields_ = [("client", C.c_ulong), ("mask", C.c_uint)]
    class Value(C.Structure):
        _fields_ = [("spec", Spec), ("length", C.c_long), ("value", C.c_void_p)]
    x = C.CDLL("libX11.so.6")
    res = C.CDLL("libXRes.so.1")
    x.XOpenDisplay.argtypes = [C.c_char_p]; x.XOpenDisplay.restype = C.c_void_p
    x.XCloseDisplay.argtypes = [C.c_void_p]
    res.XResQueryClientIds.argtypes = [C.c_void_p,C.c_long,C.POINTER(Spec),C.POINTER(C.c_long),C.POINTER(C.POINTER(Value))]
    res.XResGetClientPid.argtypes = [C.POINTER(Value)]
    res.XResClientIdsDestroy.argtypes = [C.c_long,C.POINTER(Value)]
    res.XResClientIdsDestroy.restype = None
    d = x.XOpenDisplay(None)
    check(d, "private X display unavailable in attestation helper")
    count = C.c_long()
    vals = C.POINTER(Value)()
    try:
        check(res.XResQueryClientIds(d,1,C.byref(Spec(xid,2)),C.byref(count),C.byref(vals)) == 0, "XRes failed")
        check(0 < count.value <= 16, "XRes returned malformed PID set")
        pids = [pid for v in vals[:count.value] if (pid := res.XResGetClientPid(C.byref(v))) > 0]
        check(len(pids) == 1, "XRes did not attest exactly one PID")
        print(pids[0])
    finally:
        if vals:
            res.XResClientIdsDestroy(count,vals)
        x.XCloseDisplay(d)

def edit_config(p):
    v = p.read_text()
    for section, key, value in (
        ("status_bar", "show_bar", "false"), ("behavior", "eva_companion_enabled", "false"),
        ("behavior", "compositor", "false"), ("animation", "enabled", "false"),
        ("desktop_shell", "enabled", "false"),
    ):
        parsed = tomllib.loads(v)
        if section not in parsed:
            check(section == "desktop_shell", f"missing config section {section}")
            v += f"\n[{section}]\n{key} = {value}\n"
        elif key not in parsed[section]:
            check(section == "desktop_shell", f"missing config key {section}.{key}")
            v = v.replace(f"[{section}]\n", f"[{section}]\n{key} = {value}\n", 1)
        else:
            found = re.search(r"(?ms)^\[" + section + r"\]\n(.*?)(?=^\[|\Z)", v)
            check(found, f"missing table text: {section}")
            body, n = re.subn(r"(?m)^\s*" + key + r"\s*=.*$", f"{key} = {value}", found[1])
            check(n == 1, f"ambiguous config key {section}.{key}")
            v = v[:found.start(1)] + body + v[found.end(1):]
        check(tomllib.loads(v)[section][key] is False, f"config not disabled {section}.{key}")
    p.write_text(v)

def live_leader(child):
    # Do not reap yet: the reserved leader PID prevents reuse of this PGID
    # while we are still responsible for cleaning its descendants.
    return child.returncode is None and os.waitid(
        os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT
    ) is None

def group_live(pgid):
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            fields = (entry/"stat").read_text().rpartition(")")[2].split()
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        if len(fields) > 3 and fields[0] not in ("Z","X") and int(fields[2]) == int(fields[3]) == pgid:
            return True
    return False

def stop_group(child):
    if child.returncode is not None:
        return
    try:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(child.pid,sig)
            except ProcessLookupError:
                pass
            until=time.monotonic()+2
            while time.monotonic()<until and group_live(child.pid):
                time.sleep(.03)
            if not group_live(child.pid):
                child.wait(timeout=2)
                return
        raise RuntimeError(f"owned session {child.pid} still live after SIGKILL")
    except ChildProcessError as error:
        raise RuntimeError(f"lost ownership of child {child.pid}") from error

def session(bin_path, role, scene, out):
    out.mkdir()
    state = {"phase": "setup", "status": "failed", "role": role, "scene": scene}
    write(out/"result.json",state)
    children = []
    commands = []
    errors = []
    interruption = None
    with tempfile.TemporaryDirectory(prefix="jwm-manual-x11-") as d:
        root = Path(d)
        env = {"PATH":"/usr/bin:/bin","LANG":"C.UTF-8","RUST_LOG":"info"}
        for k,v in (("HOME","home"),("XDG_RUNTIME_DIR","runtime"),("XDG_STATE_HOME","state"),
                    ("XDG_DATA_HOME","data"),("XDG_CACHE_HOME","cache"),("XDG_CONFIG_HOME","config")):
            (root/v).mkdir(mode=0o700); env[k] = str(root/v)
        env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path="+str(root/"absent-private-bus")
        env["DBUS_SYSTEM_BUS_ADDRESS"] = env["DBUS_SESSION_BUS_ADDRESS"]
        xserver = None
        def verify():
            check(xserver is not None and live_leader(xserver), "owned Xvfb not live")
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(2)
                sock.connect("/tmp/.X11-unix/X"+env["DISPLAY"][1:])
                pid, uid, _ = struct.unpack("3i",sock.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
            check((pid,uid) == (xserver.pid,os.getuid()), "wrong Xvfb peer")
            check(Path(f"/proc/{pid}/exe").resolve() == Path("/usr/bin/Xvfb").resolve(), "wrong Xvfb exe")
        def run(*argv, extra=None, permit_failure=False, owned=True):
            if owned: verify()
            cmd = list(map(str,argv))
            r = subprocess.run(cmd, env=env if extra is None else {**env,**extra}, text=True,
                               capture_output=True,timeout=8)
            commands.append(dict(argv=cmd,rc=r.returncode,stdout=r.stdout,stderr=r.stderr))
            if owned: verify()
            check(permit_failure or r.returncode == 0, (cmd,r.stderr,r.returncode))
            return r
        def spawn(argv,label,pass_fds=()):
            with (out/(label+".log")).open("wb") as log:
                p=subprocess.Popen(list(map(str,argv)),env=env,start_new_session=True,pass_fds=pass_fds,
                                   stdin=subprocess.DEVNULL,stdout=log,stderr=subprocess.STDOUT)
            children.append(p)
            return p
        def q(name): return ipc(root/"runtime/jwm-ipc.sock","query",name)
        def cmd(name,**args): return ipc(root/"runtime/jwm-ipc.sock","command",name,args)
        def rows(): return [r for r in q("get_windows") if r["name"] in TITLE.values()]
        try:
            for p in ["/usr/bin/Xvfb","/usr/bin/xdotool","/usr/bin/xmessage","/usr/bin/xprop","/usr/bin/xdpyinfo"]:
                check(Path(p).is_file(),f"missing runner dependency: {p}")
            private = root/"authority"; wrong=root/"wrong-authority"; auth(private); auth(wrong)
            env["XAUTHORITY"] = str(private)
            rd,wr = os.pipe()
            try:
                xserver=spawn(["/usr/bin/Xvfb","-displayfd",wr,"-auth",private,"-screen","0","1280x800x24",
                               "-noreset","-nolisten","tcp"],"xvfb",(wr,))
                os.close(wr); wr=None
                check(select.select([rd],[],[],8)[0],"Xvfb readiness timeout")
                n=os.read(rd,32).decode().strip()
                check(n.isascii() and n.isdigit(),"invalid Xvfb displayfd reply")
            finally:
                os.close(rd)
                if wr is not None: os.close(wr)
            env["DISPLAY"]=":"+n
            verify()
            check(run("/usr/bin/xdpyinfo",extra={"XAUTHORITY":str(wrong)},permit_failure=True).returncode != 0,
                  "wrong Xauthority unexpectedly admitted")
            run("/usr/bin/xdpyinfo")
            state["owner"]={"display":env["DISPLAY"],"pid":xserver.pid,"uid":os.getuid(),"peer_verified":True,"wrong_cookie_denied":True}
            run(bin_path,"--backend","x11rb","--gen-config")
            cfg = root/"config/jwm/config_x11.toml"
            edit_config(cfg)
            (out/"config.toml").write_text(cfg.read_text())
            run(bin_path,"--backend","x11rb","--check-config")
            wm=spawn([bin_path,"--backend","x11rb"],"jwm")
            version=wait(lambda:q("get_version"),"IPC ready",(OSError,))
            check(version["backend"]=="x11rb" and live_leader(wm),"wrong backend or dead WM")
            monitors=q("get_monitors")
            check(len(monitors)==1,"single-output contract not met")
            state["monitors"]=monitors
            def managed(name,instance,label):
                verify()
                p=spawn(["/usr/bin/xmessage","-name",instance,"-title",TITLE[name],"-buttons","OK:0",
                         "Private manual restore fixture"],label)
                matched=wait(lambda:[r for r in rows() if r["name"]==TITLE[name]],"managed "+name)
                check(len(matched)==1,"ambiguous title in IPC")
                r=matched[0]
                check(r["class"].lower()=="xmessage" and r["instance"].lower()==instance,"unexpected IPC class/instance")
                raw=run("/usr/bin/xdotool","search","--name","^"+TITLE[name]+"$").stdout.splitlines()
                check(len(raw)==1 and raw[0].isdigit(),"ambiguous native title search")
                xid=int(raw[0])
                owner=run("/usr/bin/python3","-I",Path(__file__).resolve(),"--attest-xid",xid)
                check(int(owner.stdout)==p.pid and live_leader(p),"native window not owned by expected child")
                xprop=run("/usr/bin/xprop","-id",hex(xid),"WM_CLASS","WM_NAME").stdout
                check(re.search(r'WM_CLASS\(STRING\) = "'+instance+r'", "Xmessage"',xprop,re.I) and TITLE[name] in xprop,
                      "native WM_CLASS/title mismatch")
                check(r.get("pid") in (None,0,p.pid),"IPC PID disagrees with XRes-attested owner")
                return (p,xid,r["id"],instance)
            def tag(item, mask):
                cmd("view",value=1)
                cmd("focus_window",id=item[2])
                wait(lambda:any(r["id"]==item[2] and r["is_focused"] for r in rows()),"setup IPC focus")
                cmd("tag",value=mask)
                wait(lambda:any(r["id"]==item[2] and r["tags"]==mask for r in rows()),"setup tag")
            def named(rs): return {k:next(r for r in rs if r["name"]==v) for k,v in TITLE.items() if any(r["name"]==v for r in rs)}
            def instances(k): return "unique-"+k.lower() if scene=="distinct" or k=="C" else "duplicate"
            state["phase"]="save"
            original={}
            for k,mask in SAVED.items():
                original[k]=managed(k,instances(k),"original-"+k)
                tag(original[k],mask)
            state["saved_live"]=named(rows())
            check({k:r["tags"] for k,r in state["saved_live"].items()}==SAVED,"wrong pre-save tags")
            cmd("save_session")
            snapshot=root/"state/jwm/session.json"
            saved=json.loads(snapshot.read_text())
            (out/"session.json").write_bytes(snapshot.read_bytes())
            saved_order=[next(k for k,title in TITLE.items() if title==r["name"]) for r in saved["clients"]]
            check({k:r["tags"] for k,r in zip(saved_order,saved["clients"])}==SAVED,"actual save lacks expected tags")
            check(all(e["class"].lower()=="xmessage" and e["instance"].lower()==instances(k)
                      for k,e in zip(saved_order,saved["clients"])),"actual snapshot did not preserve WM identities")
            state["save_order"]=saved_order
            for item in original.values(): stop_group(item[0])
            wait(lambda:not rows(),"original windows detached")
            state["phase"]="reopen"
            live={}
            for k in (("C","B") if scene=="survivor" else ("C","B","A")):
                live[k]=managed(k,instances(k),"reopened-"+k)
                tag(live[k],CURRENT[k])
            state["live_identity"]={k:{"pid":item[0].pid,"xid":item[1],"ipc_id":item[2],"instance":item[3]} for k,item in live.items()}
            state["before"]=named(rows())
            check({k:r["tags"] for k,r in state["before"].items()}=={k:CURRENT[k] for k in live},"bad before snapshot")
            saved_hash=digest(snapshot)
            state["phase"]="restore"
            cmd("restore_session")
            state["after"]=named(rows())
            check(digest(snapshot)==saved_hash,"restore unexpectedly rewrote snapshot")
            check({k:r["id"] for k,r in state["after"].items()}=={k:item[2] for k,item in live.items()},"restore changed live windows")
            check(all(live_leader(item[0]) for item in live.values()) and live_leader(wm),
                  "owned WM or live fixture client exited")
            verify_tags(role,scene,{k:r["tags"] for k,r in state["before"].items()},
                        {k:r["tags"] for k,r in state["after"].items()},saved_order)
            # Separate focus usability control; not a claim that manual restore preserves focus.
            cmd("view",value=SAVED["C"])
            selected=wait(lambda:any(r["name"]==TITLE["C"] and r["is_focused"] for r in rows()),"visible unique window focused")
            raw_focus=int(run("/usr/bin/xdotool","getwindowfocus","-f").stdout.strip())
            check(raw_focus==live["C"][1] and selected,"post-restore X and IPC focus disagree")
            state["focus_after_view"]={"ipc_id":live["C"][2],"native_xid":raw_focus,"view":SAVED["C"]}
            state["phase"]="complete"
        except ProbeDeadline as exception:
            interruption = exception
            errors.append(traceback.format_exc())
        except Exception:
            errors.append(traceback.format_exc())
        finally:
            for p in reversed(children):
                try: stop_group(p)
                except ProbeDeadline as exception:
                    interruption = exception
                    errors.append(traceback.format_exc())
                    try: stop_group(p)
                    except BaseException: errors.append(traceback.format_exc())
                except Exception: errors.append(traceback.format_exc())
            write(out/"commands.json",commands)
    state.update(status="failed" if errors else "passed",errors=errors)
    write(out/"result.json",state)
    if interruption is not None:
        raise interruption
    return state

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--role",choices=["baseline","candidate"],required=True)
    parser.add_argument("--binary",type=Path,required=True)
    parser.add_argument("--receipt",type=Path,required=True)
    parser.add_argument("--out",type=Path,required=True)
    a=parser.parse_args()
    a.out.mkdir(parents=True,exist_ok=False)
    write(a.out/"result.json",{"status":"failed","phase":"provenance"})
    check(os.geteuid() != 0, "use a dedicated unprivileged runner")
    receipt=json.loads(a.receipt.read_text())
    check(re.fullmatch(r"[0-9a-f]{40}",receipt["source"]), "missing exact source revision")
    check(receipt["build_exit_code"]==0 and receipt["binary_sha256"]==digest(a.binary),"build receipt mismatch")
    if a.role=="baseline": check(receipt["source"]==BASELINE,"not exact baseline")
    else: check(receipt["source"]==COMBINED_SHA and receipt["tree"]==COMBINED_TREE,"not exact reviewed combination tree")
    def interrupted(*_):
        signal.alarm(0)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        raise ProbeDeadline("acceptance interrupted or exceeded job probe limit")
    signal.signal(signal.SIGTERM,interrupted)
    signal.signal(signal.SIGALRM,interrupted)
    signal.alarm(130)
    results=[]
    failure=None
    try:
        for scene in ("duplicate","survivor","distinct"):
            results.append(session(a.binary.resolve(),a.role,scene,a.out/scene))
    except ProbeDeadline as e:
        failure=str(e)
    finally:
        signal.alarm(0)
    passed=len(results)==3 and all(r["status"]=="passed" for r in results)
    write(a.out/"result.json",{"status":"passed" if passed else "failed","role":a.role,"build":receipt,
                              "scenes":{r["scene"]:r["status"] for r in results},"scope":"one real X11RB output",
                              "interruption":failure})
    return int(not passed)

if __name__=="__main__":
    import sys
    if len(sys.argv)==3 and sys.argv[1]=="--attest-xid":
        attest_xid(int(sys.argv[2]))
    else:
        try:
            raise SystemExit(main())
        except Exception:
            traceback.print_exc()
            raise SystemExit(1)
