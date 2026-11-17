#!/usr/bin/env python3
"""Exercise operator countdown/ownership through a fake CLI and a real privatePTY."""

import json
import os
from pathlib import Path
import pty
import runpy
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/jwm-display-preview"
FAKE = r"""#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
root=Path(os.environ['FAKE_DISPLAY_ROOT']); state=root/'state.json'; calls=root/'calls.jsonl'
# CLI calls are serial; observers may read counters concurrently. Publish only
# complete decimal values, never the truncate/write window of write_text().
def increment(path):
 n=int(path.read_text())+1 if path.exists() else 1
 pending=path.with_name(path.name+'.pending')
 pending.write_text(str(n))
 os.replace(pending,path)
 return n
args=sys.argv[1:]
with calls.open('a') as f:f.write(json.dumps(args)+'\n')
heads=json.loads(state.read_text())
scenario=os.environ.get('FAKE_SCENARIO','')
if args==['--json']:
 n=increment(root/'reads')
 if scenario in ('concurrent','disconnect','replaced') and n==4:
  if scenario=='disconnect':heads=heads[:1]
  elif scenario=='replaced':heads[0]['serial']='new-device'
  else:heads[0]['position']['x']=555
  state.write_text(json.dumps(heads))
 print(json.dumps(heads));sys.exit(0)
if '--dryrun' in args:sys.exit(0)
n=increment(root/'mutations')
if scenario=='apply-fail' and n==1:sys.exit(1)
if scenario=='restore-fail' and n==2:sys.exit(1)
head=None;i=0
while i<len(args):
 flag=args[i];i+=1
 if flag=='--output':name=args[i];i+=1;head=next(v for v in heads if v['name']==name)
 elif flag=='--off':head['enabled']=False
 elif flag=='--on':head['enabled']=True
 elif flag=='--mode':
  value=args[i];i+=1;shape,rate=value.rstrip('Hz').split('@');w,h=map(int,shape.split('x'));rate=float(rate)
  for mode in head['modes']:mode['current']=(mode['width'],mode['height'],mode['refresh'])==(w,h,rate)
 elif flag=='--pos':x,y=map(int,args[i].split(','));i+=1;head['position']={'x':x,'y':y}
 elif flag=='--scale':head['scale']=round(float(args[i])*256)/256;i+=1
 elif flag=='--transform':head['transform']=args[i];i+=1
 elif flag=='--adaptive-sync':head['adaptive_sync']=args[i]=='enabled';i+=1
 else:raise AssertionError(flag)
state.write_text(json.dumps(heads))
"""


def original():
    return [
        {
            "name": name,
            "description": "test head",
            "make": "test",
            "model": "model",
            "serial": str(i),
            "physical_size": {"width": 600, "height": 340},
            "enabled": True,
            "modes": [
                {
                    "width": 1920,
                    "height": 1080,
                    "refresh": 59.94,
                    "current": True,
                    "preferred": True,
                },
                {
                    "width": 1280,
                    "height": 720,
                    "refresh": 60.0,
                    "current": False,
                    "preferred": False,
                },
            ],
            "position": {"x": i * 1920, "y": 0},
            "scale": 1.0,
            "transform": "normal",
            "adaptive_sync": False,
        }
        for i, name in enumerate(("DP-1", "DP-2"))
    ]


