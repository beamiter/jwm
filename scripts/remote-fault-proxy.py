#!/usr/bin/env python3
"""Bounded, user-space TCP fault fixture; not a packet-network emulator."""
import argparse
import asyncio
from dataclasses import dataclass
import json
import math
import signal
import socket


@dataclass(frozen=True)
class Direction:
    latency_ms: float = 0
    bytes_per_second: float = 0
    pause_after: int | None = None
    pause_seconds: float = 0  # zero means stop reading until shutdown
    disconnect_after: int | None = None

    def __post_init__(self):
        for name in ('latency_ms', 'bytes_per_second', 'pause_seconds'):
            value = getattr(self, name)
            if not math.isfinite(value) or value < 0:
                raise ValueError(f'{name} must be finite and nonnegative')
        for name in ('pause_after', 'disconnect_after'):
            value = getattr(self, name)
            if value is not None and value < 0:
                raise ValueError(f'{name} must be nonnegative')


class Disconnect(Exception):
    """Intentional whole-session disconnect."""


async def forward(recv, send, eof, policy, chunk_bytes, sleep=asyncio.sleep):
    """One chunk in flight, no read-ahead. Injected I/O enables deterministic tests."""
    transferred = 0
    paused = False
    while True:
        if policy.disconnect_after is not None and transferred >= policy.disconnect_after:
            raise Disconnect()
        if not paused and policy.pause_after is not None and transferred >= policy.pause_after:
            paused = True
            if policy.pause_seconds:
                await sleep(policy.pause_seconds)
            else:
                await asyncio.Future()  # cancelled by session/proxy shutdown
        size = chunk_bytes
        for boundary in (policy.disconnect_after, None if paused else policy.pause_after):
            if boundary is not None:
                size = min(size, boundary - transferred)
        data = await recv(size)
        if not data:
            eof()
            return
        delay = policy.latency_ms / 1000
        if policy.bytes_per_second:
            delay += len(data) / policy.bytes_per_second
        if delay:
            await sleep(delay)
        await send(data)
        transferred += len(data)


