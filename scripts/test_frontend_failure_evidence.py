import tempfile
import struct
import unittest
from pathlib import Path
from unittest.mock import patch
from types import SimpleNamespace
import subprocess
import frontend_failure_evidence as evidence


class EvidenceTests(unittest.TestCase):
    def test_titles_excluded_and_window_count_bounded(self):
        calls = []
        def run(args, **kwargs):
            calls.append((args, kwargs))
            if args[0] == "xwininfo":
                kwargs["stdout"].write(b'Window id: 0x123 "private title"\n Width: 1270\n Height: 42\n Map State: IsViewable\n Visibility State: VisibilityUnobscured\n')
            else:
                Path(args[-1]).write_bytes(struct.pack(">25I", 100, 7, 2, 32, 1280, 800, *([0] * 19)))
            return SimpleNamespace()
        with tempfile.TemporaryDirectory() as directory, patch.object(evidence.subprocess, "run", run):
            result = evidence.collect(["bad;command"] + list(range(1, 30)), directory, {"DISPLAY": ":99"})
        self.assertEqual(len(result["windows"]), 4)
        self.assertEqual(len(calls), 9)
        self.assertNotIn("private title", str(result))
        self.assertTrue(all(call[1]["timeout"] <= 1 for call in calls))
        self.assertTrue(all(call[0][0] in ("xwininfo", "xwd") for call in calls))
        self.assertEqual(result["windows"]["1"]["stats"]["Map State"], "IsViewable")
        self.assertEqual(result["capture_format"], "xwd")
        self.assertTrue(result["private_display_capture"].endswith(".xwd"))

    def test_command_failures_do_not_replace_original_readiness_failure(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(evidence.subprocess, "run", side_effect=subprocess.TimeoutExpired("xwininfo", 1)):
            result = evidence.collect(["12"], directory, {})
        self.assertIsNone(result["windows"]["12"]["stats"])
        self.assertTrue(result["capture_failed"])

    def test_partial_dump_is_not_reported_as_capture(self):
        def run(args, **kwargs):
            Path(args[-1]).write_bytes(b"not an XWD")
        with tempfile.TemporaryDirectory() as directory, patch.object(evidence.subprocess, "run", run):
            result = evidence.collect([], directory, {})
        self.assertIsNone(result["private_display_capture"])
        self.assertEqual(result["capture_failure"], {"kind": "invalid-dump"})

    def test_capture_failure_classification_contains_no_exception_text(self):
        cases = [
            (FileNotFoundError("private path"), {"kind": "missing-tool"}),
            (subprocess.TimeoutExpired("private command", 1), {"kind": "timeout"}),
            (subprocess.CalledProcessError(7, "private command"), {"kind": "exit-status", "code": 7}),
            (PermissionError(13, "private path"), {"kind": "os-error", "errno": 13}),
        ]
        for failure, expected in cases:
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as directory:
                with patch.object(evidence.subprocess, "run", side_effect=failure):
                    result = evidence.collect([], directory, {})
            self.assertEqual(result["capture_failure"], expected)
            self.assertNotIn("private", str(result["capture_failure"]))

    def test_expired_budget_skips_commands_and_capture(self):
        with patch.object(evidence.time, "monotonic", side_effect=[0, 9, 10, 11]), patch.object(evidence.subprocess, "run") as run:
            result = evidence.collect(["12"], "/tmp", {})
        run.assert_not_called()
        self.assertIsNone(result["private_display_capture"])

    def test_text_and_tree_are_capped(self):
        def run(args, **kwargs):
            if args[0] == "xwininfo":
                kwargs["stdout"].write(b"0x123 " * 10000)
        with tempfile.TemporaryDirectory() as directory, patch.object(evidence.subprocess, "run", run):
            result = evidence.collect(["0x123"], directory, {})
        self.assertEqual(len(result["windows"]["0x123"]["tree_xids"]), 32)


class ChildEvidenceTests(unittest.TestCase):
    def test_tree_root_parent_and_titles_are_not_child_ids(self):
        tree = 'Root window id: 0x21f "private root"\n Parent window id: 0x21f\n 1 child:\n  0xe00005 "private child": () 1270x42+0+0\n'
        self.assertEqual(evidence._direct_children(tree), ['0xe00005'])

    def test_child_parser_caps_and_deduplicates(self):
        tree = '20 children:\n' + ''.join('  0x%x "private": ()\n' % i for i in range(1, 21))
        self.assertEqual(evidence._direct_children(tree), ['0x1', '0x2', '0x3', '0x4'])
        self.assertEqual(evidence._direct_children('0 children.\n'), [])

    def test_collects_child_stats_passively_with_existing_budget(self):
        calls = []
        def run(args, **kwargs):
            calls.append(args)
            if args[-1] == '-tree':
                kwargs['stdout'].write(b' 1 child:\n  0xe00005 "private": () 1270x42+0+0\n')
            elif args[0] == 'xwininfo':
                kwargs['stdout'].write(b' Width: 1270\n Height: 42\n Map State: IsUnMapped\n')
            else:
                Path(args[-1]).write_bytes(struct.pack('>25I', 100, 7, 2, 32, 1280, 800, *([0] * 19)))
        with tempfile.TemporaryDirectory() as directory, patch.object(evidence.subprocess, 'run', run):
            result = evidence.collect(['12'], directory, {})
        self.assertEqual(result['child_windows']['0xe00005']['Map State'], 'IsUnMapped')
        self.assertEqual(len(calls), 4)
        self.assertEqual(calls[2], ['xwininfo', '-id', '0xe00005', '-stats'])
        self.assertEqual(result['capture_format'], 'xwd')
        self.assertNotIn('private child', str(result))

    def test_reserves_root_capture_budget_before_children(self):
        def run(args, **kwargs):
            if args[-1] == '-tree':
                kwargs['stdout'].write(b' 1 child:\n  0xe00005 "private": ()\n')
            elif args[0] == 'xwd':
                Path(args[-1]).write_bytes(struct.pack('>25I', 100, 7, 2, 32, 1280, 800, *([0] * 19)))
        with tempfile.TemporaryDirectory() as directory, patch.object(evidence.subprocess, 'run', run), patch.object(evidence.time, 'monotonic', side_effect=[0, 0, 0, 7.5, 7.5]):
            result = evidence.collect(['12'], directory, {})
        self.assertEqual(result['child_windows'], {})
        self.assertEqual(result['capture_format'], 'xwd')


if __name__ == "__main__":
    unittest.main()
