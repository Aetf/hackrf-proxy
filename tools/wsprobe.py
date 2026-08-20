#!/usr/bin/env python3
"""Poke hackrf-proxyd's WebSocket API from the command line.

A minimal client with no dependencies, for when websocat is not installed on
the box you are debugging from — which, on a headless server, is most of them.

    tools/wsprobe.py                                   # status
    tools/wsprobe.py '{"type":"status"}'               # any raw request
    tools/wsprobe.py --listen                          # follow rx_frame events
    tools/wsprobe.py --host radio-host --port 8765 ...

Each argument is sent as one request and its reply printed. With --listen it
then keeps printing pushed events until interrupted, which is how you watch
what the receiver is hearing while pressing a remote.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import struct
import sys
from typing import Any

TEXT, CLOSE, PING, PONG = 0x1, 0x8, 0x9, 0xA


class WebSocket:
    """Just enough of RFC 6455 to speak JSON to the daemon."""

    def __init__(self, host: str, port: int, timeout: float) -> None:
        self.sock = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            f"GET / HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        self.buffer: bytes = b""
        header, rest = self._until(b"\r\n\r\n")
        status = header.split(b"\r\n")[0].decode(errors="replace")
        if b"101" not in header.split(b"\r\n")[0]:
            raise SystemExit(f"handshake refused: {status}")
        # Anything the server sent immediately after the handshake arrives in
        # the same read. Dropping it would silently desynchronise the frame
        # stream and leave every later read waiting for bytes that already
        # went past.
        self.buffer = rest

    def _until(self, marker: bytes) -> tuple[bytes, bytes]:
        """Read up to and including `marker`, returning it and the remainder."""
        data = b""
        while marker not in data:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise SystemExit("connection closed during handshake")
            data += chunk
        head, _, rest = data.partition(marker)
        return head + marker, rest

    def _read(self, count: int) -> bytes:
        while len(self.buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise SystemExit("connection closed")
            self.buffer += chunk
        head, self.buffer = self.buffer[:count], self.buffer[count:]
        return head

    def send(self, text: str) -> None:
        payload = text.encode()
        length = len(payload)
        if length < 126:
            header = struct.pack("!BB", 0x80 | TEXT, 0x80 | length)
        elif length < 1 << 16:
            header = struct.pack("!BBH", 0x80 | TEXT, 0x80 | 126, length)
        else:
            header = struct.pack("!BBQ", 0x80 | TEXT, 0x80 | 127, length)
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def close(self) -> None:
        """Close politely, so the daemon logs a disconnect and not an error."""
        try:
            self.sock.sendall(struct.pack("!BB", 0x80 | CLOSE, 0x80) + os.urandom(4))
            self.sock.close()
        except OSError:
            pass

    def recv(self) -> str:
        """Return the next text message, answering pings along the way."""
        while True:
            first, second = struct.unpack("!BB", self._read(2))
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                (length,) = struct.unpack("!H", self._read(2))
            elif length == 127:
                (length,) = struct.unpack("!Q", self._read(8))
            payload = self._read(length)

            if opcode == TEXT:
                return payload.decode()
            if opcode == PING:
                self.sock.sendall(struct.pack("!BB", 0x80 | PONG, 0x80) + os.urandom(4))
            elif opcode == CLOSE:
                raise SystemExit("server closed the connection")


def show(direction: str, text: str) -> Any:
    # Flushed every time: piped to a file, Python block-buffers stdout, and a
    # monitor whose output only appears when it exits is no monitor at all.
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError:
        print(f"{direction} {text}", flush=True)
        return None
    print(f"{direction} {json.dumps(parsed, indent=2, sort_keys=True)}", flush=True)
    return parsed


def await_reply(socket_: WebSocket, request_id: str) -> Any:
    """Read until the reply to `request_id` arrives, printing events meanwhile.

    The daemon pushes events at any time, so a reply is not necessarily the
    next message on the wire — a transmission's own `device_state` event
    overtakes it. Matching on the id is how a real client keeps them apart,
    which is why requests here are always sent with one.
    """
    while True:
        message = show("<-", socket_.recv())
        if message is None or message.get("id") == request_id:
            return message


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("requests", nargs="*", help="raw JSON requests to send")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument(
        "--listen", action="store_true", help="keep printing pushed events afterwards"
    )
    args = parser.parse_args()

    socket_ = WebSocket(args.host, args.port, args.timeout)
    for number, request in enumerate(args.requests or ['{"type":"status"}'], start=1):
        # Give every request an id, unless it already has one or is deliberate
        # nonsense, so the reply can be told apart from an event.
        request_id = None
        try:
            parsed = json.loads(request)
            request_id = parsed.setdefault("id", f"probe-{number}")
            request = json.dumps(parsed)
        except (json.JSONDecodeError, AttributeError):
            pass

        show("->", request)
        socket_.send(request)
        if request_id is None:
            show("<-", socket_.recv())
        else:
            await_reply(socket_, request_id)

    if args.listen:
        print("-- following events, ^C to stop", file=sys.stderr)
        socket_.sock.settimeout(None)
        while True:
            show("<<", socket_.recv())

    socket_.close()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
