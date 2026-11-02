# Desktop Visual producer SDK (Python)

A dependency-free example of an **ordinary program**, unrelated to WaterLily,
rendering an input-transparent JWM desktop layer. The SDK sends RGBA images over
a versioned private Unix stream; the compositor does not load Python or producer
code. [`protocol.md`](../../crates/desktop_visual/protocol.md) is the wire contract.

## Try the plasma example

Enable the consumer explicitly when launching a composited X11 JWM session:

```sh
JWM_DESKTOP_VISUAL_ENABLED=1 jwm --backend x11rb
```

In that session, run:

```sh
python3 extensions/desktop_visual/plasma.py --fps 20 --size 320
# Repeatable, finite smoke run:
python3 extensions/desktop_visual/plasma.py --frames 60 --duration 10
```

The default socket is `$XDG_RUNTIME_DIR/jwm-desktop-visual/visual.sock`.
`JWM_DESKTOP_VISUAL_SOCKET` or the example's `--socket` selects an explicit socket.
There is no insecure shared `/tmp` fallback. The server creates/owns its private
runtime directory and socket; producers never unlink them. Stop with Ctrl+C.
A disconnect removes the producer's visible layer; stopping this example never
kills JWM or other producers. Consumer availability is backend-specific; an
absent socket is not an instruction to start/reconfigure the compositor.

The transparent moving plasma is generated entirely by Python math. It does not
read desktop pixels, pointer positions, keyboard input, files, or network services.
No GPU or Julia runtime is needed in the producer. A CPU-only protocol test does
not establish that the GPU compositor rendered or presented a correct image.

## Integrate your renderer

```python
from desktop_visual import Alpha, Publisher

with Publisher(timeout=0.5) as output:
    caps = output.capabilities
    # Your image: top-to-bottom rows, left-to-right pixels, 8-bit R,G,B,A.
    output.publish(bytes([255, 64, 0, 128]) * (160 * 90), 160, 90,
                   alpha=Alpha.STRAIGHT)
```

Put this directory on `PYTHONPATH` or import the standalone `desktop_visual.py`
module in your application's packaging. The SDK uses only the standard library.
The source dimensions may differ from the output canvas: v1 stretches the full
image over the virtual desktop. It does not expose windows, per-output placement,
input forwarding, scene capture, arbitrary shaders, or executable extensions.

The consumer's hello advertises output canvas size (1..65535 per axis), limits,
pixel formats, and alpha capabilities. Source textures remain bounded to 4096
per axis and the negotiated byte budget. The SDK validates all fields before publishing. Opaque,
straight, and premultiplied alpha are distinct contracts; set the one matching
your bytes. sRGB RGBA8, top-left origin, positive dimensions, bounded stride and
payload, and increasing per-connection sequences are mandatory. Padding at row
ends is permitted. Frame pixels are never interpreted as a command.

`publish()` performs bounded synchronous I/O **in the producer process**, without
an unbounded queue. Kernel backpressure and the timeout prevent an indefinitely
stalled receiver from holding the producer. The consumer retains only its newest
complete frame. Successful write is not a presentation acknowledgment; intermediate
frames may be dropped. A timeout, interruption, or failed write closes the entire
connection because a partial frame cannot safely be retried on it.

Reconnect explicitly with `connect()` after transport failure, read the fresh
hello, and redraw. Output resize causes a disconnect and a new hello; frame
sequences restart for each connection. `plasma.py` demonstrates bounded retry
backoff, resizing, Ctrl+C cleanup, and optional frame/time limits. It does not
silently retry an incompatible protocol or missing alpha capability. Use one
publisher from one thread; serialize your own application producers if needed.

## Tests

```sh
python3 -m unittest discover -s extensions/desktop_visual -p 'test_*.py' -v
```

Tests cover exact header vectors, malformed hello data, negotiated limits/alpha,
padding, sequences, output-size renegotiation, partial/stalled writes, slow hello
streams, abrupt shutdown, private endpoint ownership, repeated close, and a finite
non-WaterLily subprocess. They require Unix-domain sockets but no display server.

On a runner that explicitly forbids all Unix sockets, use
`DESKTOP_VISUAL_SKIP_SOCKETS=1` with the test command to run wire, pure-art, and
in-memory I/O-fault tests only. The native IPC/subprocess tests then report
**skipped**, not passed; a socket-capable runner must still execute the default
command before claiming native end-to-end validation.
