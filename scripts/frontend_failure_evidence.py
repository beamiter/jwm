"""Passive, bounded X11 observations for private acceptance displays only."""
from itertools import islice
import re
import struct
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



def _direct_children(tree):
    """Parse only xwininfo's direct-child section, never root/parent IDs."""
    children = []
    in_children = False
    for line in tree[:16384].splitlines():
        if re.fullmatch(r"\s*[0-9]+ (?:child|children):\s*", line):
            in_children = True
            continue
        if in_children:
            match = re.match(r"\s+(0x[0-9a-fA-F]{1,8})\s", line)
            if match and match[1] not in children:
                children.append(match[1])
            if len(children) == 4:
                break
    return children


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
            "direct_child_ids": _direct_children(tree) if tree is not None else [],
            # Only XIDs, not window names or class strings.
            "tree_xids": re.findall(r"0x[0-9a-fA-F]+", tree[:16384])[:32] if tree is not None else None,
        }
    # Snapshot only explicitly listed direct children after top-level evidence.
    # Keep at least one second of the existing total budget for root capture.
    # Never traverse grandchildren or change mapping, focus, input or repaint.
    child_ids = []
    for row in result["windows"].values():
        for child in row.pop("direct_child_ids", []):
            if child not in child_ids and len(child_ids) < 4:
                child_ids.append(child)
    result["child_windows"] = {}
    for child in child_ids:
        if deadline - time.monotonic() <= 1:
            break
        stats = read(["xwininfo", "-id", child, "-stats"])
        result["child_windows"][child] = _sanitize(stats) if stats is not None else None
    # The caller owns a private Xvfb fixture, never the user's desktop. Capture
    # final display pixels without a compositor IPC request or a repaint.
    remaining = deadline - time.monotonic()
    if remaining > 0:
        destination = Path(evidence) / "frontend-timeout-display.xwd"
        try:
            subprocess.run(["xwd", "-root", "-silent", "-out", str(destination)], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=min(1, remaining), check=True)
            # Preserve the real XWD format. Do not pretend this is a PNG or
            # accept an empty/partial dump as visual evidence.
            with destination.open("rb") as capture:
                header = capture.read(100)
            if len(header) != 100 or destination.stat().st_size > 16 * 1024 * 1024:
                raise ValueError("invalid XWD size")
            fields = struct.unpack(">25I", header)
            if fields[1] != 7 or not 100 <= fields[0] <= 65536 or not all(0 < value <= 4096 for value in fields[4:6]):
                raise ValueError("invalid XWD header")
            result["private_display_capture"] = destination.name
            result["capture_format"] = "xwd"
        except ValueError:
            result["capture_failed"] = True
            result["capture_failure"] = {"kind": "invalid-dump"}
        except FileNotFoundError as error:
            result["capture_failed"] = True
            kind = "missing-dump" if error.filename == str(destination) else "missing-tool"
            result["capture_failure"] = {"kind": kind}
        except subprocess.TimeoutExpired:
            result["capture_failed"] = True
            result["capture_failure"] = {"kind": "timeout"}
        except subprocess.CalledProcessError as error:
            result["capture_failed"] = True
            result["capture_failure"] = {"kind": "exit-status", "code": error.returncode}
        except OSError as error:
            result["capture_failed"] = True
            result["capture_failure"] = {"kind": "os-error", "errno": error.errno}
    return result
