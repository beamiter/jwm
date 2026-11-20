import json
from pathlib import Path
import subprocess
import sys
import tempfile
from unittest.mock import patch
import unittest
import probe

class Contract(unittest.TestCase):
    def test_duplicate_candidate_and_old_have_opposite_acceptance(self):
        before=dict(probe.CURRENT)
        old={"A":4,"B":2,"C":8}
        new={"A":16,"B":32,"C":8}
        probe.verify_tags("baseline","duplicate",before,old,["A","B","C"])
        probe.verify_tags("candidate","duplicate",before,new,["B","C","A"])
        with self.assertRaises(AssertionError):
            probe.verify_tags("candidate","duplicate",before,old,["A","B","C"])
        with self.assertRaises(AssertionError):
            probe.verify_tags("baseline","duplicate",before,new,["A","B","C"])
    def test_survivor_uses_actual_saved_order_only_for_old(self):
        before={"B":32,"C":64}
        probe.verify_tags("baseline","survivor",before,{"B":4,"C":8},["B","C","A"])
        probe.verify_tags("candidate","survivor",before,{"B":32,"C":8},["B","C","A"])
        for bad in ({"B":2,"C":8},{"B":4,"C":64},{"C":8}):
            with self.assertRaises(AssertionError):
                probe.verify_tags("baseline","survivor",before,bad,["B","C","A"])
    def test_distinct_and_setup_controls(self):
        for role in ["baseline","candidate"]:
            probe.verify_tags(role,"distinct",dict(probe.CURRENT),dict(probe.SAVED),["C","B","A"])
        for before,after,order in [
            (probe.SAVED,probe.SAVED,["A","B","C"]),
            (probe.CURRENT,probe.SAVED,["A","A","C"]),
            (probe.CURRENT,probe.CURRENT,["A","B","C"]),
        ]:
            with self.assertRaises(AssertionError):
                probe.verify_tags("candidate","distinct",before,after,order)

    def test_dead_leader_does_not_leave_descendant(self):
        # The leader exits first, leaving one child in the owned session.
        p=subprocess.Popen([sys.executable,"-c", "import os,time; p=os.fork(); time.sleep(30) if p==0 else os._exit(0)"],
                           start_new_session=True)
        try:
            probe.wait(lambda:not probe.live_leader(p),"owned leader exit")
            self.assertTrue(probe.group_live(p.pid))
            probe.stop_group(p)
            self.assertFalse(probe.group_live(p.pid))
            self.assertEqual(p.returncode,0)
        finally:
            probe.stop_group(p)

    def test_interruption_never_proceeds_to_next_scene(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); rec=root/"receipt"; binary=root/"jwm"; binary.write_bytes(b"synthetic")
            rec.write_text(json.dumps({"source":probe.BASELINE,"build_exit_code":0,"binary_sha256":probe.digest(binary)}))
            out=root/"evidence"
            argv=["probe","--role","baseline","--binary",str(binary),"--receipt",str(rec),"--out",str(out)]
            with patch.object(sys,"argv",argv),patch.object(probe.os,"geteuid",return_value=1001), \
                 patch.object(probe.signal,"signal"),patch.object(probe.signal,"alarm"), \
                 patch.object(probe,"session",side_effect=probe.ProbeDeadline("test interruption")) as run:
                self.assertEqual(probe.main(),1)
            self.assertEqual(run.call_count,1)
            self.assertEqual(json.loads((out/"result.json").read_text())["interruption"],"test interruption")
