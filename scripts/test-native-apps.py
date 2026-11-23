#!/usr/bin/env python3
"""Exercise native Cairo windows on private Xvfb and D-Bus sessions.

Requires Xvfb, xdotool, xprop (x11-utils), ImageMagick, D-Bus and Pillow. Never
uses the user's display, session bus, notes, or application defaults. Build the
apps before running.
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


def require_tools():
    # Check before starting any session: a missing probe must not look like an
    # application that never created its window.
    for tool in ("dbus-run-session", "Xvfb", "xdotool", "xprop", "import"):
        if not shutil.which(tool):
            raise SystemExit(f"Required tool unavailable: {tool}")


def find_window(command, name):
    # Winit publishes the Unicode title in _NET_WM_NAME;
    # xdotool's legacy WM_NAME search interprets it as Latin-1.
    ids = command("xdotool", "search", "--onlyvisible", "--class", "org.jwm").splitlines()
    return next((identifier for identifier in ids
                 if re.search(name, command("xprop", "-id", identifier, "_NET_WM_NAME"))), None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/native-apps/debug")
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--under-bus", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    require_tools()
    if not args.under_bus:
        return subprocess.call(["dbus-run-session", "--", sys.executable, __file__,
                                *sys.argv[1:], "--under-bus"])
    from PIL import Image

    binary_dir = args.bin_dir.resolve()
    processes = []
    with tempfile.TemporaryDirectory(prefix="jwm-native-ui-") as temporary:
        work = Path(temporary)
        with (work / "apps.log").open("w+") as log:
            # Readiness is reported once, then its pipe is closed. Keep the
            # server alive across gaps between short-lived probe clients.
            xvfb = subprocess.Popen(["Xvfb", "-displayfd", "1", "-noreset", "-nolisten", "tcp",
                                     "-screen", "0", "1280x900x24"],
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

            def command(*argv, timeout=None):
                return subprocess.check_output(list(map(str, argv)), env=env,
                                               stderr=subprocess.PIPE, timeout=timeout).decode().strip()

            def wait(predicate, message):
                deadline = time.monotonic() + 8
                last_probe_error = None
                while time.monotonic() < deadline:
                    try:
                        value = predicate()
                        if value:
                            return value
                    except subprocess.CalledProcessError as error:
                        last_probe_error = (f"{error.cmd!r} exited {error.returncode}: "
                                            f"{(error.stderr or b'').decode(errors='replace').strip()}")
                    time.sleep(.05)
                log.flush()
                if args.evidence_dir:
                    args.evidence_dir.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(work / "apps.log", args.evidence_dir / "apps.log")
                    subprocess.run(["import", "-window", "root", str(args.evidence_dir / "failure.png")], env=env)
                saved = {str(path.relative_to(work)): path.read_text() for path in work.glob("data/jwm/notes/*.txt")}
                owned = [(process.pid, process.poll(), Path(process.args[0]).name)
                         for process in processes[-12:]]
                try:
                    tree = command("xwininfo", "-root", "-tree", timeout=2)
                except (OSError, subprocess.SubprocessError) as error:
                    tree = repr(error)
                raise AssertionError(message + "\nLast probe error: " + str(last_probe_error)
                                     + "\nRecent owned processes: " + repr(owned)
                                     + "\nPrivate X11 window tree:\n" + tree[-6000:]
                                     + "\nSaved: " + repr(saved) + "\n" + (work / "apps.log").read_text())

            def launch(name, *files):
                process = subprocess.Popen([str(binary_dir / name), "--", *map(str, files)],
                                           env=env, stdout=log, stderr=log)
                processes.append(process)
                return process

            def window(name):
                return wait(lambda: find_window(command, name), f"Window did not appear: {name}")

            def activate_note(path, title, primary):
                activation = launch("jwm-notes", path)
                wait(lambda: activation.poll() is not None, "Secondary Notes activation did not exit")
                assert activation.returncode == 0, f"Secondary Notes activation exited {activation.returncode}"
                assert primary.poll() is None, "Primary Notes host exited during secondary activation"
                return window(title)

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

            def screenshot(identifier, *, timeout=None):
                result = subprocess.check_output(["import", "-window", str(identifier), "png:-"],
                                                 env=env, timeout=timeout)
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

                # Mixed CRLF/LF must survive real End/Delete/Backspace/Enter events.
                mixed = work / "mixed-newlines.txt"
                mixed.write_bytes(b"\xef\xbb\xbfa\r\nb\nc")
                mixed_app = launch("jwm-notes", mixed)
                mixed_window = window("mixed-newlines.txt.*JWM 备忘录")
                focus(mixed_window)
                mixed_trace = []

                def acknowledge_title(step, file, xid, dirty):
                    title = ("• " if dirty else "") + file.name + " — JWM 备忘录"
                    expected = f'_NET_WM_NAME(UTF8_STRING) = "{title}"'
                    last = None

                    def observed():
                        nonlocal last
                        try:
                            actual = command("xprop", "-id", xid, "_NET_WM_NAME", timeout=1)
                        except subprocess.TimeoutExpired:
                            actual = "<title probe timed out>"
                        if actual != last:
                            mixed_trace.append(f"{time.monotonic_ns()} {step}: title={actual!r}")
                            last = actual
                        return actual == expected

                    try:
                        wait(observed, f"{step}: window {xid} did not publish title {title!r}")
                    except AssertionError:
                        record_mixed_failure(file, xid)
                        raise

                def record_mixed_failure(file, xid):
                    if not args.evidence_dir:
                        return
                    args.evidence_dir.mkdir(parents=True, exist_ok=True)
                    detail = "\n".join(mixed_trace) + "\n"
                    if file.exists():
                        detail += f"last bytes={file.read_bytes().hex()}\n"
                    (args.evidence_dir / "mixed-newlines.log").write_text(detail)
                    log.flush()
                    shutil.copyfile(work / "apps.log", args.evidence_dir / "mixed-apps.log")
                    try:
                        screenshot(xid, timeout=3).save(args.evidence_dir / "mixed-newlines.png")
                    except (OSError, subprocess.SubprocessError) as error:
                        print(f"Mixed-newline screenshot unavailable: {error}", flush=True)

                def check_mixed(step, file, xid, expected):
                    # The title separately proves the edit and synchronous save
                    # happened. Read the exact bytes once, not until they match.
                    acknowledge_title(step + " dirty", file, xid, True)
                    key("ctrl+s")
                    acknowledge_title(step + " saved", file, xid, False)
                    got = file.read_bytes()
                    mixed_trace.append(f"{time.monotonic_ns()} {step}: "
                                       f"actual={got.hex()} expected={expected.hex()}")
                    if got != expected:
                        record_mixed_failure(file, xid)
                        raise AssertionError("\n".join(mixed_trace))

                acknowledge_title("initial clean", mixed, mixed_window, False)
                key("End")
                type_text("X")
                check_mixed("End/insert/save", mixed, mixed_window, b"\xef\xbb\xbfaX\r\nb\nc")
                key("Delete")
                check_mixed("Delete/save", mixed, mixed_window, b"\xef\xbb\xbfaXb\nc")
                key("ctrl+z", "BackSpace")
                check_mixed("Undo/Backspace/save", mixed, mixed_window, b"\xef\xbb\xbfaXb\nc")
                key("ctrl+z", "Return")
                check_mixed("Undo/Return/save", mixed, mixed_window, b"\xef\xbb\xbfaX\r\n\nb\nc")
                key("ctrl+w")
                wait(lambda: mixed_app.poll() is not None, "Mixed-newline editor did not close")

                # Independent Backspace case: the bytes before and after are
                # different here, unlike the chained Delete/Undo case above.
                control = work / "mixed-backspace.txt"
                control.write_bytes(b"\xef\xbb\xbfleft\r\nright\ntail")
                control_app = launch("jwm-notes", control)
                control_window = window("mixed-backspace.txt.*JWM 备忘录")
                focus(control_window)
                acknowledge_title("Backspace control initial clean", control, control_window, False)
                key("End", "Right", "BackSpace")
                check_mixed("Backspace control", control, control_window, b"\xef\xbb\xbfleftright\ntail")
                key("ctrl+w")
                wait(lambda: control_app.poll() is not None, "Backspace control editor did not close")
                print("Mixed newline native editing and BOM preservation: PASS", flush=True)

                external = work / "external-reload.md"
                replacement = work / "external-reload.tmp"
                external.write_text("old disk text\n", encoding="utf-8")
                external_app = launch("jwm-notes", external)
                external_window = window(r"external-reload\.md.*JWM 备忘录")
                focus(external_window)
                for trigger, text in (("key", b"new KEY disk text"), ("button", b"new BUTTON disk text")):
                    on_disk = b"\xef\xbb\xbf" + text + b"\r\nsecond\r\n"
                    replacement.write_bytes(on_disk)
                    replacement.replace(external)
                    if trigger == "key":
                        key("F5")
                    else:
                        command("xdotool", "mousemove", "--window", external_window, 180, 86,
                                "click", "1")
                    # Appended text and current disk metadata both have to come
                    # from the reload or this save cannot match the new bytes.
                    key("ctrl+Home", "End")
                    type_text(" marker")
                    check_mixed(f"external reload via {trigger}", external, external_window,
                                b"\xef\xbb\xbf" + text + b" marker\r\nsecond\r\n")
                key("ctrl+Home", "End")
                type_text(" unsaved")
                acknowledge_title("dirty before reload", external, external_window, True)
                before = external.read_bytes()
                key("F5", "Escape")
                acknowledge_title("dirty after refused reload", external, external_window, True)
                assert external.read_bytes() == before, "Refused reload wrote to disk"
                check_mixed("external unsaved text preserved", external, external_window,
                            b"\xef\xbb\xbfnew BUTTON disk text marker unsaved\r\nsecond\r\n")
                key("ctrl+w")
                wait(lambda: external_app.poll() is not None, "External reload editor did not close")
                print("External reload F5 and toolbar, format adoption and dirty buffer: PASS", flush=True)

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
                key("Escape")  # Cancel the recovery choices without writing.
                close_button(edit_window)
                time.sleep(.2)
                assert editor.poll() is None, "Header close discarded unsaved edits"
                key("Escape")  # Cancel the unsaved-changes prompt.
                assert external.read_text() == "Changed by another editor", "Header close overwrote the conflict"
                print("Explicit text save, encoding preservation and external conflict: PASS", flush=True)
                alias = work / "conflicted file alias.txt"
                alias.symlink_to(external)
                key("ctrl+shift+s")
                type_text(str(alias))
                key("Return")
                key("shift+Tab", "Return")
                assert alias.is_symlink(), "Save As replaced the symbolic link"
                assert external.read_text() == "Changed by another editor", "Alias bypassed stamped Save"
                acknowledge_title("alias refusal retained dirty original", external, edit_window, True)
                key("Return")  # Dismiss the normal outside-change warning.
                print("Save As through an alias preserves external edits and source draft: PASS", flush=True)
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

                # Confirming an overwrite must not replace a live document.
                a = work / "save-as-source.txt"
                b = work / "save-as-target.txt"
                copy = work / "save-as-fresh.txt"
                a.write_text("Source")
                b.write_text("Target")
                primary = launch("jwm-notes", a)
                a_window = window(r"save-as-source\.txt.*JWM 备忘录")
                b_window = activate_note(b, r"save-as-target\.txt.*JWM 备忘录", primary)
                assert a_window != b_window
                focus(b_window)
                key("ctrl+End")
                type_text(" dirty-B")
                acknowledge_title("target dirty before Save As", b, b_window, True)
                focus(a_window)
                key("ctrl+End")
                type_text(" dirty-A")
                acknowledge_title("source dirty before Save As", a, a_window, True)
                key("ctrl+shift+s")
                type_text(str(b))
                key("Return")
                assert b.read_text() == "Target", "Overwrite ran before confirmation"
                key("shift+Tab", "Return")  # Explicitly choose Replace.
                assert a.read_text() == "Source" and b.read_text() == "Target"
                acknowledge_title("rejected Save As retained dirty source", a, a_window, True)
                acknowledge_title("rejected Save As retained dirty target", b, b_window, True)
                key("Return")  # Dismiss the rejection.

                # Reopen B by desktop activation; it must not create a third window.
                existing_windows = set(command("xdotool", "search", "--onlyvisible",
                                               "--class", "org.jwm").splitlines())
                reopened = launch("jwm-notes", b)
                wait(lambda: reopened.poll() == 0, "Target activation did not return")
                time.sleep(.35)  # Allow the primary host to process the open request.
                assert set(command("xdotool", "search", "--onlyvisible",
                                   "--class", "org.jwm").splitlines()) == existing_windows
                focus(b_window)
                key("ctrl+s")
                wait(lambda: b.read_text() == "Target dirty-B", "Target's dirty text was not retained")
                acknowledge_title("target saved normally after refusal", b, b_window, False)
                key("ctrl+w")

                focus(a_window)
                key("ctrl+shift+s")
                type_text(str(copy))
                key("Return")
                wait(lambda: copy.exists() and copy.read_text() == "Source dirty-A",
                     "Source's dirty text was not retained for a fresh destination")
                acknowledge_title("new copy adopted normally", copy, a_window, False)
                # B is now closed. Explicit overwrite must still work.
                key("ctrl+shift+s")
                type_text(str(b))
                key("Return")
                assert b.read_text() == "Target dirty-B", "Closed target was overwritten without confirmation"
                key("shift+Tab", "Return")
                wait(lambda: b.read_text() == "Source dirty-A", "Closed target no longer accepts overwrite")
                acknowledge_title("closed target adopted after confirmation", b, a_window, False)
                key("ctrl+w")
                wait(lambda: primary.poll() is not None, "Save As proof editors did not close")
                print("Two-window Save As protects both drafts and allows closed or fresh targets: PASS",
                      flush=True)

                # Recovery cannot claim a filename held by another live editor,
                # even when that editor was opened through an alias to a leaf
                # that has since been removed from the filesystem.
                source = work / "recovery-source.md"
                reserved = work / "recovery-source (恢复副本).md"
                sibling = work / "recovery-source (恢复副本 2).md"
                alias = work / "recovery-other-editor.md"
                exported_other = work / "other-editor-draft.md"
                source.write_bytes(b"\xef\xbb\xbfsource\r\n")
                reserved.write_bytes(b"other")
                alias.symlink_to(reserved)
                primary = launch("jwm-notes", source)
                source_window = window(r"recovery-source\.md.*JWM 备忘录")
                other_window = activate_note(alias, r"recovery-other-editor\.md.*JWM 备忘录", primary)
                assert source_window != other_window
                focus(other_window)
                key("ctrl+End")
                type_text(" dirty-B")
                acknowledge_title("other editor dirty before deletion", alias, other_window, True)
                reserved.unlink()
                focus(source_window)
                key("ctrl+End")
                type_text("draft-A")
                replacement.write_bytes(b"outside edit")
                replacement.replace(source)
                key("ctrl+s", "Escape")
                acknowledge_title("cancel keeps source dirty", source, source_window, True)
                assert not reserved.exists() and not sibling.exists()
                key("ctrl+s", "shift+Tab", "Return")
                wait(lambda: sibling.exists() and sibling.read_bytes() == b"\xef\xbb\xbfsource\r\ndraft-A",
                     "Keep Both did not save the original editor draft under the next suffix")
                acknowledge_title("recovery copy adopted", sibling, source_window, False)
                assert not reserved.exists() and alias.is_symlink()
                assert source.read_bytes() == b"outside edit"
                key("ctrl+End")
                type_text(" again")
                key("ctrl+s")
                wait(lambda: sibling.read_bytes() == b"\xef\xbb\xbfsource\r\ndraft-A again",
                     "Immediate save on newly adopted recovery copy failed")
                acknowledge_title("recovery copy saved again", sibling, source_window, False)
                key("ctrl+w")

                focus(other_window)
                acknowledge_title("other editor remained dirty", alias, other_window, True)
                key("ctrl+shift+s")
                type_text(str(exported_other))
                key("Return")
                wait(lambda: exported_other.exists() and exported_other.read_bytes() == b"other dirty-B",
                     "Live editor holding the missing name lost its draft")
                assert not reserved.exists() and alias.is_symlink()
                assert source.read_bytes() == b"outside edit"
                key("ctrl+w")
                wait(lambda: primary.poll() is not None, "Recovery proof editors did not close")
                print("Keep Both preserves both drafts, skips an open missing name and saves again: PASS",
                      flush=True)

                # Real key/pointer events must not edit the Markdown preview.
                markdown = work / "preview.md"
                source = "# Preview heading\n\n**Bold content** and `code`\n\n- one\n- two\n"
                markdown.write_text(source)
                preview_app = launch("jwm-notes", markdown)
                preview_window = window("preview.md.*JWM 备忘录")
                focus(preview_window)
                before = screenshot(preview_window)
                key("ctrl+shift+p")
                after = screenshot(preview_window)
                region = (after.width // 2 + 24, 150, after.width - 24, after.height - 64)
                old_pixels = list(before.crop(region).getdata())
                new_pixels = list(after.crop(region).getdata())
                assert sum(a != b for a, b in zip(old_pixels, new_pixels)) > 100, "Markdown content was not rendered"
                command("xdotool", "mousemove", "--window", preview_window,
                        after.width * 3 // 4, 200, "click", "1")
                type_text("MUST NOT MODIFY SOURCE")
                key("ctrl+s")
                assert markdown.read_text() == source, "Preview accepted text input"
                if args.evidence_dir:
                    screenshot(preview_window).save(args.evidence_dir / "markdown-preview.png")
                key("ctrl+shift+p", "ctrl+End")
                type_text("Source edit")
                key("ctrl+s")
                wait(lambda: markdown.read_text() == source + "Source edit", "Closing preview did not restore editing")
                key("ctrl+w")
                wait(lambda: preview_app.poll() is not None, "Markdown editor did not close")
                print("Native Markdown rendering, read-only preview and source focus: PASS", flush=True)

                # Search input and match navigation must not dirty external text.
                find_path = work / "find.txt"
                find_source = "needle first\nneedle second\n"
                find_path.write_text(find_source)
                find_app = launch("jwm-notes", find_path)
                find_window_id = window("find.txt.*JWM 备忘录")
                focus(find_window_id)
                key("ctrl+f")
                type_text("needle")
                key("Return", "ctrl+s")
                assert find_path.read_text() == find_source, "Find input modified source"
                key("Escape")
                type_text("MATCH")
                key("ctrl+s")
                wait(lambda: find_path.read_text() == "needle first\nMATCH second\n",
                     "Find next/Escape did not retain the second match")
                # Typed characters are separate undo edits, not a single word.
                # Restart from an exact fixture instead of relying on undo grouping.
                key("ctrl+w")
                wait(lambda: find_app.poll() is not None, "Forward-find editor did not close")
                find_path.write_text(find_source)
                find_app = launch("jwm-notes", find_path)
                find_window_id = window("find.txt.*JWM 备忘录")
                focus(find_window_id)
                key("ctrl+Home", "ctrl+f")
                type_text("needle")
                key("shift+Return", "Escape")
                type_text("LAST")
                key("ctrl+s")
                wait(lambda: find_path.read_text() == "needle first\nLAST second\n",
                     "Find previous did not wrap to the last match")
                key("ctrl+w")
                wait(lambda: find_app.poll() is not None, "Find editor did not close")
                print("Native document find input, next/previous wrap and source focus: PASS", flush=True)

                # Ctrl+E starts at the chosen occurrence without using the clipboard.
                selection_path = work / "find-selection.txt"
                selection_source = "clipboard-sentinel\nneedle first\nneedle second\nNeedle case\n"
                selection_path.write_text(selection_source)
                selection_app = launch("jwm-notes", selection_path)
                focus(window("find-selection.txt.*JWM 备忘录"))
                key("ctrl+Home", "shift+End", "ctrl+c")
                key("ctrl+Home", "Down", "Down", "Home", *(["Right"] * 6))
                key(*(["shift+Left"] * 6), "ctrl+e", "ctrl+s")
                assert selection_path.read_text() == selection_source, "Find selection modified source"
                key("Return", "Escape", "ctrl+v", "ctrl+s")
                expected = "clipboard-sentinel\nclipboard-sentinel first\nneedle second\nNeedle case\n"
                wait(lambda: selection_path.read_text() == expected,
                     "Selection find did not wrap from the second occurrence or preserve clipboard")
                key("ctrl+w")
                wait(lambda: selection_app.poll() is not None, "Selection-find editor did not close")
                print("Native Ctrl+E selection, case-sensitive wrap and unchanged clipboard: PASS", flush=True)

                # Code view changes presentation, not source bytes or keyboard routing.
                code_path = work / "component.tsx"
                code_source = b"\xef\xbb\xbfconst view = <div>iiii WWWW</div>;\r\n"
                code_path.write_bytes(code_source)
                code_app = launch("jwm-notes", code_path)
                code_window = window("component.tsx.*JWM 备忘录")
                focus(code_window)
                before = screenshot(code_window)
                command("xdotool", "mousemove", "--window", code_window, 540, 86, "click", "1")
                after = screenshot(code_window)
                region = (16, 120, after.width - 16, 190)
                assert before.crop(region).tobytes() != after.crop(region).tobytes(), "Code view did not change source rendering"
                key("ctrl+s")
                assert code_path.read_bytes() == code_source, "Code view modified source bytes"
                key("ctrl+End")
                type_text("// appended")
                key("ctrl+s")
                wait(lambda: code_path.read_bytes() == code_source + b"// appended",
                     "Code view lost source focus or changed encoding")
                command("xdotool", "mousemove", "--window", code_window, 540, 86, "click", "1")
                key("ctrl+s", "ctrl+w")
                wait(lambda: code_app.poll() is not None, "Code-view editor did not close")
                assert code_path.read_bytes() == code_source + b"// appended"
                print("Native code-view toggle, source editing and BOM/CRLF preservation: PASS", flush=True)

                # Code indentation is one undo transaction across selected lines.
                indent_path = work / "indent.tsx"
                indent_source = b"\xef\xbb\xbf    first\r\nsecond\r\n"
                indent_path.write_bytes(indent_source)
                indent_app = launch("jwm-notes", indent_path)
                indent_window = window("indent.tsx.*JWM 备忘录")
                focus(indent_window)
                command("xdotool", "mousemove", "--window", indent_window, 540, 86, "click", "1")
                key("ctrl+a", "Tab", "ctrl+s")
                indented = b"\xef\xbb\xbf\t    first\r\n\tsecond\r\n"
                wait(lambda: indent_path.read_bytes() == indented, "Selected lines were not indented")
                key("shift+Tab", "ctrl+s")
                wait(lambda: indent_path.read_bytes() == indent_source, "Selected lines were not outdented")
                key("ctrl+z", "ctrl+s")
                wait(lambda: indent_path.read_bytes() == indented, "Outdent was not one undo transaction")
                key("ctrl+z", "ctrl+s")
                wait(lambda: indent_path.read_bytes() == indent_source, "Indent was not one undo transaction")
                key("ctrl+Home", "End", "Return")
                type_text("next")
                key("ctrl+s")
                wait(lambda: indent_path.read_bytes() == b"\xef\xbb\xbf    first\r\n    next\r\nsecond\r\n",
                     "Enter did not preserve current code indentation")
                key("ctrl+w")
                wait(lambda: indent_app.poll() is not None, "Indent editor did not close")
                print("Native code indentation, outdent, grouped undo and Enter prefix: PASS", flush=True)

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
