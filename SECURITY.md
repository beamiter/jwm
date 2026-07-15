# Security policy

## Supported versions

JWM is under active development and does not yet publish stable releases. The
current `master` branch is the only version receiving security fixes. Historical
commits, experimental branches, local forks, and third-party status bars are not
covered by this policy.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability, exploit, credential,
private support bundle, or sensitive crash report.

Use GitHub's private vulnerability reporting flow for this repository:

1. Open the repository's **Security** tab.
2. Choose **Advisories** and **Report a vulnerability**.
3. Include the affected commit, backend, renderer, configuration surface, and a
   minimal reproduction.

When private vulnerability reporting is unavailable, contact the repository
owner privately through the contact method on the owner's GitHub profile. Do not
attach secrets or personal desktop content to an unsolicited public discussion.

A useful report contains:

- affected commit SHA and build profile;
- X11RB, XCB, direct Wayland, or nested Wayland backend;
- whether XWayland, the portal, recording, or the supervisor is involved;
- expected and observed behavior;
- security impact and required attacker capabilities;
- minimal reproduction or proof of concept;
- relevant logs with tokens, paths, window titles, and user data removed;
- whether the issue is already public or known to another project.

`jwm-support` deliberately excludes common sensitive categories, but reporters
must still review a generated bundle before sharing it.

## Response process

The maintainer will validate scope, coordinate a fix on a private branch when
necessary, and credit the reporter unless anonymity is requested. Disclosure
timing depends on exploitability, downstream coordination, and the availability
of a tested fix.

## Security boundaries

Particular care is required around:

- Unix-socket and FIFO ownership, permissions, framing, and resource limits;
- configuration and session-file parsing, symlinks, and atomic replacement;
- process spawning and command arguments;
- X11 client trust and XWayland isolation limitations;
- DRM, input, capture, screencast, and portal permissions;
- shader, image, and media input sizes;
- unsafe FFI and driver-facing resource lifetime;
- logs and diagnostics that may expose desktop content.

JWM cannot make untrusted X11 clients mutually isolated; the X11 security model
allows clients to observe and influence other clients in the same display
session. Reports should distinguish a JWM implementation flaw from behavior
inherent to the selected display protocol.
