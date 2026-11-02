"""Desktop Visual v1 producer SDK (stdlib only; no desktop/input access).

One connection owns one stream. Call from a single producer thread; there is no
background queue. A successful publish means bytes were delivered, not that a
frame was presented. On transport failure reconnect, read fresh capabilities,
and render again: never replay a partially written frame on the old connection.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import IntEnum
import math
import os
from pathlib import Path
import socket
import struct
import time

VERSION = 1
HEADER_BYTES = 64
MAX_DIMENSION = 4096
MAX_OUTPUT_DIMENSION = 65535
MAX_FRAME_BYTES = 32 * 1024 * 1024
UINT64_MAX = (1 << 64) - 1
_HEADER = struct.Struct("<8s10I2Q")


class ProtocolError(ValueError):
    """A peer's wire contract is malformed or unsupported."""


class Alpha(IntEnum):
    OPAQUE = 1
    STRAIGHT = 2
    PREMULTIPLIED = 3


@dataclass(frozen=True)
class Capabilities:
    output_width: int
    output_height: int
    max_dimension: int
    max_frame_bytes: int
    pixel_formats: int
    alpha_modes: int

    @classmethod
    def parse(cls, data: bytes) -> Capabilities:
        if len(data) != HEADER_BYTES:
            raise ProtocolError("truncated Desktop Visual hello")
        (magic, version, size, width, height, dimension, frame_bytes,
         formats, alphas, origin, reserved, reserved2, reserved3) = _HEADER.unpack(data)
        if magic != b"DVRHELLO" or version != VERSION or size != HEADER_BYTES:
            raise ProtocolError("unsupported Desktop Visual hello")
        if not 1 <= width <= MAX_OUTPUT_DIMENSION or not 1 <= height <= MAX_OUTPUT_DIMENSION:
            raise ProtocolError("output canvas dimensions are out of range")
        if not 1 <= dimension <= MAX_DIMENSION or not 4 <= frame_bytes <= MAX_FRAME_BYTES:
            raise ProtocolError("invalid frame limits")
        if formats != 1 or not alphas or alphas & ~7 or origin != 1:
            raise ProtocolError("unsupported pixel/alpha/origin capabilities")
        if reserved or reserved2 or reserved3:
            raise ProtocolError("nonzero reserved hello fields")
        return cls(width, height, dimension, frame_bytes, formats, alphas)

    def supports_alpha(self, alpha: Alpha) -> bool:
        return bool(self.alpha_modes & (1 << (int(alpha) - 1)))


def default_socket_path() -> Path:
    """Resolve only explicit override or XDG_RUNTIME_DIR; never use shared /tmp."""
    override = os.environ.get("JWM_DESKTOP_VISUAL_SOCKET")
    if override:
        return Path(override)
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime or not Path(runtime).is_absolute():
        raise ValueError("set XDG_RUNTIME_DIR or JWM_DESKTOP_VISUAL_SOCKET")
    return Path(runtime) / "jwm-desktop-visual" / "visual.sock"


