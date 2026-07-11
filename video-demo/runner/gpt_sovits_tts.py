#!/usr/bin/env python3
"""Small GPT-SoVITS API v2 adapter for video-demo's --tts-command."""

from __future__ import annotations

import argparse
import json
import os
import tempfile
import urllib.error
import urllib.request
import wave
from pathlib import Path


def synthesize(args: argparse.Namespace) -> None:
    text_path = Path(args.text_file).expanduser().resolve()
    output = Path(args.output).expanduser().resolve()
    ref_audio = Path(args.ref_audio).expanduser().resolve()
    if not text_path.is_file():
        raise FileNotFoundError(f"narration text does not exist: {text_path}")
    if not ref_audio.is_file():
        raise FileNotFoundError(f"reference audio does not exist: {ref_audio}")

    text = text_path.read_text(encoding="utf-8").strip()
    if not text:
        raise ValueError(f"narration text is empty: {text_path}")
    payload = {
        "text": text,
        "text_lang": args.text_lang,
        "ref_audio_path": str(ref_audio),
        "prompt_text": args.prompt_text,
        "prompt_lang": args.prompt_lang,
        "top_k": args.top_k,
        "top_p": args.top_p,
        "temperature": args.temperature,
        "text_split_method": args.text_split_method,
        "batch_size": 1,
        "split_bucket": True,
        "speed_factor": args.speed,
        "fragment_interval": args.fragment_interval,
        "seed": args.seed,
        "media_type": "wav",
        "streaming_mode": False,
        "parallel_infer": True,
        "repetition_penalty": args.repetition_penalty,
        "sample_steps": args.sample_steps,
    }
    request = urllib.request.Request(
        args.api_url.rstrip("/") + "/tts",
        data=json.dumps(payload, ensure_ascii=False).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=args.timeout) as response:
            audio = response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"GPT-SoVITS returned HTTP {error.code}: {detail}") from error
    except urllib.error.URLError as error:
        raise RuntimeError(
            f"cannot reach GPT-SoVITS API at {args.api_url}; start api_v2.py first: {error.reason}"
        ) from error

    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=output.name + ".", suffix=".tmp", dir=output.parent)
    os.close(fd)
    temporary = Path(temporary_name)
    try:
        temporary.write_bytes(audio)
        with wave.open(str(temporary), "rb") as wav:
            if wav.getnframes() <= 0 or wav.getframerate() <= 0:
                raise RuntimeError("GPT-SoVITS returned an empty WAV")
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)
    print(f"GPT-SoVITS wrote {output} ({len(audio)} bytes)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--text-file", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--ref-audio", required=True)
    parser.add_argument("--prompt-text", required=True)
    parser.add_argument("--api-url", default="http://127.0.0.1:9880")
    parser.add_argument("--text-lang", default="zh")
    parser.add_argument("--prompt-lang", default="zh")
    parser.add_argument("--text-split-method", default="cut1")
    parser.add_argument("--top-k", type=int, default=15)
    parser.add_argument("--top-p", type=float, default=1.0)
    parser.add_argument("--temperature", type=float, default=1.0)
    parser.add_argument("--speed", type=float, default=1.0)
    parser.add_argument("--fragment-interval", type=float, default=0.3)
    parser.add_argument("--sample-steps", type=int, default=8)
    parser.add_argument("--repetition-penalty", type=float, default=1.35)
    parser.add_argument("--seed", type=int, default=-1)
    parser.add_argument("--timeout", type=float, default=300.0)
    return parser.parse_args()


if __name__ == "__main__":
    synthesize(parse_args())
