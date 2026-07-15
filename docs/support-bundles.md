# Support bundles

`jwm-support` produces a versioned JSON report that can be attached to an issue
without manually copying unrelated logs or environment dumps. It combines the
read-only startup doctor with optional live IPC health and capability queries.

## Create a report

```bash
# Inspect the default x11rb setup and a running instance.
jwm-support --output jwm-support.json

# Inspect the direct DRM/KMS Wayland setup.
jwm-support --backend wayland-udev --output jwm-support.json

# Run only filesystem, configuration, and environment preflight checks.
jwm-support --backend wayland-udev --offline --output jwm-support.json

# Useful in automation: doctor errors or failed live probes exit with status 2.
jwm-support --strict --compact --output jwm-support.json
```

When `--output` is omitted, JSON is written to stdout. File output is written to
a temporary sibling first, flushed, and atomically renamed with mode `0600`.
The destination directory must already exist.

## Report schema

Schema version 1 contains:

- generator name and JWM package version;
- generation time and requested backend;
- operating-system, architecture, kernel-release, and selected `/etc/os-release`
  fields;
- a small allowlist of desktop-session variables;
- the versioned `DoctorReport` used by `jwm --doctor --json`;
- optional `get_status` and `get_capabilities` IPC response data.

The live queries have a two-second read/write timeout and a four-megabyte
response limit. A stopped compositor therefore produces a useful report rather
than leaving the command blocked indefinitely.

## Privacy boundary

The collector deliberately excludes:

- `HOME`, `PATH`, and other user paths;
- D-Bus addresses and authentication material;
- process command lines;
- window titles and application content;
- all environment variables outside the documented allowlist.

Allowed values are stripped of control characters and limited to 256
characters. The current allowlist is:

```text
DISPLAY
WAYLAND_DISPLAY
XDG_SESSION_TYPE
XDG_CURRENT_DESKTOP
XDG_SESSION_DESKTOP
DESKTOP_SESSION
```

The selected `/etc/os-release` keys are `NAME`, `PRETTY_NAME`, `ID`, `ID_LIKE`,
`VERSION`, and `VERSION_ID`.

A support bundle is still diagnostic data. Review it before publishing it in a
public issue, especially on machines with unusual display names or custom
desktop-session identifiers.
