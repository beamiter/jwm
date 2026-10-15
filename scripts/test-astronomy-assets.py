#!/usr/bin/env python3
"""Exercise real release packaging and asset upgrade/rollback in private DESTDIRs.
Uses placeholder binaries; does not build/install software or contact a network.
"""
import hashlib
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ASSET = Path("share/jwm/astronomy/earth-nasa-january.jpg")
SHA256 = "99f5faad74efe985fbf1714c8be7296ca9999759a1215b65f99b7f1df278dde5"


def run(*args):
    subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


with tempfile.TemporaryDirectory(prefix="jwm-astronomy-assets-") as tmp:
    base = Path(tmp)
    target = base / "target/release"
    target.mkdir(parents=True)
    manifest = (ROOT / "packaging/release-manifest.tsv").read_text()
    for line in manifest.splitlines():
        if line.startswith("built\t"):
            binary = target / line.split("\t")[1]
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
    run("bash", str(ROOT / "scripts/package-release.sh"), "--no-build",
        "--target-dir", str(target.parent), "--output-dir", str(base / "dist"),
        "--version", "astronomy-2", "--source-date-epoch", "0")
    archive = next((base / "dist").glob("*.tar.gz"))
    with tarfile.open(archive) as tar:
        tar.extractall(base / "unpacked", filter="data")
    bundle = next((base / "unpacked").iterdir())
    payload = bundle / "payload/usr/local/lib/jwm/versions/astronomy-2"
    assert hashlib.sha256((payload / ASSET).read_bytes()).hexdigest() == SHA256
    assert (payload / ASSET.with_name("README.md")).is_file()
    assert (payload / ASSET).stat().st_mode & 0o777 == 0o644
    assert (payload / ASSET.with_name("README.md")).stat().st_mode & 0o777 == 0o644

    # A pre-assets release has the original stable paths. Upgrade must remain
    # compatible with it; rollback must never leave a new executable paired
    # with a previous release's texture via an unversioned global asset link.
    old = base / "old"
    shutil.copytree(bundle, old)
    old_payload = old / "payload/usr/local/lib/jwm/versions/astronomy-2"
    old_payload.rename(old_payload.with_name("astronomy-1"))
    shutil.rmtree(old_payload.with_name("astronomy-1") / "share/jwm")
    (old / "VERSION").write_text("astronomy-1\n")
    (old / "release-manifest.tsv").write_text("\n".join(
        line for line in manifest.splitlines() if "assets/astronomy/" not in line) + "\n")
    root = base / "root"
    user_override = root / "home/test/.local/share/jwm/astronomy/earth-nasa-january.jpg"
    user_override.parent.mkdir(parents=True)
    user_override.write_text("user override remains untouched")
    installer = str(bundle / "install-release.sh")
    run("bash", str(old / "install-release.sh"), "install", "--destdir", str(root))
    run("bash", installer, "install", "--destdir", str(root))
    live = root / "usr/local/bin/jwm"
    assert live.resolve().parent.parent.name == "astronomy-2"
    live_asset = live.resolve().parent.parent / ASSET
    assert hashlib.sha256(live_asset.read_bytes()).hexdigest() == SHA256
    run("bash", installer, "rollback", "--destdir", str(root))
    assert live.resolve().parent.parent.name == "astronomy-1"
    assert not (live.resolve().parent.parent / ASSET).exists()
    assert live_asset.exists(), "rollback retains inactive release assets"
    run("bash", installer, "uninstall", "--all", "--destdir", str(root))
    assert not live_asset.exists()
    assert not live.exists()
    assert user_override.read_text() == "user override remains untouched"
print("test-astronomy-assets: PASS (real package hash/modes, legacy upgrade, rollback, uninstall)")
