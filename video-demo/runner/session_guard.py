from __future__ import annotations

import fcntl
import json
import os
import shutil
from pathlib import Path
from typing import Any

from .jwm_ipc import JwmIpc


def _layout_name(value: object) -> str:
    text = str(value).lower()
    for name in ("centeredmaster", "fibonacci", "fullscreen", "scrolling", "threecol", "monocle", "vstack", "tatami", "bstack", "grid", "deck", "float", "tile"):
        if name in text:
            return name
    return "tile"


class SessionGuard:
    def __init__(self, ipc: JwmIpc, repo: Path, run_dir: Path, backend: str) -> None:
        self.ipc, self.repo, self.run_dir, self.backend = ipc, repo, run_dir, backend
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/jwm-{os.getuid()}"))
        self.lock_path = runtime / "jwm-video-demo.lock"
        self.state_path = runtime / "jwm-video-demo-recovery.json"
        self.lock_file = None
        self.original_tag: int | None = None
        self.original_layout: str | None = None
        self.config_backups: list[tuple[Path, Path]] = []

    def __enter__(self) -> "SessionGuard":
        self.lock_path.parent.mkdir(parents=True, exist_ok=True)
        self.lock_file = self.lock_path.open("a+")
        try:
            fcntl.flock(self.lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise RuntimeError(f"another video run owns {self.lock_path}") from exc
        workspaces = self.ipc.query("get_workspaces") or []
        focused = next((item for item in workspaces if item.get("focused")), None)
        if focused:
            self.original_tag = int(focused["tag_mask"])
            self.original_layout = _layout_name(focused["layout"])
        status = self.ipc.query("get_config_status") or {}
        configured_path = Path(status["path"]) if status.get("path") else None
        config_home = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "jwm"
        candidates = [configured_path] if configured_path else [config_home / "config_x11.toml", config_home / "config.toml"]
        for source in candidates:
            if source is None:
                continue
            name = source.name
            if source.exists():
                target = self.run_dir / f"{name}.before"
                shutil.copy2(source, target)
                self.config_backups.append((source, target))
        self._write_state(False, [])
        return self

    def _write_state(self, recording: bool, pids: list[int]) -> None:
        state: dict[str, Any] = {
            "pid": os.getpid(), "backend": self.backend, "recording_active": recording,
            "demo_pids": pids, "original_tag": self.original_tag,
            "original_layout": self.original_layout, "config_backups": [[str(a), str(b)] for a, b in self.config_backups],
        }
        self.state_path.write_text(json.dumps(state, indent=2) + "\n")

    def update(self, recording: bool, pids: list[int]) -> None:
        self._write_state(recording, pids)

    def restore(self) -> None:
        try:
            self.ipc.command("stop_recording")
        except Exception:
            pass
        for source, backup in self.config_backups:
            if backup.exists() and source.read_bytes() != backup.read_bytes():
                shutil.copy2(backup, source)
        if self.config_backups:
            try: self.ipc.command("reload_config")
            except Exception: pass
        if self.original_tag:
            try:
                self.ipc.command("view", {"tag": self.original_tag})
                if self.original_layout:
                    self.ipc.command("setlayout", {"layout": self.original_layout})
            except Exception:
                pass
        self.state_path.unlink(missing_ok=True)

    def __exit__(self, *_: object) -> None:
        self.restore()
        if self.lock_file:
            fcntl.flock(self.lock_file, fcntl.LOCK_UN)
            self.lock_file.close()
        self.lock_path.unlink(missing_ok=True)
