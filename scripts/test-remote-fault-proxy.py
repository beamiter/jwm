#!/usr/bin/env python3
"""No privileges, remote systems, display, or wall-clock throughput assertions."""
import asyncio
import importlib.util
from pathlib import Path
import sys
import unittest

spec = importlib.util.spec_from_file_location('fault_proxy', Path(__file__).with_name('remote-fault-proxy.py'))
proxy_module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = proxy_module
spec.loader.exec_module(proxy_module)
Direction, Proxy, forward = proxy_module.Direction, proxy_module.Proxy, proxy_module.forward


class MemoryIO:
    def __init__(self, data):
        self.data = data
        self.sent = bytearray()
        self.read_sizes = []
        self.delays = []
        self.closed = False

    async def recv(self, size):
        self.read_sizes.append(size)
        data, self.data = self.data[:size], self.data[size:]
        return data

    async def send(self, data):
        self.sent.extend(data)

    def eof(self):
        self.closed = True

    async def sleep(self, delay):
        self.delays.append(delay)

    async def run(self, policy, chunk=4):
        await forward(self.recv, self.send, self.eof, policy, chunk, self.sleep)


class PolicyTests(unittest.IsolatedAsyncioTestCase):
    async def test_exact_forward_and_delay_accounting(self):
        io = MemoryIO(b'0123456789')
        await io.run(Direction(latency_ms=10, bytes_per_second=100))
        self.assertEqual(io.sent, b'0123456789')
        self.assertEqual(io.delays, [.05, .05, .03])
        self.assertTrue(io.closed)
        self.assertLessEqual(max(io.read_sizes), 4)

    async def test_pause_occurs_at_exact_boundary_once(self):
        io = MemoryIO(b'abcdefgh')
        await io.run(Direction(pause_after=3, pause_seconds=7))
        self.assertEqual(io.delays, [7])
        self.assertEqual(io.read_sizes, [3, 4, 4, 4])
        self.assertEqual(io.sent, b'abcdefgh')

    async def test_disconnect_exact_boundary(self):
        for boundary in (0, 3, 8):
            io = MemoryIO(b'abcdefgh')
            with self.assertRaises(proxy_module.Disconnect):
                await io.run(Direction(disconnect_after=boundary))
            self.assertEqual(io.sent, b'abcdefgh'[:boundary])
            self.assertFalse(io.closed)

    async def test_stopped_reader_cancels_without_read_ahead(self):
        io = MemoryIO(b'abcdefgh')
        entered = asyncio.Event()
        async def sent(data):
            await io.send(data)
            entered.set()
        task = asyncio.create_task(forward(io.recv, sent, io.eof, Direction(pause_after=3), 4))
        await asyncio.wait_for(entered.wait(), 2)
        self.assertEqual(io.read_sizes, [3])
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await asyncio.wait_for(task, 2)
        self.assertEqual(io.sent, b'abc')

    async def test_blocked_send_has_one_chunk_and_cancels(self):
        io = MemoryIO(b'x' * 100)
        entered = asyncio.Event()
        async def blocked(data):
            self.assertEqual(len(data), 4)
            entered.set()
            await asyncio.Future()
        task = asyncio.create_task(forward(io.recv, blocked, io.eof, Direction(), 4))
        await asyncio.wait_for(entered.wait(), 2)
        self.assertEqual(io.read_sizes, [4])
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await asyncio.wait_for(task, 2)

    async def test_validation(self):
        for value in (-1, float('nan'), float('inf')):
            with self.assertRaises(ValueError):
                Direction(latency_ms=value)
        with self.assertRaises(ValueError):
            Direction(pause_after=-1)
        with self.assertRaises(ValueError):
            Proxy(('127.0.0.1', 1), chunk_bytes=0)


class TransportTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.handlers = set()
        self.received = asyncio.get_running_loop().create_future()
        self.payload = bytes(range(256)) * 333
        async def upstream(reader, writer):
            task = asyncio.current_task()
            self.handlers.add(task)
            try:
                # Send while client sends, so both directional pumps are exercised.
                writer.write(self.payload[::-1])
                await writer.drain()
                writer.write_eof()
                data = await reader.read()
                if not self.received.done():
                    self.received.set_result(data)
            except OSError:
                pass  # Expected when the fault fixture closes this connection.
            finally:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass
                self.handlers.discard(task)
        self.server = await asyncio.start_server(upstream, '127.0.0.1', 0)
        self.upstream = self.server.sockets[0].getsockname()
        self.proxy = Proxy(self.upstream, chunk_bytes=1024)
        self.address = await self.proxy.start()
        self.writers = []

    async def asyncTearDown(self):
        await asyncio.wait_for(self.proxy.close(), 2)
        for writer in self.writers:
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass
        self.server.close()
        await self.server.wait_closed()
        tasks = list(self.handlers)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)

    async def connect(self):
        reader, writer = await asyncio.open_connection(*self.address)
        self.writers.append(writer)
        return reader, writer

    async def test_bidirectional_byte_exact_and_half_close(self):
        reader, writer = await self.connect()
        writer.write(self.payload)
        await writer.drain()
        writer.write_eof()
        self.assertEqual(await asyncio.wait_for(reader.read(), 3), self.payload[::-1])
        self.assertEqual(await asyncio.wait_for(self.received, 3), self.payload)

    async def test_shutdown_stopped_reader(self):
        self.proxy.policies = (Direction(pause_after=0), Direction(pause_after=0))
        reader, writer = await self.connect()
        writer.write(b'blocked')
        await writer.drain()
        await asyncio.wait_for(self.proxy.close(), 2)
        self.assertFalse(self.proxy.sessions)
        try:
            self.assertEqual(await asyncio.wait_for(reader.read(), 2), b'')
        except ConnectionResetError:
            pass  # TCP may reset a socket closed with unread data.


if __name__ == '__main__':
    unittest.main()
