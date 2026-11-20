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
