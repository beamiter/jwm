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
ASSET_DIR = Path("share/jwm/astronomy")
ASSETS = {
    "milkyway-nasa-gaia.jpg": "0c9247995a0f31eebe8ba49e3994d3fcc468c4cd4bf9a1555445141d81c8078e",
    "earth-nasa-clouds.jpg": "daddaad84d7a33bbbc86cdda3f591099f57cee8607b7bcf3b67eb7e4f7a1c793",
    "earth-nasa-january.jpg": "99f5faad74efe985fbf1714c8be7296ca9999759a1215b65f99b7f1df278dde5",
    "moon-nasa-lroc.jpg": "f7130a1822681fa7512d7dcfd40db8c10b9ba4f06777910348698260ed7a2170",
}


def check_assets(payload):
    for name, digest in ASSETS.items():
        asset = payload / ASSET_DIR / name
        assert hashlib.sha256(asset.read_bytes()).hexdigest() == digest
        assert asset.stat().st_mode & 0o777 == 0o644
    credit = payload / ASSET_DIR / "README.md"
    assert credit.is_file()
    assert credit.stat().st_mode & 0o777 == 0o644


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
    check_assets(payload)

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
    user_overrides = [root / "home/test/.local/share/jwm/astronomy" / name
                      for name in ASSETS]
    for user_override in user_overrides:
        user_override.parent.mkdir(parents=True, exist_ok=True)
        user_override.write_text("user override remains untouched")
    installer = str(bundle / "install-release.sh")
    run("bash", str(old / "install-release.sh"), "install", "--destdir", str(root))
    run("bash", installer, "install", "--destdir", str(root))
    live = root / "usr/local/bin/jwm"
    assert live.resolve().parent.parent.name == "astronomy-2"
    active_payload = live.resolve().parent.parent
    live_assets = active_payload / ASSET_DIR
    check_assets(active_payload)
    run("bash", installer, "rollback", "--destdir", str(root))
    assert live.resolve().parent.parent.name == "astronomy-1"
    assert not (live.resolve().parent.parent / ASSET_DIR).exists()
    check_assets(active_payload)  # Inactive release stays intact.
    run("bash", installer, "uninstall", "--all", "--destdir", str(root))
    assert not live_assets.exists()
    assert not live.exists()
    for user_override in user_overrides:
        assert user_override.read_text() == "user override remains untouched"

    # Also preserve an Earth-only release when upgrading to Moon-capable JWM.
    earth_only = base / "earth-only"
    shutil.copytree(bundle, earth_only)
    earth_payload = earth_only / "payload/usr/local/lib/jwm/versions/astronomy-2"
    earth_payload.rename(earth_payload.with_name("astronomy-earth"))
    earth_payload = earth_payload.with_name("astronomy-earth")
    (earth_payload / ASSET_DIR / "moon-nasa-lroc.jpg").unlink()
    (earth_payload / ASSET_DIR / "earth-nasa-clouds.jpg").unlink()
    (earth_payload / ASSET_DIR / "milkyway-nasa-gaia.jpg").unlink()
    (earth_only / "VERSION").write_text("astronomy-earth\n")
    (earth_only / "release-manifest.tsv").write_text("\n".join(
        line for line in manifest.splitlines() if all(name not in line for name in ("moon-nasa-lroc.jpg", "earth-nasa-clouds.jpg", "milkyway-nasa-gaia.jpg"))) + "\n")
    run("bash", str(earth_only / "install-release.sh"), "install", "--destdir", str(root))
    run("bash", installer, "install", "--destdir", str(root))
    check_assets(live.resolve().parent.parent)
    run("bash", installer, "rollback", "--destdir", str(root))
    restored_payload = live.resolve().parent.parent
    assert restored_payload.name == "astronomy-earth"
    assert not (restored_payload / ASSET_DIR / "moon-nasa-lroc.jpg").exists()
    assert not (restored_payload / ASSET_DIR / "earth-nasa-clouds.jpg").exists()
    assert not (restored_payload / ASSET_DIR / "milkyway-nasa-gaia.jpg").exists()
    assert hashlib.sha256((restored_payload / ASSET_DIR / "earth-nasa-january.jpg").read_bytes()).hexdigest() == ASSETS["earth-nasa-january.jpg"]
    check_assets(active_payload)
    run("bash", installer, "uninstall", "--all", "--destdir", str(root))
    assert not live_assets.exists()
    for user_override in user_overrides:
        assert user_override.read_text() == "user override remains untouched"
    # A surface-texture release must regain its cloud-free payload on rollback.
    surfaces = base / "surfaces"
    shutil.copytree(bundle, surfaces)
    surface_payload = surfaces / "payload/usr/local/lib/jwm/versions/astronomy-2"
    surface_payload.rename(surface_payload.with_name("astronomy-surfaces"))
    surface_payload = surface_payload.with_name("astronomy-surfaces")
    (surface_payload / ASSET_DIR / "earth-nasa-clouds.jpg").unlink()
    (surface_payload / ASSET_DIR / "milkyway-nasa-gaia.jpg").unlink()
    (surfaces / "VERSION").write_text("astronomy-surfaces\n")
    (surfaces / "release-manifest.tsv").write_text("\n".join(
        line for line in manifest.splitlines() if "earth-nasa-clouds.jpg" not in line and "milkyway-nasa-gaia.jpg" not in line) + "\n")
    run("bash", str(surfaces / "install-release.sh"), "install", "--destdir", str(root))
    run("bash", installer, "install", "--destdir", str(root))
    check_assets(live.resolve().parent.parent)
    run("bash", installer, "rollback", "--destdir", str(root))
    restored_payload = live.resolve().parent.parent
    assert restored_payload.name == "astronomy-surfaces"
    assert not (restored_payload / ASSET_DIR / "earth-nasa-clouds.jpg").exists()
    assert not (restored_payload / ASSET_DIR / "milkyway-nasa-gaia.jpg").exists()
    for name, digest in ASSETS.items():
        if name not in ("earth-nasa-clouds.jpg", "milkyway-nasa-gaia.jpg"):
            assert hashlib.sha256((restored_payload / ASSET_DIR / name).read_bytes()).hexdigest() == digest
    check_assets(active_payload)
    run("bash", installer, "uninstall", "--all", "--destdir", str(root))
    assert not live_assets.exists()
    for user_override in user_overrides:
        assert user_override.read_text() == "user override remains untouched"
    # The published Earth/Moon/cloud release predates the optional real sky.
    clouds = base / "clouds"
    shutil.copytree(bundle, clouds)
    cloud_payload = clouds / "payload/usr/local/lib/jwm/versions/astronomy-2"
    cloud_payload.rename(cloud_payload.with_name("astronomy-clouds"))
    cloud_payload = cloud_payload.with_name("astronomy-clouds")
    (cloud_payload / ASSET_DIR / "milkyway-nasa-gaia.jpg").unlink()
    (clouds / "VERSION").write_text("astronomy-clouds\n")
    (clouds / "release-manifest.tsv").write_text("\n".join(
        line for line in manifest.splitlines() if "milkyway-nasa-gaia.jpg" not in line) + "\n")
    run("bash", str(clouds / "install-release.sh"), "install", "--destdir", str(root))
    run("bash", installer, "install", "--destdir", str(root))
    check_assets(live.resolve().parent.parent)
    run("bash", installer, "rollback", "--destdir", str(root))
    restored_payload = live.resolve().parent.parent
    assert restored_payload.name == "astronomy-clouds"
    assert not (restored_payload / ASSET_DIR / "milkyway-nasa-gaia.jpg").exists()
    for name, digest in ASSETS.items():
        if name != "milkyway-nasa-gaia.jpg":
            assert hashlib.sha256((restored_payload / ASSET_DIR / name).read_bytes()).hexdigest() == digest
    check_assets(active_payload)
    run("bash", installer, "uninstall", "--all", "--destdir", str(root))
    assert not live_assets.exists()
    for user_override in user_overrides:
        assert user_override.read_text() == "user override remains untouched"

print("test-astronomy-assets: PASS (Earth/Moon/cloud/sky hashes/modes, legacy + Earth-only + Earth/Moon + Earth/Moon/cloud upgrade, rollback, uninstall)")
