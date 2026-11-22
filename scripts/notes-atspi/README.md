# Isolated Notes AT-SPI baseline check

Run only on a disposable cloud runner. The harness creates synthetic notes and
a GTK positive control in a private D-Bus session and Xvfb display. The GTK
callback must leave an independent receipt before any conclusion about JWM.
The negative baseline additionally requires a visible, focused Notes X11
window with the exact synthetic title and PID. A missing or broken desktop,
crashed Notes process, or non-working GTK action is not a negative baseline.

The optional working mode for a future adapter verifies two named controls
and invokes the second note from AT-SPI. It also verifies a change to the
actual window title on the same X11 window plus its X focus and the editor's
semantic focus. This does not exercise text entry, native Wayland, Orca
speech, or other JWM applications.

The baseline job runs only on its isolated draft target and exact same-repo
head. It changes no product code. Do not merge this draft into master.

## MVP comparison workflow

`notes-atspi-mvp.yml` is a same-repository pull-request check, separate from
the isolated historical baseline workflow. It uses Rust 1.92.0 and locked
builds. The exact product head and the pinned uninstrumented master commit
`0631f2639676e6dc1cc4d1c81cdad8d0a2804e17` are checked out without retaining
credentials. Three binaries run in independent private D-Bus/Xvfb sessions
with fresh synthetic data and a real GTK callback receipt each time:

| binary | required outcome |
| --- | --- |
| Pinned baseline | `notes-no-atspi-application-for-pid` |
| Candidate with `--no-default-features` | `notes-no-atspi-application-for-pid` |
| Candidate with `--no-default-features --features accessibility` | `notes-named-controls-action-and-focus-observed` |

The positive contract is `搜索备忘录` (Entry/Text), the initially unfiltered
`JWM A11y Z target` row (PushButton/ListItem with one supported default
action), and `备忘录正文` (Entry/Text/DocumentText). Discovery alone is not
success. The probe invokes the row only through AT-SPI, then checks the same
X window's new exact title, X focus, and semantic editor focus. A failed GTK
control makes the run fail, not count as a negative baseline. The gate does
not attest to search entry, editor text ranges, speech, or Wayland.

The conflict arm opens a synthetic external file and independently replaces it.
It discovers the named modal and its three buttons, invokes Cancel and then
Keep Both through AT-SPI, verifies that the underlying editor was not exposed
while the modal was active, and checks the recovered sibling's exact bytes and
editor focus without altering the external file. No synthesized speech or
real personal documents are involved.
