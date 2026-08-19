"""The client against a scripted daemon, and the cross-language protocol pin."""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path

import aiohttp
from aiohttp import web
import pytest

from hackrf_proxy_client import PROTOCOL_VERSION, ProxyClient, ProxyError
from hackrf_proxy_client.client import _semver_major

REPO_ROOT = Path(__file__).resolve().parents[3]


def test_protocol_version_matches_the_daemon() -> None:
    """The Rust and Python ends of the wire declare the same version.

    Same repository, one wire protocol, two declarations — this is the test
    that keeps them from drifting apart.
    """
    wire = (REPO_ROOT / "proxyd" / "src" / "wire.rs").read_text()
    match = re.search(r"pub const PROTOCOL_VERSION: u32 = (\d+);", wire)
    assert match is not None, "wire.rs no longer declares PROTOCOL_VERSION"
    assert int(match.group(1)) == PROTOCOL_VERSION


def test_semver_major_parsing() -> None:
    assert _semver_major("1.2.3") == 1
    assert _semver_major("0.1.0") == 0
    assert _semver_major("10.0.0-rc.1") == 10
    assert _semver_major("unknown") is None


class ScriptedDaemon:
    """Just enough of hackrf-proxyd's WebSocket protocol to exercise the client."""

    def __init__(self, daemon_version: str = "0.0.0") -> None:
        self.daemon_version = daemon_version
        self.transmits: list[dict] = []
        self._socket: web.WebSocketResponse | None = None

    async def handler(self, request: web.Request) -> web.WebSocketResponse:
        socket = web.WebSocketResponse()
        await socket.prepare(request)
        self._socket = socket
        async for message in socket:
            if message.type is not aiohttp.WSMsgType.TEXT:
                continue
            payload = json.loads(message.data)
            reply = {"id": payload["id"]}
            if payload["type"] == "status":
                reply |= {
                    "type": "status",
                    "state": "receiving",
                    "device": "HackRF One r9",
                    "daemon_version": self.daemon_version,
                }
            elif payload["type"] == "transmit":
                self.transmits.append(payload)
                # A transmission's own device_state events overtake its reply
                # on the real wire; reproduce that so id matching is what the
                # test exercises, not luck.
                await socket.send_json({"type": "device_state", "state": "transmitting"})
                await socket.send_json({"type": "device_state", "state": "receiving"})
                reply |= {"type": "transmitted", "duration_us": 81_100}
            await socket.send_json(reply)
        return socket

    async def push(self, payload: dict) -> None:
        assert self._socket is not None
        await self._socket.send_json(payload)


async def _connected_client(
    aiohttp_server, daemon: ScriptedDaemon, **kwargs
) -> tuple[ProxyClient, aiohttp.ClientSession]:
    app = web.Application()
    app.router.add_get("/", daemon.handler)
    server = await aiohttp_server(app)
    session = aiohttp.ClientSession()
    client = ProxyClient(session, server.host, server.port, **kwargs)
    await client.async_start()
    await client.async_wait_connected(timeout=5)
    return client, session


async def test_connects_and_reports_the_daemon(aiohttp_server) -> None:
    daemon = ScriptedDaemon()
    client, session = await _connected_client(aiohttp_server, daemon)
    try:
        assert client.available
        assert client.state == "receiving"
        assert client.device == "HackRF One r9"
        assert client.daemon_version == "0.0.0"
        assert client.is_compatible is True  # both majors are 0
    finally:
        await client.async_stop()
        await session.close()


async def test_transmit_returns_air_time_despite_overtaking_events(aiohttp_server) -> None:
    daemon = ScriptedDaemon()
    client, session = await _connected_client(aiohttp_server, daemon)
    try:
        duration = await client.async_transmit(
            frequency=315_000_000, timings=[450, -450, 1350], repeat=9
        )
        assert duration == 81_100
        (request,) = daemon.transmits
        assert request["v"] == PROTOCOL_VERSION
        assert request["frequency"] == 315_000_000
        assert request["repeat"] == 9
    finally:
        await client.async_stop()
        await session.close()


async def test_rx_frames_reach_the_callback(aiohttp_server) -> None:
    daemon = ScriptedDaemon()
    heard: list[dict] = []
    client, session = await _connected_client(aiohttp_server, daemon, on_rx_frame=heard.append)
    try:
        await daemon.push({"type": "rx_frame", "frequency": 315_000_000, "timings": [450, -450]})
        async with asyncio.timeout(5):
            while not heard:
                await asyncio.sleep(0.01)
        assert heard[0]["timings"] == [450, -450]
        assert client.last_rx_frame is not None
    finally:
        await client.async_stop()
        await session.close()


async def test_a_different_major_reads_as_incompatible(aiohttp_server) -> None:
    daemon = ScriptedDaemon(daemon_version="99.0.0")
    client, session = await _connected_client(aiohttp_server, daemon)
    try:
        assert client.is_compatible is False
    finally:
        await client.async_stop()
        await session.close()


async def test_requests_fail_cleanly_when_disconnected() -> None:
    async with aiohttp.ClientSession() as session:
        client = ProxyClient(session, "localhost", 1)
        with pytest.raises(ProxyError):
            await client.async_status()
