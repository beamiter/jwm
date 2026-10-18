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
JANUARY = "earth-nasa-january.jpg"
OCTOBER = "earth-nasa-october.jpg"
ASSETS = {
    OCTOBER: "ccfd9b7a5e5ee601e1466556716bba50ea418f3a8e068ab0194ae0425815b1b0",
    JANUARY: "99f5faad74efe985fbf1714c8be7296ca9999759a1215b65f99b7f1df278dde5",
    "moon-nasa-lroc.jpg": "f7130a1822681fa7512d7dcfd40db8c10b9ba4f06777910348698260ed7a2170",
    "moon-nasa-lroc-8k.jpg": "821831cacac483ebb5390e1c6b4039b6274f7d1f9c10b3c35259496af6b0cac0",
    "earth-nasa-clouds.jpg": "daddaad84d7a33bbbc86cdda3f591099f57cee8607b7bcf3b67eb7e4f7a1c793",
    "milkyway-nasa-gaia.jpg": "0c9247995a0f31eebe8ba49e3994d3fcc468c4cd4bf9a1555445141d81c8078e",
    "galaxy-nasa-4k.png": "f92ee3d1eff038c93200a79f0a0496edb4bb37cf397de676d7d57c1e85cc8d94",
    "mercury-nasa-messenger.jpg": "6a99105fd4bcf9649977b5f163db0303bfe07c48354bcc3f1bc69ccd9566a438",
    "mars-nasa-viking.jpg": "12ec6bf02ebd42a246edc778cb2ce8c595b4d9f0892badf0bfff8abb2303c780",
    "jupiter-nasa-cassini.jpg": "147a68a29590299a052d402cda76c29877ba5ebd4a6cab752aeb5e5166f507ec",
}
# Each historical release must have exactly the assets that existed then.
# Keep these lists explicit: deriving them from today's asset map silently
# backports newly added detail/sky textures into historical release fixtures.
HISTORICAL_ASSETS = {
    "astronomy-1": (),
    "astronomy-earth": (JANUARY,),
    "astronomy-surfaces": (JANUARY, "moon-nasa-lroc.jpg"),
    "astronomy-clouds": (JANUARY, "moon-nasa-lroc.jpg", "earth-nasa-clouds.jpg"),
    "astronomy-galaxy": (JANUARY, "moon-nasa-lroc.jpg", "earth-nasa-clouds.jpg", "milkyway-nasa-gaia.jpg"),
    "astronomy-october": (OCTOBER, JANUARY, "moon-nasa-lroc.jpg", "earth-nasa-clouds.jpg", "milkyway-nasa-gaia.jpg"),
    "astronomy-cosmos": (
        OCTOBER, JANUARY, "moon-nasa-lroc.jpg", "moon-nasa-lroc-8k.jpg",
        "earth-nasa-clouds.jpg", "milkyway-nasa-gaia.jpg", "galaxy-nasa-4k.png",
    ),
}
VERSION = "astronomy-encyclopedia"
VERSION_DIR = Path("usr/local/lib/jwm/versions")


def check_assets(payload, names=ASSETS):
    directory = payload / ASSET_DIR
    if not names:
        assert not directory.exists(), directory
        return
    assert {path.name for path in directory.iterdir()} == set(names) | {"README.md"}, directory
    for name in names:
        asset = directory / name
        assert hashlib.sha256(asset.read_bytes()).hexdigest() == ASSETS[name], asset
        assert asset.stat().st_mode & 0o777 == 0o644, asset
    credit = directory / "README.md"
    assert credit.is_file(), credit
    assert credit.stat().st_mode & 0o777 == 0o644, credit


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=90)
    assert result.returncode == 0, (args, result.stdout, result.stderr)


