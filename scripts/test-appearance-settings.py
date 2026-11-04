#!/usr/bin/env python3
"""Real appearance preview/save/cancel on an isolated X11 desktop.
No existing desktop, user's config, credentials, or model is used.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location("private_desktop",ROOT/"eva/tests/desktop-ipc.py")
fixture=importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def wait(call,label,timeout=15):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        try:
            value=call()
            if value:return value
        except (OSError,ValueError,RuntimeError):pass
        time.sleep(.05)
    raise RuntimeError(label+" timed out")


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jwm",type=Path,required=True)
    parser.add_argument("--evidence-dir",type=Path,required=True)
    parser.add_argument("--probe-initial",action="store_true",help="Capture idle initial modal at 0.1/0.6/1.5 seconds with software frame counters")
    parser.add_argument("--probe-entry",choices=("rpc","hotkey"),default="rpc")
    parser.add_argument("--startup-settle-ms",type=int,default=0)
    parser.add_argument("--probe-capture-first",action="store_true")
    parser.add_argument("--probe-target-seconds",type=float)
    parser.add_argument("--trace-render",action="store_true")
    parser.add_argument("--probe-warm-reopen",action="store_true")
    parser.add_argument("--font-dir",type=Path,action="append",default=[])
    args=parser.parse_args()
    binary=args.jwm.resolve(strict=True);out=args.evidence_dir.resolve();out.mkdir(parents=True,exist_ok=False)
    private=Path(tempfile.mkdtemp(prefix="jwm-appearance-"));children=fixture.Children()
    report={"status":"failed","scope":"isolated_x11_real_keyboard_ui","binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"startup_settle_ms":args.startup_settle_ms,"steps":[]}
    env=os.environ.copy()
    for key in list(env):
        if key.startswith(("JWM_","EVA_","OPENAI_","ANTHROPIC_","JTERM_","PULSE_","PIPEWIRE_")) or key in ("DISPLAY","WAYLAND_DISPLAY","WAYLAND_SOCKET","XAUTHORITY","DBUS_SESSION_BUS_ADDRESS"):env.pop(key,None)
    for key,leaf in (("HOME","home"),("XDG_CONFIG_HOME","config"),("XDG_STATE_HOME","state"),("XDG_DATA_HOME","data"),("XDG_CACHE_HOME","cache"),("XDG_RUNTIME_DIR","runtime")):
        path=private/leaf;path.mkdir(mode=0o700);env[key]=str(path)
    env.update(LIBGL_ALWAYS_SOFTWARE="1",WINIT_UNIX_BACKEND="x11",JWM_COMPOSITOR_API="glx",RUST_LOG="warn",GTK_A11Y="none",GTK_USE_PORTAL="0")
    if args.trace_render:env["RUST_LOG"]="warn,ui_frame_trace=debug"
    if args.font_dir:
        from xml.sax.saxutils import escape
        fontconfig=private/"fontconfig.xml"
        fontconfig.write_text('<fontconfig><include>/etc/fonts/fonts.conf</include>'+''.join('<dir>'+escape(str(path.resolve(strict=True)))+'</dir>'for path in args.font_dir)+'<cachedir>'+str(private/'cache/fontconfig')+'</cachedir></fontconfig>')
        env["FONTCONFIG_FILE"]=str(fontconfig)
        report["explicit_fixture_font_dirs"]=[str(path.resolve())for path in args.font_dir]
    (private/"config/jwm").mkdir(mode=0o700)
    subprocess.run([str(binary),"--backend","x11rb","--gen-config"],env=env,check=True,capture_output=True,timeout=30)
    config=next((private/"config/jwm").glob("config_*.toml"))
    text=fixture.edit_setting(config.read_text(),"status_bar","show_bar","false")
    text=fixture.edit_setting(text,"desktop_shell","enabled","false",required=False)
    text=fixture.edit_setting(text,"behavior","compositor","true")
    config.write_text("# retained personal configuration note\n"+text)
    config.chmod(0o600)
    path=private/"runtime/jwm-ipc.sock"
    def query(name):
        data=fixture.query(path,name)
        if name in ("get_system_ui","get_ui_theme"):(out/("last-"+name+".json")).write_text(json.dumps(data,indent=2))
        return data
    def key(*keys):subprocess.run(["xdotool","key","--clearmodifiers",*keys],env=env,check=True,timeout=5);time.sleep(.08)
    def choose(index):
        key("Home")
        if index:subprocess.run(["xdotool","key","--clearmodifiers","--repeat",str(index),"--delay","30","Down"],env=env,check=True,timeout=5)
        key("Return")
        choices=["material","glass","glass-dark","aurora","nord","tokyo-night","paper","@density:compact","@density:comfortable","@density:spacious","@font:11","@font:14","@font:18","@surface:0.85","@surface:0.95","@surface:1","@motion:reduced","@motion:normal","@effects:low","@effects:balanced","@effects:rich","@save","@keep","@cancel"]
        if index not in (21,22,23):
            wait(lambda:query("get_ui_theme")["appearance_selection"]==choices[index],"stable appearance selection after preview")
    def open_theme():
        key("alt+F10");wait(lambda:query("get_system_ui")["kind"]=="control_center","Hub")
        key("t");wait(lambda:query("get_system_ui")["kind"]=="theme_picker","Theme settings")
    def capture(name):
        time.sleep(.6)  # Capture the settled card/selection, not an opening animation frame.
        subprocess.run(["import","-window","root",str(out/(name+".png"))],env=env,check=True,capture_output=True,timeout=5)
    try:
        bus=private/"bus.conf";bus.write_text('<busconfig><type>session</type><listen>unix:tmpdir='+env["XDG_RUNTIME_DIR"]+'</listen><policy context="default"><allow own="*"/><allow send_destination="*"/><allow receive_sender="*"/></policy></busconfig>')
        bus_log=out/"bus.log";children.spawn(["dbus-daemon","--nofork","--print-address=1","--config-file="+str(bus)],env,bus_log)
        env["DBUS_SESSION_BUS_ADDRESS"]=wait(lambda:bus_log.read_text().splitlines()[0] if bus_log.exists() and bus_log.read_text().strip() else None,"private bus")
        env["DISPLAY"]=children.display("Xvfb",env,out);env["DISPLAY"]=children.display("Xephyr",env,out)
        env["JWM_SOCKET"]=str(path);wm=children.spawn([str(binary),"--backend","x11rb"],env,out/"jwm.log");report["wm_pid"]=wm.pid
        wait(lambda:query("get_execution_context").get("monitors"),"private JWM")
        client=children.spawn(["xmessage","-title","Appearance fixture","-buttons","OK:0","Private appearance settings test"],env,out/"client.log")
        def ready_window():
            result=subprocess.run(["xdotool","search","--onlyvisible","--name","^Appearance fixture$"],env=env,capture_output=True,text=True,timeout=3)
            return result.stdout.splitlines()[0] if result.returncode==0 and result.stdout.strip() else None
        window=wait(ready_window,"native focus fixture")
        subprocess.run(["xdotool","windowactivate","--sync",window],env=env,check=True,timeout=5)
        wait(lambda:query("get_execution_context")["focus_token"],"native input focus")
        time.sleep(max(0,min(args.startup_settle_ms,5000))/1000)
        baseline=query("get_config");disk=config.read_text();original_tokens=query("get_ui_theme")["tokens"]
        if args.probe_initial:
            before=query("get_metrics")
            if args.probe_entry=="hotkey":open_theme()
            else:
                fixture.command(path,"control_center",{})
                wait(lambda:query("get_system_ui")["kind"]=="control_center","RPC Hub")
                key("t");wait(lambda:query("get_system_ui")["kind"]=="theme_picker","Theme after real modal key")
            report["entry_source"]=args.probe_entry
            report["startup_settle_ms"]=args.startup_settle_ms
            start=time.monotonic();timeline=[]
            targets=(max(0,min(args.probe_target_seconds,5)),) if args.probe_target_seconds is not None else (.1,.6,1.5)
            for target in targets:
                time.sleep(max(0,target-(time.monotonic()-start)))
                if args.probe_capture_first:
                    subprocess.run(["import","-window","root",str(out/("initial-"+str(target)+".png"))],env=env,check=True,capture_output=True,timeout=5)
                sample={"target_seconds":target,"elapsed_seconds":time.monotonic()-start,"capture_before_queries":args.probe_capture_first,"system_ui":query("get_system_ui"),"software_metrics":query("get_metrics"),"theme":query("get_ui_theme")}
                if not args.probe_capture_first:
                    subprocess.run(["import","-window","root",str(out/("initial-"+str(target)+".png"))],env=env,check=True,capture_output=True,timeout=5)
                timeline.append(sample)
            report.update(status="diagnostic_observed",before_metrics=before,timeline=timeline,measurement_kind="software_frame_counter_and_private_X11_capture_not_physical_latency")
            if args.probe_warm_reopen:
                time.sleep(1.5)
                key("Escape");wait(lambda:query("get_system_ui")["kind"]=="control_center","warm return Hub")
                key("Escape");wait(lambda:not query("get_system_ui")["active"],"warm close")
                report["warm_before_metrics"]=query("get_metrics")
                fixture.command(path,"control_center",{});wait(lambda:query("get_system_ui")["kind"]=="control_center","warm RPC Hub")
                key("t");wait(lambda:query("get_system_ui")["kind"]=="theme_picker","warm Theme")
                start_warm=time.monotonic();time.sleep(.6)
                subprocess.run(["import","-window","root",str(out/"warm-reopen-0.6.png")],env=env,check=True,capture_output=True,timeout=5)
                report["warm_after_metrics"]=query("get_metrics");report["warm_elapsed_seconds"]=time.monotonic()-start_warm
            return
        open_theme();capture("saved-global-defaults");choose(7);wait(lambda:query("get_config")["gap_px"]==4,"density preview")
        choose(12);wait(lambda:query("get_config")["system_ui_font"]=="Sans 18","font preview")
        choose(13);theme=wait(lambda:(j if (j:=query("get_ui_theme"))["surface_opacity"]<.9 else None),"surface preview")
        assert theme["scope"]=="preview" and theme["tokens"]["text"]==original_tokens["text"]
        assert config.read_text()==disk,"preview wrote the config file"
        capture("large-text-preview")
        key("Escape");wait(lambda:query("get_config")["system_ui_font"]==baseline["system_ui_font"],"cancel font rollback")
        assert query("get_config")["gap_px"]==baseline["gap_px"] and config.read_text()==disk
        key("Escape");report["steps"].append("multiple preset preview and Escape restore owned values without writing")
        open_theme();choose(9);choose(12);choose(16);choose(21)
        wait(lambda:not query("get_system_ui")["active"],"explicit Save")
        saved=query("get_config");assert saved["gap_px"]==12 and saved["status_bar_height"]==44 and saved["system_ui_font"]=="Sans 18" and not saved["animation_enabled"]
        assert abs(saved["ui_density"]-1.2)<1e-6
        written=config.read_text();assert "# retained personal configuration note" in written and re.search(r'system_ui_font\s*=\s*"Sans 18"',written)
        report["steps"].append("explicit Save persists density, font and motion atomically and retains comments")
        open_theme();choose(11)
        fixture.command(path,"set_config",{"key":"appearance.gap_px","value":31})
        key("Escape");wait(lambda:query("get_config")["gap_px"]==31,"newer revision retained")
        assert config.read_text()==written
        report["steps"].append("cancelling an invalidated preview preserves a newer IPC configuration revision")
        report["status"]="passed";report["saved_config"]={key:saved[key] for key in ("gap_px","status_bar_height","system_ui_font","animation_enabled","ui_surface_opacity","ui_density")}
    except Exception as error:
        report["error"]=str(error)
        try:
            report["ui_at_failure"]=query("get_system_ui")
            if not report["ui_at_failure"]["active"]:
                report["diagnostic_hub_command"]=fixture.command(path,"control_center",{},expect=False)
                report["ui_after_diagnostic_command"]=query("get_system_ui")
        except Exception as diagnostic:report["diagnostic_error"]=str(diagnostic)
        try:capture("failure")
        except Exception:pass
        raise
    finally:
        children.close();(out/"result.json").write_text(json.dumps(report,indent=2)+"\n");shutil.rmtree(private)


if __name__=="__main__":main()
