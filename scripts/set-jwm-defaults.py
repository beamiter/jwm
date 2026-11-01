#!/usr/bin/env python3
"""Set only JWM's desktop-specific MIME defaults from installed desktop entries."""
import configparser
import io
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile


def read_config(text):
    config = configparser.ConfigParser(interpolation=None, delimiters=("=",))
    config.optionxform = str
    config.read_string(text)
    return config


def main():
    if len(sys.argv) < 3:
        raise SystemExit("Usage: set-jwm-defaults.py CONFIG_HOME/jwm-mimeapps.list APP.desktop …")
    destination = Path(os.path.normpath(sys.argv[1]))
    if destination.name != "jwm-mimeapps.list":
        raise SystemExit("Only jwm-mimeapps.list may be updated")
    if destination.is_symlink():
        raise SystemExit(f"Refusing to replace symlink: {destination}")
    previous = destination.read_bytes() if destination.exists() else None
    config = read_config(previous.decode("utf-8") if previous is not None else "")
    section = "Default Applications"
    if not config.has_section(section):
        config.add_section(section)
    changed = previous is None
    for filename in sys.argv[2:]:
        desktop_file = Path(filename)
        desktop = read_config(desktop_file.read_text(encoding="utf-8"))
        for mime in desktop["Desktop Entry"]["MimeType"].split(";"):
            if not mime:
                continue
            value = desktop_file.name + ";"
            if config.get(section, mime, fallback=None) != value:
                config.set(section, mime, value)
                changed = True
    if not changed:
        return
    output = io.StringIO()
    config.write(output, space_around_delimiters=False)
    destination.parent.mkdir(parents=True, exist_ok=True)
    mode = stat.S_IMODE(destination.stat().st_mode) if previous is not None else 0o644
    fd, temporary = tempfile.mkstemp(prefix=".jwm-mimeapps-", dir=destination.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(output.getvalue())
        os.chmod(temporary, mode)
        if previous is not None:
            shutil.copy2(destination, str(destination) + ".bak")
        os.replace(temporary, destination)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


if __name__ == "__main__":
    main()
