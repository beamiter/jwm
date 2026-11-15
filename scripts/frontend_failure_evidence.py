"""Passive, bounded X11 observations for private acceptance displays only."""
from itertools import islice
import re
import subprocess
import time
import tempfile
from pathlib import Path


def _sanitize(text):
    # xwininfo embeds window titles. Retain numeric/state fields, never titles.
    fields = {}
    for line in text[:16384].splitlines():
        match = re.match(r"\s*(Absolute upper-left X|Absolute upper-left Y|Width|Height|Depth|Map State|Visibility State|Window state):\s*([\w +.-]+)$", line)
        if match:
            fields[match[1]] = match[2]
    return fields


def collect(window_ids, evidence, env):
    """Never map, raise, resize, refresh or send input; never changes readiness."""
    deadline = time.monotonic() + 8
    result = {"windows": {}, "private_display_capture": None}
    ids = []
    for value in islice(window_ids, 16):
        value = str(value)
        if re.fullmatch(r"(?:[0-9]{1,10}|0x[0-9a-fA-F]{1,8})", value) and value not in ids:
            ids.append(value)
        if len(ids) == 4:
            break

    def read(args):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        try:
            # Keep command output out of an unbounded in-memory PIPE. Commands
            # have a one-second deadline; parse at most 16 KiB from the scratch
            # file, which is closed/deleted before the next observation.
            with tempfile.TemporaryFile() as output:
                subprocess.run(args, env=env, stdout=output,
                               stderr=subprocess.DEVNULL,
                               timeout=min(1, remaining), check=True)
                output.seek(0)
                return output.read(16384).decode("utf-8", errors="replace")
        except (OSError, subprocess.SubprocessError):
            return None

    for xid in ids:
        stats = read(["xwininfo", "-id", xid, "-stats"])
        tree = read(["xwininfo", "-id", xid, "-tree"])
        result["windows"][xid] = {
            "stats": _sanitize(stats) if stats is not None else None,
            # Only XIDs, not window names or class strings.
            "tree_xids": re.findall(r"0x[0-9a-fA-F]+", tree[:16384])[:32] if tree is not None else None,
        }
    # The caller owns a private Xvfb fixture, never the user's desktop. Capture
    # final display pixels without a compositor IPC request or a repaint.
    remaining = deadline - time.monotonic()
    if remaining > 0:
        destination = Path(evidence) / "frontend-timeout-display.png"
        try:
            subprocess.run(["import", "-window", "root", str(destination)], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=min(1, remaining), check=True)
            result["private_display_capture"] = destination.name
        except (OSError, subprocess.SubprocessError):
            result["capture_failed"] = True
    return result
