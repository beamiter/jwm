# Remote benchmark context capture

Run this standard-library-only helper alongside a benchmark, on both endpoints:

```sh
python3 scripts/remote-benchmark-metadata.py \
  --backend remote-x11 --display-size 1920x1080 \
  --build-label c8cf77ec-release > host-context.json
```

The helper prints one JSON object to stdout. It makes no network connections,
launches no subprocesses, requires no display connection, changes no settings,
and writes no files itself. Redirecting stdout is an explicit shell action.
Run it using Python 3.8 or newer. Test it with:

```sh
python3 scripts/test-remote-benchmark-metadata.py
```

## Fields and unknown values

- `schema_version`: currently `1`.
- `kind`: `benchmark_context_not_performance`.
- `os.family`: Python's platform family (`linux`, `darwin`, etc.). No kernel
  version, OS account information, hostname or distribution file is collected.
- `cpu.logical_count`: logical CPU count reported by the OS, or `null`. This
  can differ from CPUs available to a process under affinity/container quotas.
- `cpu.model`: first Linux `/proc/cpuinfo` `model name` or `Hardware` value,
  bounded to 160 printable ASCII characters, otherwise `null`.
- `gpu.requested`: whether `--include-gpu` was explicitly supplied.
- `gpu.pci_ids`: `null` by default. With `--include-gpu` on Linux, an array of
  PCI `vendor`, `device`, `subsystem_vendor`, and `subsystem_device` hexadecimal
  IDs for readable DRM cards. Both vendor and device must be valid to include
  a record; unknown subsystem IDs are `null`. No accessible valid cards yields
  `null`, not a claim that the machine has no GPU. Identical devices may produce
  identical records. There are no device serial numbers or PCI addresses.
- `declared`: operator-supplied `backend`, `display_size` and `build_label`.
  Unspecified values are `null`; none are inferred from running sessions,
  environment variables, Git, binaries, or build logs. The build label is not
  verified provenance. Use a public revision/tag, never a secret, username,
  hostname, IP address or other personal identifier as a label.

`null` means unavailable, invalid, outside the collection bound, unsupported,
or unspecified, depending on the field. Errors do not include filesystem paths
or raw contents. Invalid CLI arguments fail before collection. Backend choices
are a fixed allowlist, dimensions must be 1–32768 pixels, and labels must be
1–96 ASCII letters/digits or `. _ + -`, starting with a letter/digit.

## Bounds and privacy

CPU parsing reads at most 65,536 bytes from one file, emits only the allowlisted
model field, and ignores a partial final line. GPU collection is opt-in and
probes only `card0` through `card31`, four fixed numeric fields each, reading at
most 32 bytes per field. It does not enumerate arbitrary directories. File
reads are unbuffered and nonblocking, accept only regular files, and require
resolved paths to remain under `/proc` or `/sys`. Symlinked sysfs parents are
supported. Final-open symlinks and special files are refused. These are byte
and operation bounds, not hard wall-clock deadlines on kernel filesystem I/O.
The system pseudo-filesystems must be trusted and OS-managed; path resolution
is not an adversarial-filesystem sandbox.

No hostnames, usernames, IP addresses, environment dumps, secrets, process
lists, machine IDs, timestamps, monitor EDID, clipboard, display contents, or
network configuration are automatically collected. CPU model and GPU product
IDs describe hardware classes, not unique machine identity. Review the JSON
and any explicit label before sharing it. The helper is independent of the
remote protocol and is not automatically transmitted to a peer.

This is benchmark context, **not end-to-end performance evidence**. It does
not detect a driver version, renderer API, actual display configuration,
refresh rate, encoder, workload, network path, build flags or binary version.
Record those non-sensitive details separately as needed. Pair it with measured
benchmark results and the exact workload/build command; do not infer latency,
throughput or physical GPU correctness from metadata or fixture tests.
