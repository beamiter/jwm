#!/usr/bin/env python3
"""Native CI only: bounded, download-only acquisition before normal apt install.

APT selects the exact archive and SHA256 from the indexes just authenticated by
strict apt update. The fallback changes only the official Ubuntu mirror, not the
archive or digest. Normal apt install still performs its own authentication and
cache checks. Never run dpkg inside the timed process group.
"""
import hashlib
import os
from pathlib import Path
import pwd
import re
import shlex
import signal
import stat
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


def prepare_download_directory(folder):
    # Only this newly-created private directory changes owner. APT methods must
    # be able to create both the archive and local hash-verification output.
    # Keep _apt sandboxing enabled; missing account/permissions are fatal.
    account = pwd.getpwnam("_apt")
    os.chown(folder, account.pw_uid, account.pw_gid)
    folder.chmod(0o700)


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
    with tempfile.TemporaryDirectory(prefix="jwm-cjk-", dir="/tmp") as directory:
        folder = Path(directory)
        prepare_download_directory(folder)
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
        cache_verified_archive(archive, size, digest)


def cache_verified_archive(archive, size, digest):
    # _apt owns the acquisition directory. Never assume a path still refers to
    # the bytes apt-helper checked (file methods may also return symlinks).
    # Copy a bounded amount into root's private 0600 staging inode, then verify
    # THAT inode before exposing exactly those bytes through atomic replace.
    with tempfile.NamedTemporaryFile(prefix="jwm-cjk-", dir=CACHE, delete=False) as staged:
        staging = Path(staged.name)
        try:
            # A replaced FIFO/device must not block the root cache-copy phase.
            # Symlinks to regular files are acceptable: staging bytes are hashed.
            fd = os.open(archive, os.O_RDONLY | os.O_NONBLOCK)
            with os.fdopen(fd, "rb") as source:
                if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                    raise RuntimeError("CJK archive must be a regular file")
                remaining = size + 1
                while remaining:
                    chunk = source.read(min(1024 * 1024, remaining))
                    if not chunk:
                        break
                    staged.write(chunk)
                    remaining -= len(chunk)
            staged.flush()
            with staging.open("rb") as copied:
                actual = hashlib.file_digest(copied, "sha256").hexdigest()
            if staging.stat().st_size != size or actual.lower() != digest.removeprefix("SHA256:").lower():
                raise RuntimeError("CJK staging archive verification failed")
            staging.chmod(0o644)
            os.replace(staging, CACHE / archive.name)
        finally:
            staging.unlink(missing_ok=True)



if __name__ == "__main__":
    try:
        prefetch(sys.argv[1:])
    except (KeyError, OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"ci-prefetch-cjk: {error}", file=sys.stderr)
        sys.exit(1)
