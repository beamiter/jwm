#!/usr/bin/env python3
"""Exercise actual release archives in an isolated destination, without root."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import struct
import subprocess
import tarfile
import tempfile
import time


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def verify_installation(bundle, version, destination):
    """Check bytes and links after both installation and rollback."""
    current = destination / "usr/local/lib/jwm/current"
    assert current.is_symlink() and not os.readlink(current).startswith("/")
    assert current.resolve() == destination / f"usr/local/lib/jwm/versions/{version}"
    payload = bundle / f"payload/usr/local/lib/jwm/versions/{version}"
    hashes, elf, stable_paths = {}, {}, []
    for line in (bundle / "release-manifest.tsv").read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        kind, name, version_path, stable_path, mode = line.split("\t")
        if stable_path != "-":
            stable = destination / stable_path
            assert stable.is_symlink() and not os.readlink(stable).startswith("/"), stable_path
            assert stable.resolve() == (current / version_path).resolve(), stable_path
            stable_paths.append(stable_path)
        if kind in ("built", "repo-file"):
            source = payload / version_path
            installed = current / version_path
            assert installed.is_file() and not installed.is_symlink(), version_path
            hashes[name] = digest(installed)
            assert hashes[name] == digest(source), name
            assert installed.stat().st_mode & 0o7777 == int(mode, 8), name
            if kind == "built":
                with installed.open("rb") as executable:
                    header = executable.read(20)
                assert len(header) == 20 and header[:4] == b"\x7fELF", name
                assert header[4] in (1, 2) and header[5] in (1, 2) and header[6] == 1, name
                byte_order = "<" if header[5] == 1 else ">"
                executable_type, machine = struct.unpack(byte_order + "HH", header[16:20])
                assert executable_type in (2, 3) and machine != 0, name
                elf[name] = {"class": 32 if header[4] == 1 else 64, "machine": machine}
        elif kind == "repo-tree":
            source = payload / version_path
            installed = current / version_path
            assert installed.is_dir() and not installed.is_symlink(), version_path
            expected_files = {p.relative_to(source) for p in source.rglob("*") if p.is_file()}
            actual_files = {p.relative_to(installed) for p in installed.rglob("*") if p.is_file()}
            assert expected_files == actual_files, version_path
            for relative in sorted(expected_files):
                path = installed / relative
                assert not path.is_symlink() and path.stat().st_mode & 0o7777 == int(mode, 8), str(path)
                key = str(PurePosixPath(version_path) / relative)
                hashes[key] = digest(path)
                assert hashes[key] == digest(source / relative), key
    for directory in [current.resolve(), *(p for p in current.rglob("*") if p.is_dir())]:
        assert not directory.is_symlink() and directory.stat().st_mode & 0o7777 == 0o755, str(directory)
    return {"sha256": hashes, "elf": elf, "stable_paths": stable_paths}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archives", type=Path, nargs="+")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = {"status": "running", "installations": [], "commands": []}

    def run(*argv, cwd=None, timeout=None):
        print("+", *map(str, argv), flush=True)
        # File operations can wait on the host filesystem journal. Do not kill
        # an in-progress installer transaction merely because storage is slow;
        # runtime smoke commands below have their own bounded timeout.
        started = time.monotonic()
        result = subprocess.run(list(map(str, argv)), cwd=cwd, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        report["commands"].append({"argv": list(map(str, argv)),
                                   "elapsed_seconds": round(time.monotonic() - started, 3),
                                   "returncode": result.returncode})
        print(result.stdout, end="", flush=True)
        result.check_returncode()
        return result.stdout

    with tempfile.TemporaryDirectory(prefix="jwm-release-acceptance-") as directory:
        work = Path(directory)
        destination = work / "system"
        original = destination / "usr/local/bin/jwm"
        original.parent.mkdir(parents=True)
        original.write_text("pre-existing installation\n")
        config = destination / "home/test/.config/jwm/config_x11.toml"
        config.parent.mkdir(parents=True)
        config.write_text('[status_bar]\nname = "custom_bar"\n')
        config_before = config.read_bytes()
        bundles = []
        stable_paths = set()
        for index, archive in enumerate(args.archives):
            archive = archive.resolve()
            run("sha256sum", "-c", archive.name + ".sha256", cwd=archive.parent)
            extraction = work / f"archive-{index}"
            extraction.mkdir()
            with tarfile.open(archive) as source:
                for member in source.getmembers():
                    path = PurePosixPath(member.name)
                    if path.is_absolute() or ".." in path.parts or not (member.isfile() or member.isdir()):
                        raise ValueError(f"unsafe archive member: {member.name}")
                source.extractall(extraction)
            entries = list(extraction.iterdir())
            assert len(entries) == 1 and entries[0].is_dir()
            bundle = entries[0]
            version = (bundle / "VERSION").read_text().strip()
            assert re.fullmatch(r"[0-9A-Za-z][0-9A-Za-z._+-]{0,127}", version) and ".." not in version
            bundles.append((bundle, version))
            run("bash", bundle / "install-release.sh", "install", "--destdir", destination, "--replace")
            verified = verify_installation(bundle, version, destination)
            stable_paths.update(verified["stable_paths"])
            helper = destination / "usr/local/bin/jwm-toggle-sidebar-mode"
            if "usr/local/bin/jwm-toggle-sidebar-mode" in verified["stable_paths"]:
                assert "Toggle shared sidebars" in run(helper, "--help", timeout=15)
            assert config.read_bytes() == config_before
            assert "protocol 1" in run(destination / "usr/local/bin/jwm-shell", "--version", timeout=15)
            report["installations"].append({"archive": str(archive), "archive_sha256": digest(archive),
                                             "version": version, **verified})
        if len(bundles) > 1:
            run("bash", bundles[-1][0] / "install-release.sh", "rollback", "--destdir", destination)
            report["rollback"] = {"version": bundles[-2][1],
                                  **verify_installation(*bundles[-2], destination)}
            assert config.read_bytes() == config_before
        run("bash", bundles[-1][0] / "install-release.sh", "uninstall", "--all", "--destdir", destination)
        assert original.is_file() and not original.is_symlink()
        assert original.read_text() == "pre-existing installation\n"
        assert config.read_bytes() == config_before
        for stable_path in stable_paths - {"usr/local/bin/jwm"}:
            path = destination / stable_path
            assert not path.exists() and not path.is_symlink(), stable_path
        assert not (destination / "usr/local/lib/jwm").exists()
    report.update(status="passed", config_preserved=True, original_installation_restored=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", dir=args.output.parent, delete=False) as target:
        target.write(json.dumps(report, indent=2) + "\n")
        target.flush()
        os.fsync(target.fileno())
        temporary = target.name
    os.replace(temporary, args.output)
    directory = os.open(args.output.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    print("PASS: actual release installation, upgrade, rollback, uninstall and preservation")


if __name__ == "__main__":
    main()
