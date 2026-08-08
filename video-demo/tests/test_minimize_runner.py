import unittest

from runner.run_demo import execute_action


class FakeIpc:
    def __init__(self):
        self.window = {
            "id": 42,
            "class": "JwmDemo",
            "is_focused": True,
            "x": 100,
            "y": 100,
            "w": 800,
            "h": 600,
            "tags": 1,
        }

    def query(self, name):
        if name == "get_windows":
            return [dict(self.window)]
        if name == "get_monitors":
            return [{"x": 0, "y": 0, "w": 1920, "h": 1080, "focused": True}]
        if name == "get_workspaces":
            return [
                {
                    "focused": True,
                    "tag_mask": 1,
                    "layout": "tile",
                    "m_fact": 0.55,
                    "n_master": 1,
                }
            ]
        if name == "get_tree":
            return [{"windows": [dict(self.window)]}]
        if name == "get_effect_status":
            return {}
        raise AssertionError(f"unexpected query: {name}")

    def command(self, name, args=None):
        raise AssertionError(f"demo controls must not use JWM IPC command {name}: {args}")


class FakeDemoWindows:
    def __init__(self, ipc):
        self.ipc = ipc
        self.last_minimized_window_id = None
        self.controls = []

    def control(self, window_id, command):
        self.controls.append((window_id, command))
        if command == "minimize":
            self.last_minimized_window_id = window_id
            self.ipc.window.update({"x": -800, "is_focused": False})
        elif command == "restore":
            self.last_minimized_window_id = None
            self.ipc.window.update({"x": 100, "is_focused": True})
        return {"success": True}


class MinimizeRunnerTests(unittest.TestCase):
    def test_actions_control_and_probe_the_same_managed_window(self):
        ipc = FakeIpc()
        windows = FakeDemoWindows(ipc)

        minimized = execute_action(
            {"command": "minimize_demo", "hold": 0}, ipc, windows, None, 1, 1
        )
        restored = execute_action(
            {"command": "restore_demo", "hold": 0}, ipc, windows, None, 1, 1
        )

        self.assertEqual(windows.controls, [(42, "minimize"), (42, "restore")])
        self.assertEqual(minimized["target_window_id"], restored["target_window_id"])
        self.assertEqual(
            minimized["state_probe"],
            {"managed": True, "focused": False, "fully_left_of_desktop": True},
        )
        self.assertEqual(
            restored["state_probe"],
            {"managed": True, "focused": True, "intersects_monitor": True},
        )


if __name__ == "__main__":
    unittest.main()
