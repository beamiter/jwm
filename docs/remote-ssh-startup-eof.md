# SSH startup diagnostic EOF

During `jwm-remote ssh` startup, an SSH process (or a user-configured wrapper)
can close its diagnostic stream before the remote host announces readiness,
without exiting itself. EOF therefore must not trigger a blocking process wait.

The readiness loop now checks the exit status without blocking. If SSH has
already exited, its existing exit-code-specific diagnostic is preserved. If it
is still alive, startup fails with `ConnectionAborted` and returns to the
existing shutdown path, which closes stdin, grants a five-second grace period,
and then kills and reaps the direct SSH process when necessary.

This change does not extend the startup timeout or grant new remote authority.
It does not guarantee cleanup of arbitrary descendants of a custom SSH wrapper,
and does not change the diagnostic relay's stream lifetime.

## Regression coverage

The first regression launches a real shell that closes stderr and remains alive.
Readiness must fail while that process is still running rather than waiting for
its eventual exit. The second checks that a process already exited with SSH's
255 status retains its connection/login error message. These are local process
lifecycle tests, not physical two-machine or SSH-server acceptance tests.
