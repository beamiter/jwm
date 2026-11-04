#!/usr/bin/env python3
"""Acceptance must reject stale binaries/source, software GPUs and planned soak."""

import copy
import datetime as dt
from pathlib import Path
import tempfile
import unittest

from acceptance_evidence import artifact, audit, dependency_hashes, release_gate


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


if __name__ == "__main__":
    unittest.main()