class PreviewTests(unittest.TestCase):
    def run_preview(self, scenario="", keypress=None, interrupt=False):
        with tempfile.TemporaryDirectory(prefix="jwm-output-preview-") as directory:
            root = Path(directory)
            fake = root / "wlr-randr"
            fake.write_text(FAKE)
            fake.chmod(0o755)
            state = root / "state.json"
            state.write_text(json.dumps(original()))
            report = root / "report.json"
            master, slave = pty.openpty()
            env = dict(os.environ, FAKE_DISPLAY_ROOT=str(root), FAKE_SCENARIO=scenario)
            child = subprocess.Popen(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--wlr-randr",
                    str(fake),
                    "--timeout",
                    ".5",
                    "--report",
                    str(report),
                    "--",
                    "--output",
                    "DP-1",
                    "--mode",
                    "1280x720@60Hz",
                    "--pos",
                    "10,20",
                ],
                env=env,
                stdin=slave,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            os.close(slave)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if (root / "mutations").exists() and int(
                    (root / "mutations").read_text()
                ) >= 1:
                    break
                if child.poll() is not None:
                    break
                time.sleep(0.01)
            if interrupt:
                # Wait for the post-apply snapshot to establish ownership.
                while time.monotonic() < deadline:
                    if (root / "reads").exists() and int(
                        (root / "reads").read_text()
                    ) >= 4:
                        break
                    time.sleep(0.01)
                child.send_signal(signal.SIGTERM)
            if keypress is not None:
                os.write(master, keypress)
            stdout, stderr = child.communicate(timeout=8)
            os.close(master)
            self.assertTrue(report.exists(), (stdout, stderr))
            result = json.loads(report.read_text())
            calls = [
                json.loads(v) for v in (root / "calls.jsonl").read_text().splitlines()
            ]
            return child.returncode, result, json.loads(state.read_text()), calls

    def test_fake_counters_publish_complete_values_atomically(self):
        # Pause the real fake CLI immediately before rename. A concurrent
        # observer must see either the previous complete value or no file on
        # first publication, never an empty/partially written public counter.
        publish = " os.replace(pending,path)"
        self.assertEqual(FAKE.count(publish), 1)
        paused = FAKE.replace(
            publish,
            """ import time
 (root/'ready').write_text('ready')
 deadline=time.monotonic()+5
 while not (root/'release').exists():
  if time.monotonic()>=deadline:raise RuntimeError('publication test timed out')
  time.sleep(0.001)
 os.replace(pending,path)""",
        )
        for counter, args in (
            ("reads", ["--json"]),
            ("mutations", ["--output", "DP-1", "--off"]),
        ):
            for previous in (None, 3):
                with self.subTest(counter=counter, previous=previous):
                    with tempfile.TemporaryDirectory(prefix="jwm-counter-") as directory:
                        root = Path(directory)
                        fake = root / "fake.py"
                        fake.write_text(paused)
                        (root / "state.json").write_text(json.dumps(original()))
                        path = root / counter
                        if previous is not None:
                            path.write_text(str(previous))
                        child = subprocess.Popen(
                            [sys.executable, str(fake), *args],
                            env=dict(
                                os.environ,
                                FAKE_DISPLAY_ROOT=str(root),
                                FAKE_SCENARIO="",
                            ),
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            text=True,
                        )
                        try:
                            deadline = time.monotonic() + 5
                            while not (root / "ready").exists():
                                self.assertIsNone(
                                    child.poll(), "fake exited before publication"
                                )
                                self.assertLess(time.monotonic(), deadline)
                                time.sleep(0.001)
                            self.assertEqual(path.exists(), previous is not None)
                            if previous is not None:
                                self.assertEqual(path.read_text(), str(previous))
                            self.assertEqual(
                                (root / (counter + ".pending")).read_text(),
                                str(1 if previous is None else previous + 1),
                            )
                        finally:
                            (root / "release").write_text("release")
                            try:
                                stdout, stderr = child.communicate(timeout=8)
                            except subprocess.TimeoutExpired:
                                child.kill()
                                child.communicate()
                                raise
                        self.assertEqual(child.returncode, 0, (stdout, stderr))
                        self.assertEqual(
                            int(path.read_text()),
                            1 if previous is None else previous + 1,
                        )
                        self.assertFalse((root / (counter + ".pending")).exists())

    def test_timeout_restores_exact_mode_rate_geometry_and_only_owned_outputs(self):
        code, report, state, calls = self.run_preview()
        self.assertEqual(code, 2, report)
        self.assertEqual(report["status"], "restored")
        self.assertEqual(state, original())
        self.assertIn("1920x1080@59.940000Hz", calls[-2])
        self.assertNotIn("DP-2", calls[-2])

    def test_enter_confirms_and_escape_reverts(self):
        code, report, state, _ = self.run_preview(keypress=b"\n")
        self.assertEqual((code, report["status"]), (0, "confirmed"))
        self.assertEqual(state[0]["position"], {"x": 10, "y": 20})
        code, report, state, _ = self.run_preview(keypress=b"\x1b")
        self.assertEqual((code, report["status"]), (2, "restored"))
        self.assertEqual(state, original())

    def test_concurrent_settings_disconnect_and_replaced_identity_stop_writes(self):
        for scenario in ("concurrent", "disconnect", "replaced"):
            with self.subTest(scenario=scenario):
                code, report, state, calls = self.run_preview(scenario)
                self.assertEqual((code, report["status"]), (3, "ownership_lost"))
                self.assertEqual(len([v for v in calls if v[:1] == ["--output"]]), 1)

    def test_failed_apply_and_failed_restore_are_explicit(self):
        code, report, state, _ = self.run_preview("apply-fail")
        self.assertEqual((code, report["status"]), (1, "apply_failed"))
        self.assertEqual(state, original())
        code, report, state, calls = self.run_preview("restore-fail")
        self.assertEqual((code, report["status"]), (4, "restore_failed"))
        self.assertEqual(
            len([v for v in calls if v[:1] == ["--output"]]),
            2,
            "a failed rollback must not be silently retried",
        )

    def test_signal_rolls_back_verified_owned_state(self):
        code, report, state, _ = self.run_preview(interrupt=True)
        self.assertEqual(code, 128 + signal.SIGTERM)
        self.assertEqual(report["status"], "restored")
        self.assertEqual(state, original())

    def test_json_refresh_is_hz_and_fractional_scale_is_protocol_quantized(self):
        code = runpy.run_path(str(SCRIPT))
        before = code["normalize"](original())
        desired, _ = code["desired_layout"](
            before, ["--output", "DP-1", "--scale", "1.2"]
        )
        self.assertEqual(before["DP-1"]["mode"], (1920, 1080, 59940))
        self.assertEqual(desired["DP-1"]["scale"], round(307 / 256, 6))
        rollback = code["restore_args"](before, {"DP-1"})
        self.assertIn("1920x1080@59.940000Hz", rollback)
        self.assertNotIn("1920x1080@59940.000000Hz", rollback)

    def test_unrestorable_current_mode_fails_before_an_apply(self):
        code = runpy.run_path(str(SCRIPT))
        heads = original()
        heads[0]["modes"][0]["current"] = False
        with self.assertRaises(code["PreviewError"]):
            code["normalize"](heads)

    def test_non_tty_never_executes_cli(self):
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--wlr-randr",
                "/does/not/exist",
                "--",
                "--output",
                "DP-1",
                "--off",
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("interactive TTY", result.stderr)


if __name__ == "__main__":
    unittest.main()
