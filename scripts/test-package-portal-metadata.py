#!/usr/bin/env python3
"""Verify the built package without extracting to system paths or installing."""

import argparse
from io import BytesIO
from pathlib import Path
import subprocess
import tarfile


ROOT = Path(__file__).resolve().parents[1]
NAME = "jwm-portal-metadata"
DESCRIPTOR = "usr/share/xdg-desktop-portal/portals/jwm.portal"
COPYRIGHT = f"usr/share/doc/{NAME}/copyright"


def dpkg(*args):
    return subprocess.check_output(["dpkg-deb", *args])


def verify_tar(data, *, files, directories):
    seen = set()
    files_seen = set()
    with tarfile.open(fileobj=BytesIO(data), mode="r:") as archive:
        for member in archive:
            name = member.name.removeprefix("./").rstrip("/") or "."
            if name in seen:
                raise ValueError(f"duplicate tar member {name!r}")
            seen.add(name)
            if (member.uid, member.gid, member.uname, member.gname) != (0, 0, "root", "root"):
                raise ValueError(f"{name}: expected root:root, got {member.uid}:{member.gid}")
            if member.isdir() and name in directories and member.mode == 0o755:
                continue
            if member.isfile() and name in files and member.mode == 0o644:
                files_seen.add(name)
                expected = files[name]
                if expected is None or archive.extractfile(member).read() == expected:
                    continue
            raise ValueError(f"unexpected tar member, content, or mode: {name!r}")
    if files_seen != set(files):
        raise ValueError(f"missing files: {set(files) - files_seen}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    parser.add_argument("--version", required=True, help="expected Debian version")
    args = parser.parse_args()
    expected_name = f"{NAME}_{args.version}_all.deb"
    if args.package.name != expected_name:
        raise ValueError(f"expected filename {expected_name}, got {args.package.name}")
    # Reject strong/weak relationship fields and any maintainer scripts/triggers.
    fields = {
        "Package": NAME,
        "Version": args.version,
        "Architecture": "all",
        "Section": "x11",
        "Priority": "optional",
        "Maintainer": "JWM maintainers <beamiter@users.noreply.github.com>",
        "Depends": "xdg-desktop-portal",
        "Homepage": "https://github.com/beamiter/jwm",
    }
    control = dpkg("--field", args.package).decode("utf-8")
    observed = {}
    for line in control.splitlines():
        if line[:1].isspace():
            continue
        key, value = line.split(": ", 1)
        if key in observed:
            raise ValueError(f"duplicate control field {key!r}")
        observed[key] = value
    if not observed.pop("Description", "") or observed != fields:
        raise ValueError(f"unexpected Debian control fields: {observed}")
    verify_tar(dpkg("--ctrl-tarfile", args.package), files={"control": None}, directories={"."})
    files = {
        DESCRIPTOR: (ROOT / "portal/dist/jwm.portal").read_bytes(),
        COPYRIGHT: (ROOT / "LICENSE").read_bytes(),
    }
    directories = {"."}
    for name in files:
        directories.update(str(path) for path in Path(name).parents)
    verify_tar(dpkg("--fsys-tarfile", args.package), files=files, directories=directories)
    print(f"{expected_name}: control and descriptor-only payload verified (no installation)")


if __name__ == "__main__":
    main()
