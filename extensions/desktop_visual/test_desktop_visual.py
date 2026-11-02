"""CPU-only protocol/lifecycle tests; these do not substitute for GPU QA."""

from contextlib import contextmanager
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from desktop_visual import (
    Alpha, Capabilities, HEADER_BYTES, MAX_FRAME_BYTES, ProtocolError,
    Publisher, default_socket_path, frame_header,
)
from plasma import frame_size, plasma

WIRE = struct.Struct("<8s10I2Q")


def hello(width=1920, height=1080, *, alphas=7):
    return WIRE.pack(b"DVRHELLO", 1, 64, width, height, 4096,
                     MAX_FRAME_BYTES, 1, alphas, 1, 0, 0, 0)


def receive(stream, count):
    result = bytearray()
    while len(result) < count:
        chunk = stream.recv(count - len(result))
        if not chunk:
            raise EOFError("incomplete message")
        result.extend(chunk)
    return bytes(result)


def receive_frame(stream):
    header = receive(stream, 64)
    fields = WIRE.unpack(header)
    return fields, receive(stream, fields[5] * fields[4])


@contextmanager
def consumer(*handlers):
    """Private same-uid mock consumer; every worker is joined and checked."""
    with tempfile.TemporaryDirectory(prefix="desktop-visual-test-") as directory:
        path = Path(directory) / "visual.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        listener.listen(2)
        listener.settimeout(2)
        stop = threading.Event()
        errors = []

        def run():
            try:
                for handle in handlers:
                    stream, _ = listener.accept()
                    with stream:
                        stream.settimeout(2)
                        handle(stream, stop)
            except BaseException as error:
                errors.append(error)

        worker = threading.Thread(target=run, name="desktop-visual-test-consumer")
        worker.start()
        try:
            yield path, stop
        finally:
            stop.set()
            worker.join(3)
            listener.close()
            if worker.is_alive():
                raise AssertionError("consumer worker failed to stop")
            if errors:
                raise errors[0]


