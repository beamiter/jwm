# Persistent host session-result lifetime

`host --once` returns its admitted session's result to its caller. A persistent
host instead logs session errors and continues serving subsequent authorized
viewers. It does not need to retain prior session results.

Previously both modes sent each completed result into an unbounded channel,
but only `--once` consumed the channel. A persistent host therefore retained
one queued result per completed session, including any owned error payload,
until shutdown.

Only `--once` workers now receive a result sender. Persistent-session outcomes
are dropped after logging, without changing authentication, the single-viewer
slot, cleanup-error poisoning, or shutdown semantics.

Regression tests use real loopback authentication and a drop-observable error:
repeated persistent sessions must release each error before the next session,
while `--once` must transfer the original error to the caller and keep it alive
until that caller drops it. This is a session-result retention fix, not a claim
that every host memory allocation or queue is bounded.
