#!/usr/bin/env python3
"""Acceptance must reject stale binaries/source, software GPUs and planned soak."""

import copy
import datetime as dt
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from acceptance_evidence import (
    DAILY_CASES,
    artifact,
    audit,
    daily_gate,
    dependency_hashes,
    release_gate,
)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "src").mkdir()
        (self.root / "src/current.rs").write_text("current code")
        binary = self.root / "jwm"
        binary.write_text("exact executable")
        log = self.root / "runner.log"
        log.write_text("observed output")
        self.record = {
            "format": 2,
            "case": "login-upgrade",
            "status": "pass",
            "level": "physical",
            "provenance": {
                "dependencies": ["src"],
                "dependency_sha256": dependency_hashes(["src"], self.root),
            },
            "binaries": [artifact(binary)],
            "raw_evidence": [artifact(log)],
            "hardware": {
                "gpu_vendor": "nvidia",
                "driver": "test-driver",
                "outputs": [{"name": "DP-1", "mode": "2560x1600@60"}],
            },
        }

    def test_added_source_invalidates_only_owned_dependencies(self):
        (self.root / "README.md").write_text("unrelated documentation")
        self.assertTrue(audit(self.record, self.root)["current"])
        (self.root / "src/new.rs").write_text("new policy changes behavior")
        checked = audit(self.record, self.root)
        self.assertFalse(checked["current"])
        self.assertEqual(checked["historical_status"], "pass")
        self.assertEqual(checked["invalidated_by"], ["source dependency changed: src"])

    def test_binary_replacement_and_missing_log_are_not_current(self):
        (self.root / "jwm").write_text("replaced executable")
        (self.root / "runner.log").unlink()
        checked = audit(self.record, self.root)
        self.assertEqual(len(checked["invalidated_by"]), 2)

    def test_missing_source_and_unknown_format_cannot_claim_current_pass(self):
        self.record["provenance"]["dependencies"].append("missing.rs")
        self.record["provenance"]["dependency_sha256"]["missing.rs"] = None
        self.assertFalse(audit(self.record, self.root)["current"])
        self.record["format"] = 999
        self.assertIn(
            "unsupported or historical evidence format",
            audit(self.record, self.root)["invalidated_by"],
        )

    def test_historical_unknown_source_is_not_promoted(self):
        del self.record["provenance"]
        self.assertFalse(audit(self.record, self.root)["current"])

    def records(self):
        result = []
        for case in (
            "login-upgrade",
            "bad-binary",
            "rollback",
            "crash-recovery",
            "active-exit",
            "daily-soak",
        ):
            record = copy.deepcopy(self.record)
            record["case"] = case
            if case == "daily-soak":
                end = dt.datetime.now(dt.timezone.utc) - dt.timedelta(minutes=1)
                record["hardware"].update(
                    soak_started_at=(end - dt.timedelta(days=7)).isoformat(),
                    soak_ended_at=end.isoformat(),
                )
            result.append(record)
        other = copy.deepcopy(self.record)
        other["hardware"]["gpu_vendor"] = "intel"
        result.append(other)
        return result

    def test_release_requires_completed_soak_and_two_physical_vendors(self):
        records = self.records()
        self.assertTrue(release_gate(records, self.root)["release_ready"])
        records[-1]["level"] = "isolated"
        self.assertFalse(release_gate(records, self.root)["release_ready"])
        records = self.records()
        records[-1]["hardware"]["software_renderer"] = True
        self.assertFalse(release_gate(records, self.root)["release_ready"])
        records = self.records()
        future = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=7)
        records[-2]["hardware"]["soak_ended_at"] = future.isoformat()
        self.assertFalse(release_gate(records, self.root)["release_ready"])

    def test_user_waiver_unblocks_only_named_gate_without_hardware_pass(self):
        records = self.records()[:-1]
        report = release_gate(
            records,
            self.root,
            {"second_gpu_vendor": "User has no second device; omit it"},
        )
        self.assertTrue(report["candidate_ready"])
        self.assertFalse(report["hardware_certified"])
        self.assertFalse(report["release_ready"])
        # A named device waiver cannot hide a missing physical lifecycle case.
        records[0]["status"] = "skip"
        self.assertFalse(
            release_gate(
                records, self.root, {"second_gpu_vendor": "omit external device"}
            )["candidate_ready"]
        )


    def daily_records(self):
        for manifest in ("Cargo.toml", "Cargo.lock"):
            (self.root / manifest).write_text("fixture")
        config = self.root / "jwm.toml"
        config.write_text("tested config")
        dependencies = ["src", "Cargo.toml", "Cargo.lock"]
        records = []
        for case, level, backend, outputs, _ in DAILY_CASES:
            record = copy.deepcopy(self.record)
            record.update(case=case, level=level, backend=backend, configs=[artifact(config)])
            record["provenance"].update(
                dependencies=dependencies.copy(),
                dependency_sha256=dependency_hashes(dependencies, self.root),
            )
            record["hardware"]["software_renderer"] = False
            if outputs == 2:
                record["hardware"]["outputs"].append({"name": "HDMI-A-1", "mode": "1920x1080@60"})
            records.append(record)
        return records

    def test_daily_gate_does_not_promote_isolated_or_wrong_backend(self):
        records = self.daily_records()
        software = [record for record in records if record["level"] == "isolated"]
        report = daily_gate(software, self.root)
        self.assertTrue(report["isolated_ready"])
        self.assertFalse(report["physical_ready"])
        self.assertFalse(report["daily_ready"])
        self.assertEqual(len([r for r in report["cases"] if r["state"] == "missing"]), 7)

        states = {r["case"]: r["state"] for r in report["cases"]}
        self.assertEqual(states["session-host-isolated"], "passed")
        self.assertEqual(states["portal"], "missing")
        portal = next(record for record in records if record["case"] == "portal")
        portal.update(level="isolated", backend="host")
        report = daily_gate(software + [portal], self.root)
        self.assertEqual(next(r for r in report["cases"] if r["case"] == "portal")["state"], "wrong-scope")
        portal.update(level="physical", backend="x11rb")
        self.assertFalse(daily_gate(records, self.root)["physical_ready"])

    def test_daily_gate_physical_topology_and_level_are_explicit(self):
        records = self.daily_records()
        self.assertTrue(daily_gate(records, self.root)["daily_ready"])
        for case in ("dual-output-hotplug", "dual-output-suspend"):
            record = next(r for r in records if r["case"] == case)
            with self.subTest(case=case):
                original = copy.deepcopy(record["hardware"])
                record["hardware"]["outputs"][1]["name"] = record["hardware"]["outputs"][0]["name"]
                self.assertFalse(daily_gate(records, self.root)["physical_ready"])
                record["hardware"] = copy.deepcopy(original)
                del record["hardware"]["software_renderer"]
                self.assertFalse(daily_gate(records, self.root)["physical_ready"])
                record["hardware"] = original

    def test_daily_gate_does_not_hide_skips_failures_or_stale_passes(self):
        records = self.daily_records()
        hotplug = next(r for r in records if r["case"] == "dual-output-hotplug")
        hotplug.update(status="skip", reason="no external screen")
        report = daily_gate(records, self.root)
        self.assertFalse(report["daily_ready"])
        row = next(r for r in report["cases"] if r["case"] == "dual-output-hotplug")
        self.assertEqual(row["state"], "skipped")
        self.assertEqual(row["attempts"][0]["reasons"], ["no external screen"])
        hotplug.update(status="pass", reason=None)
        fail = dict(hotplug, status="fail", reason="focus not recovered")
        self.assertFalse(daily_gate(records + [fail], self.root)["daily_ready"])
        (self.root / "jwm.toml").write_text("replaced config")
        self.assertFalse(daily_gate(records, self.root)["daily_ready"])

    def test_daily_gate_rejects_incomplete_or_inconsistent_pass_provenance(self):
        records = self.daily_records()
        records[0]["configs"] = []
        self.assertFalse(daily_gate(records, self.root)["isolated_ready"])
        records = self.daily_records()
        records[0]["provenance"]["dependencies"].remove("Cargo.lock")
        del records[0]["provenance"]["dependency_sha256"]["Cargo.lock"]
        self.assertFalse(daily_gate(records, self.root)["isolated_ready"])
        records = self.daily_records()
        records[0]["provenance"]["dependencies"].remove("Cargo.lock")
        self.assertFalse(daily_gate(records, self.root)["isolated_ready"])

    def test_daily_cli_with_no_evidence_is_a_nonzero_checklist(self):
        result = subprocess.run(
            [sys.executable, str(Path(__file__).with_name("acceptance_evidence.py")), "daily"],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertFalse(report["daily_ready"])
        self.assertEqual({r["state"] for r in report["cases"]}, {"missing"})
        self.assertTrue(all(r["checklist"] for r in report["cases"]))

if __name__ == "__main__":
    unittest.main()