class WireTests(unittest.TestCase):
    def test_golden_frame_vector(self):
        expected = bytes.fromhex(
            "4456524652414d45 01000000 40000000 02000000 01000000 "
            "08000000 01000000 01000000 02000000 01000000 00000000 "
            "0807060504030201 1817161514131211")
        self.assertEqual(frame_header(2, 1, 8, Alpha.STRAIGHT,
                                      0x0102030405060708, 0x1112131415161718), expected)
        self.assertEqual(len(expected), HEADER_BYTES)

    def test_hello_capabilities(self):
        caps = Capabilities.parse(hello(7680, 2160))
        self.assertEqual((caps.output_width, caps.output_height), (7680, 2160))
        self.assertTrue(caps.supports_alpha(Alpha.PREMULTIPLIED))
        self.assertEqual(caps.max_frame_bytes, MAX_FRAME_BYTES)
        edge = Capabilities.parse(hello(65535, 65535))
        self.assertEqual((edge.output_width, edge.output_height), (65535, 65535))

    def test_bad_hello_fields(self):
        valid = hello()
        for index, value in [(0, b"NOTHELLO"), (1, 2), (2, 63), (3, 0), (4, 0), (3, 65536), (4, 65536),
                             (5, 0), (5, 4097), (6, 3), (6, MAX_FRAME_BYTES + 1),
                             (7, 2), (8, 0), (8, 8), (9, 2), (10, 1),
                             (11, 1), (12, 1)]:
            fields = list(WIRE.unpack(valid))
            fields[index] = value
            with self.subTest(index=index, value=value), self.assertRaises(ProtocolError):
                Capabilities.parse(WIRE.pack(*fields))
        for size in (0, 63, 65):
            with self.subTest(size=size), self.assertRaises(ProtocolError):
                Capabilities.parse(valid[:size] if size < 64 else valid + b"x")

    def test_frame_ranges_checked_before_pack(self):
        valid = dict(width=2, height=1, stride=8, alpha=Alpha.STRAIGHT,
                     sequence=1, timestamp_ns=0)
        for key, value in [("width", 0), ("width", True), ("width", 4097),
                           ("height", -1), ("height", 1.0), ("stride", 7),
                           ("stride", 2**32), ("alpha", 0), ("alpha", 4),
                           ("alpha", True), ("alpha", 1.0), ("sequence", 0),
                           ("sequence", 2**64), ("timestamp_ns", -1),
                           ("timestamp_ns", 2**64)]:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                frame_header(**(valid | {key: value}))
        with self.assertRaises(ValueError):
            frame_header(4096, 4096, 4096 * 4, Alpha.OPAQUE, 1, 0)
        # Exactly the limit is legal, padded rows are legal.
        self.assertEqual(len(frame_header(2048, 4096, 8192, Alpha.OPAQUE, 1, 0)), 64)
        self.assertEqual(len(frame_header(2, 2, 12, Alpha.OPAQUE, 1, 0)), 64)

    def test_negotiated_limits_and_alpha(self):
        caps = Capabilities(100, 100, 16, 64, 1, 1)
        with self.assertRaises(ValueError):
            frame_header(1, 1, 4, Alpha.STRAIGHT, 1, 0, caps)
        with self.assertRaises(ValueError):
            frame_header(17, 1, 68, Alpha.OPAQUE, 1, 0, caps)
        with self.assertRaises(ValueError):
            frame_header(8, 8, 32, Alpha.OPAQUE, 1, 0, caps)

    def test_no_unsafe_default_path(self):
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError):
                default_socket_path()
        with patch.dict(os.environ, {"XDG_RUNTIME_DIR": "relative"}, clear=True):
            with self.assertRaises(ValueError):
                default_socket_path()
        with patch.dict(os.environ, {"XDG_RUNTIME_DIR": "/run/user/1000"}, clear=True):
            self.assertEqual(default_socket_path(), Path("/run/user/1000/jwm-desktop-visual/visual.sock"))
        with patch.dict(os.environ, {"JWM_DESKTOP_VISUAL_SOCKET": "/private/custom.sock"}, clear=True):
            self.assertEqual(default_socket_path(), Path("/private/custom.sock"))

    def test_timeout_argument(self):
        for value in (0, -1, float("inf"), float("nan"), True, "1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                Publisher("/unused.sock", timeout=value)


@unittest.skipIf(os.environ.get("DESKTOP_VISUAL_SKIP_SOCKETS") == "1",
                     "native Unix IPC explicitly disabled for this restricted runner")
class LifecycleTests(unittest.TestCase):
    def test_payload_padding_sequence_and_validation(self):
        received = []

        def handler(stream, stop):
            stream.sendall(hello())
            received.append(receive_frame(stream))
            received.append(receive_frame(stream))
            self.assertEqual(stream.recv(1), b"")

        with consumer(handler) as (path, _):
            with Publisher(path) as publisher:
                with self.assertRaises(ValueError):
                    publisher.publish(b"bad", 2, 1)
                self.assertTrue(publisher.connected)
                self.assertEqual(publisher.publish(bytes(range(12)), 2, 1, stride=12,
                                                   alpha=Alpha.OPAQUE, timestamp_ns=8), 1)
                self.assertEqual(publisher.publish(b"\x10\x20\x30\x40", 1, 1,
                                                   alpha=Alpha.PREMULTIPLIED, timestamp_ns=9), 2)
            self.assertTrue(path.is_socket(), "SDK never unlinks consumer-owned socket")
            self.assertFalse(publisher.connected)
        self.assertEqual(received[0][0][11:], (1, 8))
        self.assertEqual(received[1][0][11:], (2, 9))
        self.assertEqual(received[0][1], bytes(range(12)))
        self.assertEqual(received[1][1], b"\x10\x20\x30\x40")

    def test_reconnect_renegotiates_output_and_resets_sequence(self):
        received = []

        def first(stream, stop):
            stream.sendall(hello(640, 480))
            received.append(receive_frame(stream))
            self.assertEqual(stream.recv(1), b"")

        def second(stream, stop):
            stream.sendall(hello(2560, 720))
            received.append(receive_frame(stream))

        with consumer(first, second) as (path, _):
            publisher = Publisher(path)
            old = publisher.connect()
            publisher.publish(b"\xff" * 4, 1, 1)
            new = publisher.connect()
            self.assertNotEqual(old.output_width, new.output_width)
            self.assertEqual(new.output_width, 2560)
            publisher.publish(b"\x00" * 4, 1, 1)
            publisher.close()
        self.assertEqual([fields[11] for fields, _ in received], [1, 1])

    def test_invalid_or_truncated_hello_closes_socket(self):
        for data, error in [(b"x" * 64, ProtocolError), (hello()[:12], ConnectionError)]:
            def handler(stream, stop):
                stream.sendall(data)
            with self.subTest(data=data), consumer(handler) as (path, _):
                publisher = Publisher(path)
                with self.assertRaises(error):
                    publisher.connect()
                self.assertFalse(publisher.connected)
                self.assertIsNone(publisher.capabilities)

    def test_slowloris_hello_has_total_deadline(self):
        def handler(stream, stop):
            for value in hello():
                if stop.wait(0.02):
                    return
                try:
                    stream.sendall(bytes([value]))
                except (BrokenPipeError, ConnectionResetError):
                    return

        with consumer(handler) as (path, _):
            publisher = Publisher(path, timeout=0.09)
            started = time.monotonic()
            with self.assertRaises(TimeoutError):
                publisher.connect()
            self.assertLess(time.monotonic() - started, 0.7)
            self.assertFalse(publisher.connected)

    def test_backpressure_disconnects_partial_frame(self):
        def handler(stream, stop):
            stream.sendall(hello())
            stop.wait(2)

        with consumer(handler) as (path, _):
            publisher = Publisher(path, timeout=0.1)
            publisher.connect()
            publisher._socket.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
            payload = bytes(1024 * 1024 * 4)
            started = time.monotonic()
            with self.assertRaises(TimeoutError):
                publisher.publish(payload, 1024, 1024)
            self.assertLess(time.monotonic() - started, 0.8)
            self.assertFalse(publisher.connected)
            with self.assertRaises(ConnectionError):
                publisher.publish(b"\x00" * 4, 1, 1)

    def test_abrupt_shutdown_is_not_silent_success(self):
        closed = threading.Event()

        def handler(stream, stop):
            stream.sendall(hello())
            stream.shutdown(socket.SHUT_RDWR)
            closed.set()

        with consumer(handler) as (path, _):
            publisher = Publisher(path)
            publisher.connect()
            self.assertTrue(closed.wait(1))
            with self.assertRaises(OSError):
                publisher.publish(b"\x00" * 4, 1, 1)
            self.assertFalse(publisher.connected)

    def test_close_is_idempotent_and_no_server_required(self):
        publisher = Publisher("/nonexistent/desktop-visual-test.sock")
        publisher.close()
        publisher.close()
        with self.assertRaises(ConnectionError):
            publisher.publish(b"\x00" * 4, 1, 1)
        with self.assertRaises(OSError):
            publisher.connect()
        self.assertFalse(publisher.connected)



class TransportStub:
    """In-memory I/O fault injector, not a substitute for the Unix tests above."""

    def __init__(self, data=None, *, error=None, peer_uid=None):
        self.data = bytearray(hello() if data is None else data)
        self.sent = []
        self.closed = False
        self.timeouts = []
        self.error = error
        self.peer_uid = os.geteuid() if peer_uid is None else peer_uid
        self.recv_hook = lambda: None

    def settimeout(self, timeout):
        self.timeouts.append(timeout)

    def connect(self, path):
        self.path = path

    def getsockopt(self, level, option, length):
        return struct.pack("3i", 100, self.peer_uid, os.getgid())

    def recv(self, count):
        self.recv_hook()
        result = bytes(self.data[:min(count, 7)])
        del self.data[:len(result)]
        return result

    def sendall(self, data):
        self.sent.append(bytes(data))
        if self.error and len(self.sent) == 2:
            raise self.error

    def shutdown(self, how):
        self.shutdown_called = True

    def close(self):
        self.closed = True


class MockTransportTests(unittest.TestCase):
    def test_partial_hello_and_two_frames(self):
        stream = TransportStub()
        with patch("desktop_visual.socket.socket", return_value=stream):
            with Publisher("/private/visual.sock") as publisher:
                self.assertEqual(publisher.capabilities.output_width, 1920)
                publisher.publish(b"abcd", 1, 1, timestamp_ns=1)
                publisher.publish(b"efgh", 1, 1, timestamp_ns=2)
        self.assertEqual([WIRE.unpack(stream.sent[i])[11] for i in (0, 2)], [1, 2])
        self.assertEqual([stream.sent[i] for i in (1, 3)], [b"abcd", b"efgh"])
        self.assertTrue(stream.closed)
        self.assertTrue(all(0 < timeout <= 1 for timeout in stream.timeouts))

    def test_partial_body_failure_always_closes(self):
        for error in (TimeoutError("stalled"), BrokenPipeError("closed"), KeyboardInterrupt()):
            stream = TransportStub(error=error)
            with self.subTest(error=type(error)), patch("desktop_visual.socket.socket", return_value=stream):
                publisher = Publisher("/private/visual.sock")
                publisher.connect()
                with self.assertRaises(type(error)):
                    publisher.publish(b"abcd", 1, 1)
                self.assertFalse(publisher.connected)
                self.assertTrue(stream.closed)
                self.assertIsNone(publisher.capabilities)

    def test_hello_errors_close_unpublished_connection(self):
        for data in (hello()[:5], b"x" * 64):
            stream = TransportStub(data)
            with self.subTest(data=data), patch("desktop_visual.socket.socket", return_value=stream):
                publisher = Publisher("/private/visual.sock")
                with self.assertRaises((ConnectionError, ProtocolError)):
                    publisher.connect()
                self.assertTrue(stream.closed)
                self.assertFalse(publisher.connected)

    @unittest.skipUnless(hasattr(socket, "SO_PEERCRED"), "Linux peer credentials unavailable")
    def test_other_uid_consumer_is_rejected(self):
        stream = TransportStub(peer_uid=os.geteuid() + 1)
        with patch("desktop_visual.socket.socket", return_value=stream):
            publisher = Publisher("/private/visual.sock")
            with self.assertRaises(PermissionError):
                publisher.connect()
            self.assertTrue(stream.closed)
            self.assertFalse(publisher.connected)

    @unittest.skipUnless(hasattr(socket, "SO_PEERCRED"), "Linux peer credentials unavailable")
    def test_peer_check_uses_effective_uid(self):
        effective_uid = os.geteuid() + 100
        stream = TransportStub(peer_uid=effective_uid)
        with patch("desktop_visual.os.geteuid", return_value=effective_uid), \
                patch("desktop_visual.socket.socket", return_value=stream):
            with Publisher("/private/visual.sock") as publisher:
                self.assertTrue(publisher.connected)

    def test_complete_handshake_deadline_not_per_chunk(self):
        clock = [100.0]
        stream = TransportStub()
        def advance():
            clock[0] += 0.06
        stream.recv_hook = advance
        with patch("desktop_visual.socket.socket", return_value=stream), \
                patch("desktop_visual.time.monotonic", side_effect=lambda: clock[0]):
            publisher = Publisher("/private/visual.sock", timeout=0.1)
            with self.assertRaises(TimeoutError):
                publisher.connect()
            self.assertTrue(stream.closed)
            self.assertAlmostEqual(clock[0], 100.12)

    def test_bad_local_frame_does_not_write_or_close(self):
        stream = TransportStub()
        with patch("desktop_visual.socket.socket", return_value=stream):
            with Publisher("/private/visual.sock") as publisher:
                with self.assertRaises(ValueError):
                    publisher.publish(b"short", 4, 4)
                self.assertEqual(stream.sent, [])
                self.assertTrue(publisher.connected)
                self.assertEqual(publisher.publish(b"abcd", 1, 1), 1)

    def test_reconnect_replaces_socket_and_reads_new_hello(self):
        old, new = TransportStub(hello(100, 100)), TransportStub(hello(200, 50))
        with patch("desktop_visual.socket.socket", side_effect=[old, new]):
            publisher = Publisher("/private/visual.sock")
            publisher.connect()
            publisher.publish(b"abcd", 1, 1)
            self.assertEqual(publisher.connect().output_width, 200)
            self.assertTrue(old.closed)
            publisher.publish(b"abcd", 1, 1)
            publisher.close()
        self.assertEqual(WIRE.unpack(old.sent[0])[11], 1)
        self.assertEqual(WIRE.unpack(new.sent[0])[11], 1)


class PlasmaTests(unittest.TestCase):
    def test_art_is_deterministic_rgba_and_animated(self):
        first = plasma(31, 17, 0.0)
        self.assertEqual(first, plasma(31, 17, 0.0))
        self.assertEqual(len(first), 31 * 17 * 4)
        self.assertNotEqual(first, plasma(31, 17, 0.2))
        alpha = first[3::4]
        self.assertLess(min(alpha), 10)
        self.assertLessEqual(max(alpha), 150)
        self.assertGreater(max(alpha), 100)

    def test_output_resize_and_limits(self):
        caps = Capabilities.parse(hello(7680, 2160))
        self.assertEqual(frame_size(caps, 320), (320, 90))
        caps = Capabilities(400, 400, 8, 64, 1, 7)
        width, height = frame_size(caps, 320)
        self.assertLessEqual(max(width, height), 8)
        self.assertLessEqual(width * height * 4, 64)
        for output in [(1, 65535), (65535, 1), (500, 500)]:
            caps = Capabilities(*output, 4096, 4, 1, 7)
            self.assertEqual(frame_size(caps, 4096), (1, 1))

    @unittest.skipIf(os.environ.get("DESKTOP_VISUAL_SKIP_SOCKETS") == "1",
                     "native Unix IPC explicitly disabled for this restricted runner")
    def test_finite_example_sends_three_frames(self):
        frames = []

        def handler(stream, stop):
            stream.sendall(hello(100, 50))
            frames.extend(receive_frame(stream) for _ in range(3))
            self.assertEqual(stream.recv(1), b"")

        with consumer(handler) as (path, _):
            result = subprocess.run([sys.executable, str(Path(__file__).with_name("plasma.py")),
                                     "--socket", str(path), "--frames", "3", "--size", "16",
                                     "--fps", "120"], timeout=5, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([fields[11] for fields, _ in frames], [1, 2, 3])
        self.assertEqual([fields[3:5] for fields, _ in frames], [(16, 8)] * 3)
        self.assertNotEqual(frames[0][1], frames[-1][1])

    @unittest.skipIf(os.environ.get("DESKTOP_VISUAL_SKIP_SOCKETS") == "1",
                     "native Unix IPC explicitly disabled for this restricted runner")
    def test_duration_bounds_reconnect_without_server(self):
        started = time.monotonic()
        result = subprocess.run([sys.executable, str(Path(__file__).with_name("plasma.py")),
                                 "--socket", "/nonexistent/desktop-visual-test.sock",
                                 "--duration", "0.15"], timeout=2, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 1.0)


if __name__ == "__main__":
    unittest.main()
