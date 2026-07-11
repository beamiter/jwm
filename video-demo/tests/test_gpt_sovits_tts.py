import argparse
import io
import json
import tempfile
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

from runner.gpt_sovits_tts import synthesize


class Response:
    def __init__(self, body):
        self.body = body

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self):
        return self.body


def wav_bytes():
    output = io.BytesIO()
    with wave.open(output, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(32000)
        wav.writeframes(b"\0\0" * 320)
    return output.getvalue()


class GptSovitsAdapterTests(unittest.TestCase):
    def test_posts_chinese_text_and_atomically_writes_wav(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            text_file, ref_audio, output = root / "text.txt", root / "ref.wav", root / "voice/out.wav"
            text_file.write_text("中文旁白。")
            ref_audio.write_bytes(wav_bytes())
            args = argparse.Namespace(
                text_file=str(text_file), output=str(output), ref_audio=str(ref_audio),
                prompt_text="参考文本。", api_url="http://127.0.0.1:9880", text_lang="zh",
                prompt_lang="zh", text_split_method="cut1", top_k=15, top_p=1.0,
                temperature=1.0, speed=1.0, fragment_interval=0.3, seed=-1,
                repetition_penalty=1.35, sample_steps=8, timeout=5.0,
            )
            with patch("runner.gpt_sovits_tts.urllib.request.urlopen", return_value=Response(wav_bytes())) as call:
                synthesize(args)
            payload = json.loads(call.call_args.args[0].data.decode("utf-8"))
            self.assertEqual(payload["text"], "中文旁白。")
            self.assertEqual(payload["text_split_method"], "cut1")
            self.assertEqual(payload["sample_steps"], 8)
            self.assertTrue(output.is_file())
            with wave.open(str(output), "rb") as wav:
                self.assertGreater(wav.getnframes(), 0)


if __name__ == "__main__":
    unittest.main()