def _integer(value: int, name: str, low: int, high: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ValueError(f"{name} must be an integer in {low}..{high}")
    return value


def frame_header(width: int, height: int, stride: int, alpha: Alpha,
                 sequence: int, timestamp_ns: int,
                 capabilities: Capabilities | None = None) -> bytes:
    """Encode the normative v1 header, validating before any bytes are sent."""
    maximum = capabilities.max_dimension if capabilities else MAX_DIMENSION
    byte_limit = capabilities.max_frame_bytes if capabilities else MAX_FRAME_BYTES
    _integer(width, "width", 1, maximum)
    _integer(height, "height", 1, maximum)
    _integer(stride, "stride", width * 4, (1 << 32) - 1)
    if stride * height > byte_limit:
        raise ValueError("frame exceeds negotiated byte limit")
    _integer(int(alpha) if isinstance(alpha, Alpha) else alpha, "alpha", 1, 3)
    alpha = Alpha(alpha)
    if capabilities and not capabilities.supports_alpha(alpha):
        raise ValueError("consumer does not support requested alpha mode")
    _integer(sequence, "sequence", 1, UINT64_MAX)
    _integer(timestamp_ns, "timestamp_ns", 0, UINT64_MAX)
    return _HEADER.pack(b"DVRFRAME", VERSION, HEADER_BYTES, width, height,
                        stride, 1, 1, alpha, 1, 0, sequence, timestamp_ns)


class Publisher:
    """Bounded blocking producer endpoint, with explicit reconnect ownership.

    timeout bounds a complete handshake or frame write, not each individual
    chunk. Socket buffers apply backpressure; callers decide whether to retry or
    drop a frame. This class never spawns processes, reads input, captures pixels,
    unlinks sockets, or starts a compositor.
    """

    def __init__(self, path: str | os.PathLike[str] | None = None, *, timeout: float = 1.0):
        if not isinstance(timeout, (int, float)) or isinstance(timeout, bool) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be finite and positive")
        self.path = Path(path) if path is not None else default_socket_path()
        self.timeout = float(timeout)
        self.capabilities: Capabilities | None = None
        self._socket: socket.socket | None = None
        self._sequence = 0

    @property
    def connected(self) -> bool:
        """Whether a local connection exists (remote liveness is learned on I/O)."""
        return self._socket is not None

    def _remaining(self, stream: socket.socket, deadline: float) -> None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Desktop Visual operation timed out")
        stream.settimeout(remaining)

    def connect(self) -> Capabilities:
        self.close()
        stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        deadline = time.monotonic() + self.timeout
        try:
            self._remaining(stream, deadline)
            stream.connect(os.fspath(self.path))
            # JWM authenticates producers; authenticate the consumer too on
            # Linux, rather than trusting a user-supplied pathname alone.
            if hasattr(socket, "SO_PEERCRED"):
                credentials = stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
                _, uid, _ = struct.unpack("3i", credentials)
                if uid != os.geteuid():
                    raise PermissionError("Desktop Visual consumer belongs to another user")
            received = bytearray()
            while len(received) < HEADER_BYTES:
                self._remaining(stream, deadline)
                chunk = stream.recv(HEADER_BYTES - len(received))
                if not chunk:
                    raise ConnectionError("consumer closed before hello completed")
                received.extend(chunk)
            capabilities = Capabilities.parse(bytes(received))
        except BaseException:
            stream.close()
            raise
        self._socket = stream
        self.capabilities = capabilities
        self._sequence = 0
        return capabilities

    def publish(self, rgba: bytes | bytearray | memoryview, width: int, height: int,
                *, stride: int | None = None, alpha: Alpha = Alpha.STRAIGHT,
                timestamp_ns: int | None = None) -> int:
        stream, capabilities = self._socket, self.capabilities
        if stream is None or capabilities is None:
            raise ConnectionError("connect before publishing")
        if stride is None:
            # Validate before multiplication to avoid accepting floats/bools.
            _integer(width, "width", 1, capabilities.max_dimension)
            stride = width * 4
        if timestamp_ns is None:
            timestamp_ns = time.monotonic_ns()
        sequence = self._sequence + 1
        header = frame_header(width, height, stride, alpha, sequence,
                              timestamp_ns, capabilities)
        data = memoryview(rgba).cast("B")
        if len(data) != stride * height:
            raise ValueError("payload length must equal stride * height")
        deadline = time.monotonic() + self.timeout
        try:
            self._remaining(stream, deadline)
            stream.sendall(header)
            self._remaining(stream, deadline)
            stream.sendall(data)
        except BaseException:
            # Even an interruption can leave a header/body prefix in the stream.
            # Closing is mandatory; a retry on this connection corrupts framing.
            self.close()
            raise
        self._sequence = sequence
        return sequence

    def close(self) -> None:
        stream, self._socket = self._socket, None
        self.capabilities = None
        self._sequence = 0
        if stream is not None:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            stream.close()

    def __enter__(self) -> Publisher:
        self.connect()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
