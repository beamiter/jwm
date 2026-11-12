# TCP phase deadline accounting

TCP phase guards capture an absolute monotonic deadline before cloning the
socket and starting the helper thread. Time spent creating or scheduling that
thread is part of the phase budget. Every wake recomputes the remaining time
from the same deadline; spurious wakes cannot restart the budget.

Cancellation still wakes and joins the helper and leaves the socket usable.
An expired guard closes the socket and its clones. Mutex poisoning stays
fail-closed. A timeout that cannot be represented as an `Instant` is rejected
before starting a guard, without modifying the socket.

The existing negotiation, handshake and first-frame timeout values, wire
protocol and authentication rules are unchanged. This is scheduling accounting,
not a hard-real-time guarantee: the operating system must still schedule the
helper to execute shutdown. DNS and TCP connection attempts have their own
lifetimes. No throughput or end-to-end latency improvement is claimed.

## Validation

Unit coverage includes already-expired worker startup, cancellation before
worker startup, prompt cancellation of a long wait, overflow rejection without
socket closure, blocked-read interruption and socket usability after cancel.
Client and authenticated TCP integration regressions cover the existing users
of this guard. Tests run on Linux loopback, not a physical dual-machine network.