class Proxy:
    def __init__(self, upstream, c2s=Direction(), s2c=Direction(), *, chunk_bytes=16384,
                 max_connections=16, connect_timeout=5, session_seconds=0):
        if not 1 <= chunk_bytes <= 1048576:
            raise ValueError('chunk_bytes must be in 1..1048576')
        if not 1 <= max_connections <= 1024:
            raise ValueError('max_connections must be in 1..1024')
        if not math.isfinite(connect_timeout) or connect_timeout <= 0:
            raise ValueError('connect_timeout must be finite and positive')
        if not math.isfinite(session_seconds) or session_seconds < 0:
            raise ValueError('session_seconds must be finite and nonnegative')
        self.upstream = upstream
        self.policies = (c2s, s2c)
        self.chunk_bytes = chunk_bytes
        self.max_connections = max_connections
        self.connect_timeout = connect_timeout
        self.session_seconds = session_seconds
        self.sessions = set()
        self.listener = None
        self.accept_task = None

    async def start(self, host='127.0.0.1', port=0):
        # Listener accepts numeric IPv4/IPv6 addresses, never implicit wildcard DNS.
        family = socket.AF_INET6 if ':' in host else socket.AF_INET
        listener = socket.socket(family, socket.SOCK_STREAM)
        try:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind((host, port))
            listener.listen(self.max_connections)
            listener.setblocking(False)
        except BaseException:
            listener.close()
            raise
        self.listener = listener
        self.accept_task = asyncio.create_task(self._accept())
        return listener.getsockname()

    async def _accept(self):
        loop = asyncio.get_running_loop()
        while True:
            client, _ = await loop.sock_accept(self.listener)
            if len(self.sessions) >= self.max_connections:
                client.close()
                continue
            client.setblocking(False)
            task = asyncio.create_task(self._session(client))
            self.sessions.add(task)
            task.add_done_callback(self.sessions.discard)
            task.add_done_callback(lambda done, sock=client: sock.close())

    async def _session(self, client):
        upstream = None
        pumps = []
        loop = asyncio.get_running_loop()
        try:
            async with asyncio.timeout(self.connect_timeout):
                addresses = await loop.getaddrinfo(*self.upstream, type=socket.SOCK_STREAM)
                for family, kind, proto, _, address in addresses:
                    upstream = socket.socket(family, kind, proto)
                    upstream.setblocking(False)
                    try:
                        await loop.sock_connect(upstream, address)
                        break
                    except OSError:
                        upstream.close()
                else:
                    raise OSError('no upstream address connected')
            def half_close(sock):
                try:
                    sock.shutdown(socket.SHUT_WR)
                except OSError:
                    pass
            for source, target, policy in ((client, upstream, self.policies[0]),
                                           (upstream, client, self.policies[1])):
                async def recv(size, sock=source):
                    return await loop.sock_recv(sock, size)
                async def send(data, sock=target):
                    await loop.sock_sendall(sock, data)
                pumps.append(asyncio.create_task(forward(
                    recv, send, lambda sock=target: half_close(sock), policy, self.chunk_bytes)))
            async with asyncio.timeout(self.session_seconds or None):
                await asyncio.gather(*pumps)
        except (OSError, Disconnect, TimeoutError):
            pass
        finally:
            for task in pumps:
                task.cancel()
            await asyncio.gather(*pumps, return_exceptions=True)
            client.close()
            if upstream is not None:
                upstream.close()

    async def close(self):
        if self.accept_task is not None:
            self.accept_task.cancel()
            await asyncio.gather(self.accept_task, return_exceptions=True)
        if self.listener is not None:
            self.listener.close()
        sessions = list(self.sessions)
        for task in sessions:
            task.cancel()
        await asyncio.gather(*sessions, return_exceptions=True)
        self.sessions.clear()


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument('--upstream-host', required=True)
    result.add_argument('--upstream-port', required=True, type=int)
    result.add_argument('--listen-host', default='127.0.0.1')
    result.add_argument('--listen-port', default=0, type=int)
    result.add_argument('--chunk-bytes', default=16384, type=int)
    result.add_argument('--max-connections', default=16, type=int)
    result.add_argument('--connect-timeout', default=5, type=float)
    result.add_argument('--session-seconds', default=0, type=float)
    for prefix in ('c2s', 's2c'):
        result.add_argument(f'--{prefix}-latency-ms', default=0, type=float)
        result.add_argument(f'--{prefix}-bytes-per-second', default=0, type=float)
        result.add_argument(f'--{prefix}-pause-after', type=int)
        result.add_argument(f'--{prefix}-pause-seconds', default=0, type=float)
        result.add_argument(f'--{prefix}-disconnect-after', type=int)
    return result


async def main(args):
    policies = [Direction(**{name: getattr(args, prefix + '_' + name)
                              for name in Direction.__dataclass_fields__})
                for prefix in ('c2s', 's2c')]
    if not 1 <= args.upstream_port <= 65535 or not 0 <= args.listen_port <= 65535:
        raise ValueError('invalid TCP port')
    proxy = Proxy((args.upstream_host, args.upstream_port), *policies,
                  chunk_bytes=args.chunk_bytes, max_connections=args.max_connections,
                  connect_timeout=args.connect_timeout, session_seconds=args.session_seconds)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)
    try:
        address = await proxy.start(args.listen_host, args.listen_port)
        print(json.dumps({'schema_version': 1, 'listening': address, 'config': vars(args)}), flush=True)
        await stop.wait()
    finally:
        await proxy.close()


if __name__ == '__main__':
    cli = parser()
    try:
        asyncio.run(main(cli.parse_args()))
    except ValueError as error:
        cli.error(str(error))