def make_historical_bundle(bundle, destination, manifest, version, names):
    shutil.copytree(bundle, destination)
    original = destination / "payload" / VERSION_DIR / VERSION
    payload = original.with_name(version)
    original.rename(payload)
    for name in set(ASSETS) - set(names):
        (payload / ASSET_DIR / name).unlink()
    if not names:
        shutil.rmtree(payload / "share/jwm")
    (destination / "VERSION").write_text(version + "\n")
    lines = []
    for line in manifest.splitlines():
        if "assets/astronomy/" in line:
            source_name = Path(line.split("\t")[1]).name
            if not names or source_name not in set(names) | {"README.md"}:
                continue
        lines.append(line)
    historical_manifest = "\n".join(lines) + "\n"
    (destination / "release-manifest.tsv").write_text(historical_manifest)
    historical_sources = {
        Path(line.split("\t")[1]).name
        for line in historical_manifest.splitlines()
        if line.startswith("repo-file\tassets/astronomy/")
    }
    assert historical_sources == (set(names) | {"README.md"} if names else set()), version
    check_assets(payload, names)


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
        "--version", VERSION, "--source-date-epoch", "0")
    archive = next((base / "dist").glob("*.tar.gz"))
    with tarfile.open(archive) as tar:
        tar.extractall(base / "unpacked", filter="data")
    bundle = next((base / "unpacked").iterdir())
    check_assets(bundle / "payload" / VERSION_DIR / VERSION)
    installer = str(bundle / "install-release.sh")

    for old_version, old_names in HISTORICAL_ASSETS.items():
        old = base / old_version
        make_historical_bundle(bundle, old, manifest, old_version, old_names)
        root = base / ("root-" + old_version)
        user_home = root / "home/test"
        preserved = {}
        for name in ASSETS:
            override = user_home / ".local/share/jwm/astronomy" / name
            preserved[override] = ("user override remains untouched: " + name).encode()
        # An explicit January path from the source installer remains usable;
        # release installs must neither replace it nor add global October links.
        legacy_january = root / "usr/local" / ASSET_DIR / JANUARY
        preserved[legacy_january] = (ROOT / "assets/astronomy" / JANUARY).read_bytes()
        custom_january = user_home / "wallpapers/custom-january.jpg"
        preserved[custom_january] = b"custom January texture remains untouched"
        for backend, explicit_path in (
            ("x11", "/usr/local/share/jwm/astronomy/" + JANUARY),
            ("wayland", "/home/test/wallpapers/custom-january.jpg"),
        ):
            config = user_home / ".config/jwm" / ("config_" + backend + ".toml")
            preserved[config] = (
                '[behavior]\nwallpaper_effect = "astronomy"\nwallpaper_earth_texture = "'
                + explicit_path + '"\n'
            ).encode()
        for path, contents in preserved.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)

        def check_preserved():
            for path, contents in preserved.items():
                assert path.read_bytes() == contents, (old_version, path)
            global_assets = root / "usr/local" / ASSET_DIR
            assert {path.name for path in global_assets.iterdir()} == {JANUARY}

        live = root / "usr/local/bin/jwm"
        old_payload = root / VERSION_DIR / old_version
        active_payload = root / VERSION_DIR / VERSION
        run("bash", str(old / "install-release.sh"), "install", "--destdir", str(root))
        assert live.resolve().parent.parent == old_payload
        check_assets(old_payload, old_names)
        check_preserved()

        run("bash", installer, "install", "--destdir", str(root))
        assert live.resolve().parent.parent == active_payload
        check_assets(active_payload)
        check_assets(old_payload, old_names)
        check_preserved()

        run("bash", installer, "rollback", "--destdir", str(root))
        assert live.resolve().parent.parent == old_payload
        check_assets(old_payload, old_names)
        check_assets(active_payload)  # Inactive detail/panorama assets stay version-local.
        check_preserved()

        run("bash", installer, "uninstall", "--all", "--destdir", str(root))
        assert not live.exists() and not live.is_symlink()
        assert not old_payload.exists()
        assert not active_payload.exists()
        check_preserved()

        # Also exercise fresh install and uninstall with the cosmos assets active, rather
        # than only uninstalling it as an inactive release after rollback.
        run("bash", installer, "install", "--destdir", str(root))
        assert live.resolve().parent.parent == active_payload
        check_assets(active_payload)
        check_preserved()
        run("bash", installer, "uninstall", "--all", "--destdir", str(root))
        assert not live.exists() and not live.is_symlink()
        assert not active_payload.exists()
        check_preserved()

print("test-astronomy-assets: PASS (Earth/Moon/8K/cloud/panorama/Mercury/Mars/Jupiter hashes/modes; "
      "7 historical upgrade/rollback/uninstall + fresh install/uninstall cases; "
      "user overrides and explicit January paths)")
