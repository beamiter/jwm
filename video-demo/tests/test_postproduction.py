import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from runner.postproduction import generate_assets, srt_time, subtitle_chunks


class PostproductionTests(unittest.TestCase):
    def test_srt_time_handles_hour_boundary(self):
        self.assertEqual(srt_time(3661.234), "01:01:01,234")

    def test_chinese_subtitles_are_limited_to_two_lines(self):
        chunks = subtitle_chunks("这是一个用于验证字幕自动分段规则的很长中文句子，而且它还包含标点。", width=8)
        self.assertTrue(chunks)
        self.assertTrue(all(len(chunk.splitlines()) <= 2 for chunk in chunks))
        self.assertTrue(all(len(line) <= 8 for chunk in chunks for line in chunk.splitlines()))

    def test_asset_generation_writes_global_timeline(self):
        with tempfile.TemporaryDirectory() as directory:
            generated = Path(directory)
            clips = generated / "clips"; clips.mkdir()
            (clips / "one.mp4").write_bytes(b"fake")
            scene = {"id": "one", "title": "One", "narration": "第一段旁白。"}
            with patch("runner.postproduction.probe_duration", return_value=4.0):
                generate_assets([scene], generated)
            self.assertIn("one", (generated / "editing/timeline.csv").read_text())
            self.assertIn("第一段旁白", (generated / "subtitles/final.srt").read_text())
            self.assertTrue((generated / "editing/chapters.ffmeta").exists())


if __name__ == "__main__":
    unittest.main()
