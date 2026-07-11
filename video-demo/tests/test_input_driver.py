import unittest
from unittest.mock import patch

from runner.input_driver import XdotoolInput
from runner.run_demo import action_time_budget


class InputDriverTests(unittest.TestCase):
    def test_action_budget_includes_pointer_duration(self):
        actions = [{"hold": 0.8}, {"hold": 1.0, "duration": 2.5}]
        self.assertEqual(action_time_budget(actions), 4.3)

    def test_smooth_path_does_not_wait_for_each_xtest_sample(self):
        driver = XdotoolInput.__new__(XdotoolInput)
        calls = []
        driver._run = lambda *args: calls.append(args)
        with patch("runner.input_driver.time.sleep"):
            points = driver.smooth((0, 0), (100, 50), steps=4, duration=1.0)
        self.assertEqual(len(points), 5)
        self.assertEqual(len(calls), 5)
        self.assertTrue(all(call[0] == "mousemove" and "--sync" not in call for call in calls))


if __name__ == "__main__":
    unittest.main()
