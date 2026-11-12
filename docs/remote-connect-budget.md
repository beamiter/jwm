# TCP connection attempt budget

The direct viewer applies one elapsed-time budget (currently ten seconds) to
TCP connection attempts **after hostname resolution**. Name resolution is still
synchronous and is not included in this bound. Authentication and negotiation
retain their separate deadlines and permission checks.

Resolved endpoints keep resolver order, with duplicate socket addresses removed.
Each attempt gets a fair share of the time remaining. A fast refusal leaves its
unused time for later endpoints. A slow attempt, scheduling delay or overdue
return cannot start another attempt after the overall budget is exhausted.
The former one-second per-address floor could exceed ten seconds when many
addresses timed out; it is no longer applied.

This is sequential fallback, not Happy Eyeballs, automatic session reconnect,
address-family preference, or a throughput improvement. A stale first IPv6
address can still delay an IPv4 fallback by its share of the budget. Existing
reconnect restrictions for scoped input and Wayland sessions are unchanged.

## Validation

`remote::client::connect::tests` covers a deterministic twenty-address timeout
sequence (500 ms each, ten seconds total), fast-failure budget reuse, duplicate
answers, zero/exhausted budgets, and dropping a socket returned after the overall
deadline. Loopback tests cover injected-refusal fallback to a real TCP stream and numeric-address
resolution. No external black-holed route or DNS server is required.
