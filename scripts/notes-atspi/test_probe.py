import unittest
from types import SimpleNamespace
import probe

Role = SimpleNamespace(ENTRY=1, TEXT=2, PUSH_BUTTON=3, LIST_ITEM=4, DOCUMENT_TEXT=5)


class Node:
    def __init__(self, name="", role=0, pid=0, children=(), action=None, focused=False):
        self.name, self.role, self.pid = name, role, pid
        self.children, self.action, self.focused = list(children), action, focused

    def clear_cache(self): pass
    def get_child_count(self): return len(self.children)
    def get_child_at_index(self, i): return self.children[i]
    def get_role(self): return self.role
    def get_name(self): return self.name
    def get_process_id(self): return self.pid
    def get_action_iface(self): return self if self.action else None
    def get_n_actions(self): return 2
    def get_action_name(self, i): return ("menu", "click")[i]
    def do_action(self, i): return i == 1 and self.action()
    def get_state_set(self): return self
    def contains(self, _): return self.focused


class Receipt:
    value = None

    def exists(self): return self.value is not None
    def read_text(self): return self.value

    def write_text(self, value):
        self.value = value
        return len(value)


class TestProbe(unittest.TestCase):
    def setUp(self):
        self.receipt = Receipt()
        self.title = probe.INITIAL
        self.button = Node(probe.CONTROL, Role.PUSH_BUTTON,
                           action=lambda: self.receipt.write_text("activated\n") > 0)
        self.target = Node(probe.TARGET, Role.LIST_ITEM, action=self.select_target)
        self.editor = Node(probe.EDITOR, Role.DOCUMENT_TEXT, focused=True)
        self.search = Node(probe.SEARCH, Role.ENTRY)
        self.gtk = Node(pid=101, children=[self.button])
        self.notes = Node(pid=102, children=[self.target, self.editor, self.search])
        self.desk = Node(children=[self.gtk, self.notes])
        self.atspi = SimpleNamespace(Role=Role, StateType=SimpleNamespace(FOCUSED=1),
                                     get_desktop=lambda _: self.desk)

    def select_target(self):
        self.title = probe.TARGET
        return True

    def run_probe(self):
        return probe.evaluate(self.atspi, 101, 102, self.receipt,
                              lambda title: self.title == title, 0)

    def test_working(self):
        self.assertEqual(self.run_probe(), "notes-named-controls-action-and-focus-observed")

    def test_unregistered(self):
        self.desk.children.pop()
        self.assertEqual(self.run_probe(), "notes-no-atspi-application-for-pid")

    def test_missing_controls(self):
        self.notes.children.remove(self.search)
        self.assertEqual(self.run_probe(), "notes-atspi-controls-missing")

    def test_action_unavailable(self):
        self.target.action = None
        self.assertEqual(self.run_probe(), "notes-result-not-actionable")

    def test_lost_focus(self):
        self.editor.focused = False
        self.assertEqual(self.run_probe(), "notes-result-selection-or-focus-not-observed")

    def test_title_must_change(self):
        self.target.action = lambda: True
        self.assertEqual(self.run_probe(), "notes-result-selection-or-focus-not-observed")

    def test_gtk_callback_must_run(self):
        self.button.action = lambda: True
        with self.assertRaisesRegex(probe.ProbeError, "gtk-positive-control-action-not-observed"):
            self.run_probe()

    def test_wrong_pid_does_not_satisfy_control(self):
        self.gtk.pid += 900
        with self.assertRaisesRegex(probe.ProbeError, "gtk-positive-control-not-found"):
            self.run_probe()

    def test_gtk_control_can_be_present_but_not_actionable(self):
        self.button.action = None
        with self.assertRaisesRegex(probe.ProbeError, "gtk-positive-control-not-actionable"):
            self.run_probe()

    def test_transport_failure_diagnostics_omit_exception_text(self):
        try:
            next(probe.children(SimpleNamespace()))
        except AttributeError as exc:
            payload = probe.failure_payload(exc)
        else:
            self.fail("expected the incomplete transport object to fail")
        self.assertEqual(payload["outcome"], "probe-transport-or-fixture-error")
        self.assertEqual(payload["error_type"], "AttributeError")
        self.assertEqual(len(payload["probe_stack"]), 1)
        self.assertRegex(payload["probe_stack"][0], r"^children:[0-9]+$")
        self.assertNotIn("SimpleNamespace", repr(payload))
        os_failure = probe.failure_payload(OSError(5, "private-path"))
        self.assertEqual(os_failure["error_code"], 5)
        self.assertNotIn("private-path", repr(os_failure))

    def test_stale_receipt(self):
        self.receipt.write_text("activated\n")
        with self.assertRaisesRegex(probe.ProbeError, "stale-positive-control-receipt"):
            self.run_probe()

    def test_notes_window_required(self):
        self.title = None
        with self.assertRaisesRegex(probe.ProbeError, "initial-notes-window-not-ready"):
            self.run_probe()

    def test_desktop_required(self):
        self.desk = None
        with self.assertRaisesRegex(probe.ProbeError, "desktop-unavailable"):
            self.run_probe()

    def test_ambiguous_result_is_not_activated(self):
        self.notes.children.append(self.target)
        with self.assertRaisesRegex(probe.ProbeError, "ambiguous-control"):
            self.run_probe()
        self.assertEqual(self.title, probe.INITIAL)

    def test_conflict_snapshot_retries_only_retired_atspi_object(self):
        class TransportError(Exception):
            def __init__(self, message, code=1, domain="atspi_error"):
                self.message, self.code, self.domain = message, code, domain

        path = "/org/a11y/atspi/accessible/0/202914184810805067776"
        stale = TransportError(path)
        calls = iter((stale, "fresh focused editor"))

        def check(_):
            value = next(calls)
            if isinstance(value, Exception):
                raise value
            return value

        self.assertIsNone(probe.conflict_tree_snapshot(lambda: self.notes, check, TransportError))
        self.assertEqual(probe.conflict_tree_snapshot(lambda: self.notes, check, TransportError),
                         "fresh focused editor")

        for error in (TransportError("bus disconnected"), TransportError(path, 2),
                      TransportError(path, domain="other_error")):
            def report():
                raise error

            with self.assertRaises(TransportError) as raised:
                probe.conflict_tree_snapshot(report, check, TransportError)
            self.assertIs(raised.exception, error)

    def test_probe_failure_reports_only_safe_atspi_error_categories(self):
        class TransportError(Exception):
            code = 1

            def __init__(self, domain, message):
                super().__init__(message)
                self.domain, self.message = domain, message

        path = "/org/a11y/atspi/accessible/0/42"
        for domain, message, expected in (
            ("atspi_error", path, "numeric-object-path"),
            ("atspi_error", "/org/a11y/atspi/accessible/root", "application-root-path"),
            ("atspi_error", "Missing object " + path, "contains-accessible-path"),
            ("atspi_error", "private error body", "other"),
            ("private domain", path, "non-atspi-domain"),
        ):
            with self.subTest(expected=expected):
                payload = probe.failure_payload(TransportError(domain, message))
                self.assertEqual(payload["atspi_message_kind"], expected)
                self.assertEqual(payload["error_code"], 1)
                self.assertNotIn(message, str(payload))
                self.assertNotIn(domain, str(payload))
        self.assertNotIn("atspi_message_kind", probe.failure_payload(OSError("private")))

    def test_bounded_tree(self):
        self.desk.children.extend([self.gtk] * 256)
        with self.assertRaisesRegex(probe.ProbeError, "invalid-node-count"):
            self.run_probe()


if __name__ == "__main__":
    unittest.main()
