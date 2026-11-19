#!/usr/bin/env python3
"""Native CI only: bounded, download-only acquisition before normal apt install.

APT selects the exact archive and SHA256 from the indexes just authenticated by
strict apt update. The fallback changes only the official Ubuntu mirror, not the
archive or digest. Normal apt install still performs its own authentication and
cache checks. Never run dpkg inside the timed process group.
"""
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
from urllib.parse import unquote

ATTEMPT_SECONDS = 90
APT_HELPER = "/usr/lib/apt/apt-helper"
CACHE = Path("/var/cache/apt/archives")


def bounded(command, *, cwd, seconds=ATTEMPT_SECONDS):
    process = subprocess.Popen(command, cwd=cwd, start_new_session=True)
    try:
        return process.wait(timeout=seconds)
    except subprocess.TimeoutExpired:
        # Kill the entire download-only process group, including apt methods.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        return 124


def archive_record(output):
    lines = [line for line in output.splitlines() if line.startswith("'")]
    if len(lines) != 1:
        raise ValueError("expected exactly one CJK archive")
    uri, filename, size, digest = shlex.split(lines[0])
    if not re.fullmatch(r"fonts-noto-cjk_[A-Za-z0-9.+~:%-]+_all\.deb", filename):
        raise ValueError("unexpected package or archive filename")
    if not size.isdecimal() or int(size) <= 0:
        raise ValueError("invalid archive size")
    if not re.fullmatch(r"SHA256:[0-9a-fA-F]{64}", digest):
        raise ValueError("a SHA256 archive digest is required")
    # Keep the signed-index path verbatim. Reject any query/traversal ambiguity.
    match = re.search(r"(/pool/(?:main|universe)/f/fonts-noto-cjk/[^/?#]+)$", uri)
    if not match or not re.fullmatch(
        r"fonts-noto-cjk_[A-Za-z0-9.+~:-]+_all\.deb",
        unquote(match.group(1).rsplit("/", 1)[-1]),
    ):
        raise ValueError("unexpected Ubuntu CJK archive path")
    return filename, int(size), digest, uri, "https://archive.ubuntu.com/ubuntu" + match.group(1)


def prefetch(options):
    # APT prefers SHA512 when both exist. Request the strong SHA256 field
    # explicitly; absent SHA256 still fails closed in archive_record.
    selected = subprocess.run(
        ["apt-get", *options, "-o", "Acquire::ForceHash=SHA256",
         "--print-uris", "download", "fonts-noto-cjk"],
        check=True, text=True, capture_output=True, timeout=30,
    )
    filename, size, digest, primary, fallback = archive_record(selected.stdout)
    # Same filename + digest for both attempts, with no second index refresh.
    with tempfile.TemporaryDirectory(prefix="jwm-cjk-") as directory:
        folder = Path(directory)
        # Permit APT methods to traverse the private acquisition directory.
        folder.chmod(0o755)
        archive = folder / filename
        status = bounded([APT_HELPER, *options, "download-file", primary,
                          str(archive), digest], cwd=folder)
        if status:
            archive.unlink(missing_ok=True)
            print(f"CJK primary download failed ({status}); trying official archive", flush=True)
            status = bounded([APT_HELPER, *options, "download-file", fallback,
                              str(archive), digest], cwd=folder)
        if status:
            raise RuntimeError(f"CJK download failed ({status}); refusing installation")
        # Reuse apt's native hash verifier even for a successful primary download.
        verified = folder / "verified.deb"
        status = bounded([APT_HELPER, "download-file", archive.as_uri(), str(verified), digest], cwd=folder)
        if status or verified.stat().st_size != size:
            raise RuntimeError("CJK archive verification failed")
        # Cache only a completely verified archive; the subsequent normal apt
        # install retains all original package names and authentication checks.
        with tempfile.NamedTemporaryFile(prefix="jwm-cjk-", dir=CACHE, delete=False) as staged:
            staging = Path(staged.name)
        try:
            shutil.copyfile(verified, staging)
            staging.chmod(0o644)
            os.replace(staging, CACHE / filename)
        finally:
            staging.unlink(missing_ok=True)


if __name__ == "__main__":
    try:
        prefetch(sys.argv[1:])
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"ci-prefetch-cjk: {error}", file=sys.stderr)
        sys.exit(1)
