#!/usr/bin/env python3
"""Exercise native Cairo windows on private Xvfb and D-Bus sessions.

Requires Xvfb, xdotool, ImageMagick and Pillow. Never uses the user's display,
session bus, notes, or application defaults. Build the apps before running.
"""
import argparse
import io
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/native-apps/debug")
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--under-bus", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not args.under_bus:
        return subprocess.call(["dbus-run-session", "--", sys.executable, __file__,
                                *sys.argv[1:], "--under-bus"])
    from PIL import Image

    for tool in ("Xvfb", "xdotool", "import"):
        if not shutil.which(tool):
            raise SystemExit(f"Required tool unavailable: {tool}")
    binary_dir = args.bin_dir.resolve()
    processes = []
    with tempfile.TemporaryDirectory(prefix="jwm-native-ui-") as temporary:
        work = Path(temporary)
        with (work / "apps.log").open("w+") as log:
            xvfb = subprocess.Popen(["Xvfb", "-displayfd", "1", "-screen", "0", "1280x900x24"],
                                    stdout=subprocess.PIPE, stderr=log)
            processes.append(xvfb)
            display = ":" + xvfb.stdout.readline().decode().strip()
            xvfb.stdout.close()
            runtime = work / "runtime"
            runtime.mkdir(mode=0o700)
            env = dict(os.environ, DISPLAY=display, HOME=str(work), XDG_RUNTIME_DIR=str(runtime),
                       XDG_DATA_HOME=str(work / "data"), XDG_CONFIG_HOME=str(work / "config"),
                       XDG_CACHE_HOME=str(work / "cache"), GIO_USE_VFS="local")
            for name in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET"):
                env.pop(name, None)

            def command(*argv):
                return subprocess.check_output(list(map(str, argv)), env=env, stderr=subprocess.DEVNULL).decode().strip()

            def wait(predicate, message):
                deadline = time.monotonic() + 8
                while time.monotonic() < deadline:
                    try:
                        value = predicate()
                        if value:
                            return value
                    except (subprocess.CalledProcessError, FileNotFoundError):
                        pass
                    time.sleep(.05)
                log.flush()
                if args.evidence_dir:
                    args.evidence_dir.mkdir(parents=True, exist_ok=True)
                    subprocess.run(["import", "-window", "root", str(args.evidence_dir / "failure.png")], env=env)
                saved = {str(path.relative_to(work)): path.read_text() for path in work.glob("data/jwm/notes/*.txt")}
                raise AssertionError(message + "\nSaved: " + repr(saved) + "\n" + (work / "apps.log").read_text())

            def launch(name, *files):
                process = subprocess.Popen([str(binary_dir / name), "--", *map(str, files)],
                                           env=env, stdout=log, stderr=log)
                processes.append(process)
                return process

            def window(name):
                def find():
                    # Winit publishes the Unicode title in _NET_WM_NAME;
                    # xdotool's legacy WM_NAME search interprets it as Latin-1.
                    ids = command("xdotool", "search", "--onlyvisible", "--class", "org.jwm").splitlines()
                    return next((identifier for identifier in ids
                                 if re.search(name, command("xprop", "-id", identifier, "_NET_WM_NAME"))), None)
                return wait(find, f"Window did not appear: {name}")

            def focus(identifier):
                command("xdotool", "windowfocus", "--sync", identifier)
                time.sleep(.3)

            def key(*keys):
                command("xdotool", "key", "--clearmodifiers", *keys)
                time.sleep(.15)

            def type_text(text):
                for index, line in enumerate(text.split("\n")):
                    if index:
                        key("Return")
                    command("xdotool", "type", "--clearmodifiers", "--delay", "1", line)

            def screenshot(identifier):
                result = subprocess.check_output(["import", "-window", str(identifier), "png:-"], env=env)
                return Image.open(io.BytesIO(result)).convert("RGB")

            def close_button(identifier):
                image = screenshot(identifier)
                command("xdotool", "mousemove", "--window", identifier, image.width - 34, 32,
                        "click", "1")
                time.sleep(.15)

            def color_bounds(identifier):
                image = screenshot(identifier)
                # Red/green test images have high saturation; the neutral chrome
                # and antialiased text are neutral. Inspect actual rendered pixels.
                points = [(x, y) for y in range(image.height) for x in range(image.width)
                          if max(image.getpixel((x, y))) - min(image.getpixel((x, y))) > 150]
                assert points, "Image pixels were not rendered"
                return max(x for x, _ in points) - min(x for x, _ in points) + 1, \
                    max(y for _, y in points) - min(y for _, y in points) + 1

            try:
                notebook = launch("jwm-notes")
                notes_window = window("JWM 备忘录")
                focus(notes_window)
                type_text("Shopping\nmilk")
                notes_dir = work / "data/jwm/notes"
                wait(lambda: any(path.read_text() == "Shopping\nmilk" for path in notes_dir.glob("*.txt")),
                     "Notebook did not automatically save")
                key("ctrl+a", "ctrl+c", "ctrl+End", "Return", "ctrl+v")
                wait(lambda: any(path.read_text() == "Shopping\nmilk\nShopping\nmilk" for path in notes_dir.glob("*.txt")),
                     "Clipboard copy/paste failed")
                key("ctrl+z", "ctrl+z")
                wait(lambda: any(path.read_text() == "Shopping\nmilk" for path in notes_dir.glob("*.txt")),
                     "Undo did not restore the note")
                # Desktop activation must focus the existing notebook.
                activated = launch("jwm-notes")
                wait(lambda: activated.poll() == 0, "Repeated activation did not reuse the app")
                assert len(command("xdotool", "search", "--onlyvisible", "--class", "org.jwm.Notes").splitlines()) == 1
                key("ctrl+n")
                type_text("Second note")
                wait(lambda: len(list(notes_dir.glob("*.txt"))) == 2 and
                     any(path.read_text() == "Second note" for path in notes_dir.glob("*.txt")),
                     "New note did not save independently")
                if args.evidence_dir:
                    args.evidence_dir.mkdir(parents=True, exist_ok=True)
                    screenshot(notes_window).save(args.evidence_dir / "notes.png")
                close_button(notes_window)
                wait(lambda: notebook.poll() is not None, "Notebook did not exit after closing")
                notebook = launch("jwm-notes")
                notes_window = window("Second note.*JWM 备忘录")
                focus(notes_window)
                key("ctrl+w")
                wait(lambda: notebook.poll() is not None, "Reopened notebook did not close")
                print("Notebook autosave, independent notes and restart persistence: PASS", flush=True)

                external = work / 'file with spaces.txt'
                external.write_bytes(b"\xef\xbb\xbfOriginal\r\n")
                editor = launch("jwm-notes", external)
                edit_window = window("file with spaces.txt.*JWM 备忘录")
                focus(edit_window)
                key("ctrl+End")
                type_text("Appended")
                assert external.read_bytes() == b"\xef\xbb\xbfOriginal\r\n", "External file autosaved unexpectedly"
                key("ctrl+s")
                wait(lambda: external.read_bytes() == b"\xef\xbb\xbfOriginal\r\nAppended",
                     "External UTF-8 BOM/CRLF save failed")
                # Replace the file to guarantee an etag change even on a
                # filesystem with coarse modification timestamps.
                replacement = work / "replacement.txt"
                replacement.write_text("Changed by another editor")
                replacement.replace(external)
                type_text("Conflict")
                key("ctrl+s")
                time.sleep(.2)
                assert external.read_text() == "Changed by another editor", "Save overwrote an external change"
                assert editor.poll() is None, "Conflict killed the editor"
                key("Return")  # Dismiss save error.
                close_button(edit_window)
                time.sleep(.2)
                assert editor.poll() is None, "Header close discarded unsaved edits"
                key("Escape")  # Cancel the unsaved-changes prompt.
                assert external.read_text() == "Changed by another editor", "Header close overwrote the conflict"
                print("Explicit text save, encoding preservation and external conflict: PASS", flush=True)
                # Export through the actual self-drawn path picker. A target
                # that exists must remain untouched until explicit confirmation.
                existing = work / "existing copy.txt"
                existing.write_text("Keep this file")
                key("ctrl+shift+s")
                type_text(str(existing))
                key("Return")
                assert existing.read_text() == "Keep this file", "Save-as overwrote without confirmation"
                key("Escape")
                exported = work / "export with spaces.txt"
                key("ctrl+shift+s")
                type_text(str(exported))
                key("Return")
                wait(lambda: exported.exists() and exported.read_bytes() == b"\xef\xbb\xbfOriginal\r\nAppendedConflict",
                     "Self-drawn save-as did not preserve content and encoding")
                assert external.read_text() == "Changed by another editor"
                key("ctrl+w")
                wait(lambda: editor.poll() is not None, "Saved editor did not close")
                print("Clipboard, undo, desktop activation, file picker and overwrite confirmation: PASS", flush=True)

                # Solid asymmetric images make fit, zoom and rotation measurable.
                first, second = work / "01 red.png", work / "02 green.webp"
                Image.new("RGB", (400, 200), (240, 20, 20)).save(first)
                Image.new("RGB", (200, 400), (20, 240, 20)).save(second)
                viewer = launch("jwm-image-viewer", first)
                image_window = window("01 red.png.*JWM 图片查看器")
                focus(image_window)
                wait(lambda: color_bounds(image_window)[0] > 400, "Fit-to-window did not render")
                key("1")
                wait(lambda: color_bounds(image_window) == (400, 200), "Original-size view is wrong")
                key("r")
                wait(lambda: color_bounds(image_window) == (200, 400), "Rotation did not swap image dimensions")
                key("plus")
                wait(lambda: color_bounds(image_window) == (250, 500), "Zoom did not resize the image")
                key("Right")
                window("02 green.webp.*JWM 图片查看器")
                key("1")
                wait(lambda: color_bounds(image_window) == (200, 400), "WebP or directory navigation failed")
                key("Right")
                window("01 red.png.*JWM 图片查看器")
                key("f")
                if args.evidence_dir:
                    time.sleep(.2)
                    screenshot(image_window).save(args.evidence_dir / "image-viewer.png")
                close_button(image_window)
                wait(lambda: viewer.poll() is not None, "Viewer did not exit after closing")
                print("Image rendering, actual size, rotation, zoom, WebP and directory wrap: PASS", flush=True)
                log.flush()
                contents = (work / "apps.log").read_text()
                assert all(error not in contents for error in
                           ("panicked", "CRITICAL", "Theme parser error")), contents
            finally:
                for process in reversed(processes):
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(3)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(3)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
