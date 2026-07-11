from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path

from .jwm_ipc import JwmIpc


@dataclass
class EnvironmentReport:
    ok: bool
    session_type: str
    display: str
    ipc_socket: str
    screen: str | None
    commands: dict[str, str | None]
    errors: list[str]
    warnings: list[str]

    def as_dict(self) -> dict:
        return asdict(self)


def _screen_dimensions() -> str | None:
    if not shutil.which("xdpyinfo"):
        return None
    result = subprocess.run(["xdpyinfo"], text=True, capture_output=True, timeout=5, check=False)
    for line in result.stdout.splitlines():
        if "dimensions:" in line:
            return line.split("dimensions:", 1)[1].strip().split()[0]
    return None


def preflight(ipc: JwmIpc) -> EnvironmentReport:
    session = os.environ.get("XDG_SESSION_TYPE", "").lower()
    display = os.environ.get("DISPLAY", "")
    commands = {name: shutil.which(name) for name in ("ffmpeg", "ffprobe", "xdpyinfo", "glxinfo", "xdotool")}
    errors: list[str] = []
    warnings: list[str] = []
    if not display:
        errors.append("DISPLAY is unset; a real X11 session is required")
    if session and session != "x11":
        errors.append(f"XDG_SESSION_TYPE={session!r}; formal recording requires x11")
    for required in ("ffmpeg", "ffprobe", "xdpyinfo"):
        if not commands[required]:
            errors.append(f"required command not found: {required}")
    if not commands["xdotool"]:
        warnings.append("xdotool is unavailable; pointer-driven effect scenes are disabled")
    if not commands["glxinfo"]:
        warnings.append("glxinfo is unavailable; GPU diagnostics are incomplete")
    if not ipc.path.exists():
        errors.append(f"JWM IPC socket not found: {ipc.path}")
    else:
        try:
            ipc.query("get_version")
        except Exception as exc:
            errors.append(f"JWM IPC is not responding: {exc}")
        else:
            try:
                ipc.query("get_recording_status")
            except Exception as exc:
                errors.append(
                    "running JWM lacks the explicit recording IPC required by video-demo "
                    f"({exc}); rebuild and restart JWM, not only the demo client"
                )
    return EnvironmentReport(not errors, session, display, str(ipc.path), _screen_dimensions(), commands, errors, warnings)
