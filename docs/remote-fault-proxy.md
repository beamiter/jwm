# User-space remote transport fault fixture

`scripts/remote-fault-proxy.py` is a Python 3.11+ standard-library TCP proxy for
repeatable weak-network and cancellation experiments. It changes no system
network settings, needs no privileges, and defaults to an ephemeral loopback
listener. Its upstream host and port must be supplied explicitly. Use a
controlled local upstream, never a production desktop, for fault tests.

This is a fixture, **not an achieved end-to-end benchmark**, a workload replay
system, or evidence of remote-control latency/throughput improvement. It forwards
opaque bytes and neither terminates nor adds authentication or encryption.
Keep the listener on loopback unless intentionally exposing the upstream service.

## Run

First start the upstream service on a dedicated local port, then run:

```sh
python3 scripts/remote-fault-proxy.py \
  --upstream-host 127.0.0.1 --upstream-port 5901 \
  --listen-port 5902 \
  --c2s-latency-ms 20 --s2c-latency-ms 20 \
  --c2s-bytes-per-second 131072 --s2c-bytes-per-second 1048576
```

Point the client at `127.0.0.1:5902`. The first stdout line is versioned JSON
containing the bound address and complete configuration; keep it with test
artifacts. Port zero lets the OS select a free port. The tool writes no captures
or session contents to disk. SIGINT/SIGTERM stop accepting and cancel all active
connections, including ones paused indefinitely or blocked on writes.

Both directions have these independent options; `c2s` means client to upstream,
`s2c` means upstream to client:

- `--c2s-latency-ms N`: delay each chunk before forwarding by N milliseconds.
- `--c2s-bytes-per-second N`: add `chunk_length / N` seconds before every write.
  Zero disables rate limiting; there is no accumulating token bucket or burst
  credit. Latency and bandwidth delays add together.
- `--c2s-pause-after N`: after exactly N forwarded bytes, stop reading that
  direction. Zero pauses before the first read. The reverse direction continues.
- `--c2s-pause-seconds N`: resume after N seconds, once per connection. Default
  zero pauses indefinitely, until shutdown, peer error, or session deadline.
- `--c2s-disconnect-after N`: close both directions after exactly N bytes have
  been passed to the destination socket. Zero disconnects immediately. Disconnect
  takes precedence over pause at the same boundary. Closing unread TCP data may
  produce a reset; successful socket writes do not guarantee peer receipt.

Use the equivalent `--s2c-*` options for the other direction. Byte counters reset
for every new connection. A pause or disconnect beyond EOF never fires. EOF is
forwarded as a TCP half-close so the reverse response can still finish.

For a deliberately stalled reader:

```sh
python3 scripts/remote-fault-proxy.py \
  --upstream-host 127.0.0.1 --upstream-port 5901 \
  --listen-port 5902 --s2c-pause-after 0 --session-seconds 10
```

`--session-seconds` is an optional whole-session forwarding deadline (zero means
none), measured after upstream connection setup. `--connect-timeout` bounds
upstream lookup/connection awaits (default five seconds). Prefer numeric IPs for
fully local reproduction: an OS DNS resolver running in Python's executor can
outlive cancellation and delay interpreter exit. A refused or timed-out upstream
closes only that client connection.

## Resource bounds and modeling limits

The proxy uses nonblocking raw sockets, not asyncio stream prefetch buffers.
Each direction holds at most one `--chunk-bytes` payload (default 16 KiB, maximum
1 MiB) and does not read again until its write finishes. The default maximum is
16 simultaneous sessions (`--max-connections`, maximum 1024); excess accepted
connections are closed. Thus in-flight Python payload storage is bounded by
`2 * chunk_bytes * max_connections`, plus fixed socket/task/runtime overhead.
Kernel TCP buffers and OS resolver storage are outside that payload bound.

TCP buffers can absorb data even when the proxy stops reading. Test a large
enough workload to exercise actual backpressure. The chunk size and TCP read
segmentation affect latency; this is **per-chunk serialization delay**, not an
accurate simulation of propagation latency, packet loss/reordering, jitter,
congestion control, or a precise network bandwidth cap. Remote-control framing
is not interpreted, and a disconnect may split a record. Record chunk size and
all parameters alongside any measurement.

## Deterministic regression gate

```sh
python3 scripts/test-remote-fault-proxy.py
```

The fast suite checks exact bytes in both directions over local TCP, TCP
half-close, exact pause/disconnect boundaries, deterministic requested delay
accounting via an injected clock, no read-ahead under a blocked write, and
bounded cancellation/shutdown. It makes no wall-clock throughput assertions.
Local TCP sockets are required; a sandbox denial is a failure, not a passing
network test. Run this command in CI alongside the Rust remote protocol tests.
It does not require X11 or access to any remote machine.

For performance evidence, separately replay identical documented input, scroll,
resize, clipboard/file-transfer, and idle workloads with and without the proxy.
Collect host stage statistics and viewer presentation measurements, test
connection loss and reconnect behavior, and run the planned resource soak on
representative hardware. See `remote-control-optimization.md` for the remaining
validation requirements. Passing this fixture's tests establishes none of those
end-to-end outcomes.
