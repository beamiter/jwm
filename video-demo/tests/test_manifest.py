import tomllib
import unittest
from pathlib import Path

from runner.run_demo import (
    layout_name,
    occupied_user_tags,
    resolve_args,
    uses_symbol,
    validate_scene,
    window_fully_left_of_desktop,
    window_intersects_monitor,
)


BASE = Path(__file__).resolve().parents[1]


class StageTwoManifestTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data = tomllib.loads((BASE / "manifest/scenes.toml").read_text())
        cls.scenes = {scene["id"]: scene for scene in cls.data["scene"]}

    def test_layout_profile_covers_every_jwm_layout(self):
        expected = {
            "tile", "float", "monocle", "fibonacci", "centeredmaster", "bstack",
            "grid", "deck", "threecol", "tatami", "fullscreen", "scrolling", "vstack",
        }
        actual = {self.scenes[scene_id]["layout"] for scene_id in self.data["profiles"]["layouts"]}
        self.assertEqual(actual, expected)

    def test_ready_stage_two_scenes_have_actions_and_narration(self):
        for scene_id in self.data["profiles"]["stage2"]:
            scene = self.scenes[scene_id]
            self.assertEqual(scene["status"], "ready")
            self.assertTrue(scene["actions"])
            self.assertTrue(scene["narration"].strip())

    def test_focus_actions_are_demo_isolated(self):
        for scene_id in self.data["profiles"]["stage2"]:
            commands = [action["command"] for action in self.scenes[scene_id]["actions"]]
            self.assertNotIn("focusstack", commands)
            self.assertNotIn("scrolling_focus_column", commands)

    def test_symbolic_tags_are_resolved_recursively(self):
        value = {"tag": "$alternate_tag", "nested": ["$demo_tag"]}
        self.assertEqual(resolve_args(value, 256, 128), {"tag": 128, "nested": [256]})

    def test_layout_profile_does_not_require_secondary_tag(self):
        layouts = [self.scenes[scene_id] for scene_id in self.data["profiles"]["layouts"]]
        self.assertFalse(any(uses_symbol(scene.get("actions", []), "$alternate_tag") for scene in layouts))
        tag_scenes = [self.scenes[scene_id] for scene_id in self.data["profiles"]["tags"]]
        self.assertTrue(any(uses_symbol(scene.get("actions", []), "$alternate_tag") for scene in tag_scenes))

    def test_workspace_layout_debug_value_is_normalized(self):
        self.assertEqual(layout_name('LayoutEnum("tile")'), "tile")
        self.assertEqual(layout_name('LayoutEnum("centeredmaster")'), "centeredmaster")

    def test_global_floating_bar_does_not_occupy_every_tag(self):
        windows = [
            {"class": "gtk_bar", "tags": 511, "is_floating": True},
            {"class": "jterm1", "tags": 1, "is_floating": False},
        ]
        self.assertEqual(occupied_user_tags(windows, 9), 1)

    def test_stage_three_effects_require_manual_visual_review(self):
        expected = {"effect-rounded", "effect-shadow", "effect-blur", "effect-fade", "effect-wobbly", "effect-motion-trail", "effect-overview", "effect-magnifier", "effect-annotation"}
        self.assertEqual(set(self.data["profiles"]["stage3"]), expected)
        for scene_id in expected:
            scene = self.scenes[scene_id]
            self.assertEqual(scene["status"], "ready")
            self.assertTrue(scene["manual_review"])
            self.assertIn("effect_changed", scene["assertions"])

    def test_minimize_profile_uses_a_balanced_demo_client_round_trip(self):
        self.assertEqual(self.data["profiles"]["minimize"], ["wm-minimize-restore"])
        scene = self.scenes["wm-minimize-restore"]
        self.assertEqual(
            [action["command"] for action in scene["actions"]],
            ["minimize_demo", "restore_demo"],
        )
        self.assertIn("minimize_restore_round_trip", scene["assertions"])
        self.assertTrue(scene["manual_review"])
        validate_scene(scene)

    def test_minimize_scene_validation_rejects_unbalanced_controls(self):
        with self.assertRaisesRegex(RuntimeError, "restores without"):
            validate_scene({"id": "bad", "actions": [{"command": "restore_demo"}]})
        with self.assertRaisesRegex(RuntimeError, "leaves a demo window minimized"):
            validate_scene({"id": "bad", "actions": [{"command": "minimize_demo"}]})

    def test_minimized_geometry_probe_handles_negative_monitor_origins(self):
        monitors = [
            {"x": -1920, "y": 0, "w": 1920, "h": 1080},
            {"x": 0, "y": 0, "w": 2560, "h": 1440},
        ]
        hidden = {"x": -2720, "y": 100, "w": 800, "h": 600}
        restored = {"x": -1600, "y": 100, "w": 800, "h": 600}
        self.assertTrue(window_fully_left_of_desktop(hidden, monitors))
        self.assertFalse(window_intersects_monitor(hidden, monitors))
        self.assertFalse(window_fully_left_of_desktop(restored, monitors))
        self.assertTrue(window_intersects_monitor(restored, monitors))


if __name__ == "__main__":
    unittest.main()
