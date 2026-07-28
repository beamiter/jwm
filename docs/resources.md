# Resource rows

The control center shows what the machine is doing: how busy the CPU is, how
much memory is in use, and how fast the network is moving.

```
  CPU                            37%
  Memory          25%  8.0 GiB / 32.0 GiB
  Network I/O      1.2 MiB/s   14 KiB/s
```

They are read-only — `Up`/`Down` move past them, `Left`/`Right` and `Enter` do
nothing on them, the same as the Battery row. Each appears only when `/proc`
answered for it, so a container without a network interface shows CPU and
memory and no third row rather than a row reading zero.

Switch the whole thing off, sampling included, with:

```toml
[behavior]
resource_rows = false
```

## These are the machine's numbers, not JWM's

The compositor's debug HUD also shows a CPU and a memory figure, and they are a
different number **on purpose**: those come from `/proc/self/*` and are JWM's
own process. Showing the compositor's 3% in a panel labelled "CPU" while a
build pins sixteen cores would be a lie, so the shell reads what `top` and
`free` read instead.

| Row | Source | Shows |
| --- | --- | --- |
| CPU | `/proc/stat`, aggregate `cpu` line | busy share since the last sample |
| Memory | `/proc/meminfo` | `MemTotal − MemAvailable`, and the total |
| Network I/O | `/proc/net/dev` | received and sent bytes per second |

Memory is `MemTotal − MemAvailable`, not `MemTotal − MemFree`. A machine with
twenty gigabytes of page cache is not nearly full, and reporting it that way
sends people hunting for a leak that is a feature.

The CPU figure counts `iowait` as idle: a machine blocked on a disk is not
busy, whatever the scheduler is doing about it.

## Why a rate can be an em dash

Memory is a **level** — it is real from the first sample. CPU and network are
**rates**, and a rate needs two samples to subtract. Until the second one
arrives the row shows `—` rather than a fabricated `0%`, which would be a
number the machine never said.

The same dash comes back, briefly, whenever a delta cannot be trusted:

- the counters went backwards, because an interface disappeared or the kernel
  reset them — that is a reset, not a negative rate;
- the gap between samples is longer than 30 seconds, after a suspend or a VT
  switch. A correct ten-minute average presented as the current rate is worse
  than admitting there is nothing to report yet.

## Which interfaces count

Throughput sums the interfaces that carry traffic off the machine — `en*`,
`eth*`, `wl*`, `ww*`, `ppp*`, `usb*` — and ignores loopback, bridges, `veth`
pairs, `docker0`, `virbr0`, tunnels, `wg*` and `tailscale*`.

That means **a VPN's bytes are counted once**, on the physical link that
carries the encapsulated traffic, instead of twice.

It is an allowlist, which fails by *hiding* the row on a machine whose NIC has
been renamed to something unusual (`lan0`). A denylist would fail the other
way, by silently counting a bridge's traffic on top of the interface beneath it
and reporting double the real rate. A row that is missing is easier to
understand than one that is wrong.

## Cost

Three small `/proc` reads every two seconds, whether or not the panel is open —
sampling has to run continuously because otherwise the first thing you would
see on opening the panel is two em dashes. The rows are only *rendered* while
the control center is up, and only their three labels are retyped: rebuilding
the panel would re-run `wpctl`, `brightnessctl` and `powerprofilesctl`, three
processes every two seconds.

The panel updates itself while you watch it — the poll pushes the new labels
into the compositor rather than waiting for the next keypress.

## Over IPC

```sh
jwm-tool msg get_resources
# {"cpu_present": true, "cpu_percent": 37,
#  "memory": {"total_kib": 32558940, "used_kib": 5287756, "percent": 16},
#  "net_present": true,
#  "throughput": {"rx_bytes_per_sec": 1258291, "tx_bytes_per_sec": 14336}}
```

Parts the machine could not answer are `null`, never zero.
