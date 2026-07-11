from __future__ import annotations

import csv
import json
import re
import shlex
import subprocess
from pathlib import Path


def probe_duration(path: Path) -> float:
    result = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1", str(path)], text=True, capture_output=True, check=True)
    return float(result.stdout.strip())


def srt_time(seconds: float) -> str:
    millis = max(0, round(seconds * 1000))
    hours, remainder = divmod(millis, 3_600_000)
    minutes, remainder = divmod(remainder, 60_000)
    secs, ms = divmod(remainder, 1000)
    return f"{hours:02d}:{minutes:02d}:{secs:02d},{ms:03d}"


def subtitle_chunks(text: str, width: int = 18) -> list[str]:
    clauses = [item.strip() for item in re.split(r"(?<=[。！？；，])", text.strip()) if item.strip()]
    lines: list[str] = []
    for clause in clauses or [text.strip()]:
        while len(clause) > width:
            lines.append(clause[:width])
            clause = clause[width:]
        if clause: lines.append(clause)
    return ["\n".join(lines[index:index + 2]) for index in range(0, len(lines), 2)] or [""]


def selected_assets(scenes: list[dict], generated: Path) -> list[dict]:
    assets = []
    for scene in scenes:
        clip = generated / "clips" / f"{scene['id']}.mp4"
        if not clip.exists(): raise FileNotFoundError(f"missing scene clip: {clip}")
        assets.append({"scene": scene, "clip": clip.resolve(), "duration": probe_duration(clip)})
    return assets


def generate_assets(scenes: list[dict], generated: Path) -> list[dict]:
    assets = selected_assets(scenes, generated)
    narration_dir, subtitles_dir, editing = generated / "narration", generated / "subtitles", generated / "editing"
    for directory in (narration_dir, subtitles_dir, editing): directory.mkdir(parents=True, exist_ok=True)
    concat_lines, combined_srt, narration_jsonl, timeline_rows = [], [], [], []
    chapters = [";FFMETADATA1"]
    offset, subtitle_index = 0.0, 1
    for order, asset in enumerate(assets, 1):
        scene, clip, duration = asset["scene"], asset["clip"], asset["duration"]
        text = scene["narration"].strip()
        (narration_dir / f"{scene['id']}.txt").write_text(text + "\n")
        chunks = subtitle_chunks(text)
        slot = duration / len(chunks)
        local_entries = []
        for index, chunk in enumerate(chunks):
            start, end = index * slot, min(duration, (index + 1) * slot)
            local_entries.append(f"{index + 1}\n{srt_time(start)} --> {srt_time(end)}\n{chunk}\n")
            combined_srt.append(f"{subtitle_index}\n{srt_time(offset + start)} --> {srt_time(offset + end)}\n{chunk}\n")
            subtitle_index += 1
        (subtitles_dir / f"{scene['id']}.srt").write_text("\n".join(local_entries))
        concat_lines.append(f"file '{clip}'")
        timeline_rows.append([order, scene["id"], scene["title"], f"{offset:.3f}", f"{offset + duration:.3f}", f"{duration:.3f}", str(clip)])
        narration_jsonl.append(json.dumps({"scene": scene["id"], "title": scene["title"], "text": text, "target_duration": duration}, ensure_ascii=False))
        chapters.extend(["[CHAPTER]", "TIMEBASE=1/1000", f"START={round(offset * 1000)}", f"END={round((offset + duration) * 1000)}", f"title={scene['title']}"])
        offset += duration
    (editing / "concat.txt").write_text("\n".join(concat_lines) + "\n")
    (editing / "chapters.ffmeta").write_text("\n".join(chapters) + "\n")
    (editing / "narration.jsonl").write_text("\n".join(narration_jsonl) + "\n")
    (subtitles_dir / "final.srt").write_text("\n".join(combined_srt))
    with (editing / "timeline.csv").open("w", newline="") as handle:
        writer = csv.writer(handle); writer.writerow(["order", "scene", "title", "start", "end", "duration", "clip"]); writer.writerows(timeline_rows)
    return assets


def run_tts(assets: list[dict], generated: Path, command_template: str) -> None:
    voice_dir = generated / "voice"; voice_dir.mkdir(parents=True, exist_ok=True)
    for asset in assets:
        scene = asset["scene"]; text_file = generated / "narration" / f"{scene['id']}.txt"; output = voice_dir / f"{scene['id']}.wav"
        command = [token.format(text=str(text_file), output=str(output), scene=scene["id"]) for token in shlex.split(command_template)]
        subprocess.run(command, check=True)
        if not output.exists(): raise RuntimeError(f"TTS command did not create {output}")


def assemble_silent(generated: Path) -> Path:
    output = generated / "final-silent.mp4"
    subprocess.run(["ffmpeg", "-v", "error", "-f", "concat", "-safe", "0", "-i", str(generated / "editing/concat.txt"), "-i", str(generated / "editing/chapters.ffmeta"), "-map", "0:v:0", "-map_metadata", "1", "-an", "-c:v", "copy", "-movflags", "+faststart", "-y", str(output)], check=True)
    probe_duration(output)
    return output


def assemble_voice(assets: list[dict], generated: Path) -> Path:
    mux_dir = generated / "tmp" / "voice-mux"; mux_dir.mkdir(parents=True, exist_ok=True)
    concat = []
    for asset in assets:
        scene_id, clip = asset["scene"]["id"], asset["clip"]
        voice, output = generated / "voice" / f"{scene_id}.wav", mux_dir / f"{scene_id}.mp4"
        if not voice.exists(): raise FileNotFoundError(f"missing narration audio: {voice}")
        subprocess.run(["ffmpeg", "-v", "error", "-i", str(clip), "-i", str(voice), "-filter_complex", "[1:a]apad[a]", "-map", "0:v:0", "-map", "[a]", "-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-shortest", "-y", str(output)], check=True)
        concat.append(f"file '{output.resolve()}'")
    list_path = mux_dir / "concat.txt"; list_path.write_text("\n".join(concat) + "\n")
    final = generated / "final-with-voice.mp4"
    subprocess.run(["ffmpeg", "-v", "error", "-f", "concat", "-safe", "0", "-i", str(list_path), "-c", "copy", "-movflags", "+faststart", "-y", str(final)], check=True)
    probe_duration(final)
    return final
