"""WebSocket client for the hackrf-proxyd daemon.

Thin on purpose: it moves raw OOK timings and knows nothing about any
appliance. The daemon's protocol is documented in `proxyd/README.md`.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable
import contextlib
from datetime import datetime
import logging
from typing import Any

import aiohttp

from homeassistant.util import dt as dt_util

_LOGGER = logging.getLogger(__name__)

#: Protocol version this client speaks. The daemon refuses anything else by
#: name rather than misreading it.
PROTOCOL_VERSION = 1

#: How long to wait for a reply. Generous because a reply arrives when the
#: transmission is *over*, and the daemon allows up to 30 seconds of air time
#: per request, plus whatever is queued ahead of it on a shared radio.
REQUEST_TIMEOUT = 60.0

_INITIAL_BACKOFF = 1.0
_MAX_BACKOFF = 60.0


class ProxyError(Exception):
    """The daemon refused a request, or could not be reached."""


class ProxyClient:
    """A reconnecting client for one daemon.

    Holds a single connection and multiplexes requests over it. Replies are
    matched by id, which is not optional: the daemon pushes events at any time,
    and a transmission's own `device_state` event overtakes its reply.
    """

    def __init__(
        self,
        session: aiohttp.ClientSession,
        host: str,
        port: int,
        *,
        on_rx_frame: Callable[[dict[str, Any]], None] | None = None,
    ) -> None:
        """Initialize the client."""
        self._session = session
        self._url = f"ws://{host}:{port}"
        self._on_rx_frame = on_rx_frame
        self._availability_listeners: list[Callable[[bool], None]] = []
        #: Told about anything the diagnostics show, not only availability.
        #: The radio moving between idle, receiving and transmitting does not
        #: change availability at all, so a listener on that alone would show a
        #: state that only ever updated when something broke.
        self._update_listeners: list[Callable[[], None]] = []

        self._socket: aiohttp.ClientWebSocketResponse | None = None
        self._task: asyncio.Task[None] | None = None
        self._pending: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._next_id = 0
        self._connected = asyncio.Event()
        self._closing = False
        #: Board and firmware of the radio, once the daemon has reported them.
        self.device: str | None = None
        #: The daemon's own version.
        self.daemon_version: str | None = None

        #: What the radio was last seen doing: `receiving`, `transmitting`,
        #: `idle` or `faulted`, and `None` while there is no connection to ask.
        #: This is the refinement `available` cannot carry — a transmitter that
        #: is unavailable is either unreachable or broken, and which of the two
        #: is the entire question when something stops working.
        self.state: str | None = None
        #: When the current connection was established, and how many previous
        #: ones ended. Kept because an intermittent link is invisible from a
        #: connected socket: the only trace it leaves is how recently this one
        #: started.
        self.connected_since: datetime | None = None
        self.disconnects = 0
        #: When the receiver last heard anything at all. The one reading that
        #: says whether the receive path works, rather than whether the daemon
        #: is answering.
        self.last_rx_frame: datetime | None = None

    def add_availability_listener(
        self, listener: Callable[[bool], None]
    ) -> Callable[[], None]:
        """Subscribe to availability changes, returning an unsubscribe."""
        self._availability_listeners.append(listener)

        def unsubscribe() -> None:
            with contextlib.suppress(ValueError):
                self._availability_listeners.remove(listener)

        return unsubscribe

    def add_update_listener(self, listener: Callable[[], None]) -> Callable[[], None]:
        """Subscribe to any observable change, returning an unsubscribe."""
        self._update_listeners.append(listener)

        def unsubscribe() -> None:
            with contextlib.suppress(ValueError):
                self._update_listeners.remove(listener)

        return unsubscribe

    def _notify(self) -> None:
        for listener in list(self._update_listeners):
            listener()

    @property
    def available(self) -> bool:
        """Whether the daemon is currently connected."""
        return self._connected.is_set()

    @property
    def url(self) -> str:
        """The daemon's WebSocket URL."""
        return self._url

    async def async_start(self) -> None:
        """Begin connecting, and keep the connection up."""
        self._closing = False
        self._task = asyncio.create_task(self._run())

    async def async_stop(self) -> None:
        """Disconnect and stop reconnecting."""
        self._closing = True
        if self._task is not None:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task
            self._task = None
        if self._socket is not None:
            await self._socket.close()

    async def async_wait_connected(self, timeout: float) -> None:
        """Wait for the first successful connection."""
        async with asyncio.timeout(timeout):
            await self._connected.wait()

    async def async_status(self) -> dict[str, Any]:
        """Ask the daemon what state it is in."""
        return await self._request({"type": "status"})

    async def async_transmit(
        self,
        *,
        frequency: int,
        timings: list[int],
        repeat: int = 0,
        gap_us: int | None = None,
        output_power: float | None = None,
    ) -> int:
        """Transmit OOK timings, returning the air time in microseconds.

        Returns when the transmission is over, not when it is queued.
        """
        request: dict[str, Any] = {
            "type": "transmit",
            "frequency": frequency,
            "timings": timings,
            "repeat": repeat,
        }
        if gap_us is not None:
            request["gap_us"] = gap_us
        if output_power is not None:
            # The platform expresses power as a 0..1 fraction; the daemon takes
            # the radio's own 0..47 dB TX VGA setting.
            request["txvga_db"] = max(0, min(47, round(output_power * 47)))

        reply = await self._request(request)
        return int(reply.get("duration_us", 0))

    async def _request(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Send a request and await the reply that carries its id."""
        socket = self._socket
        if socket is None or not self._connected.is_set():
            raise ProxyError(f"not connected to {self._url}")

        self._next_id += 1
        request_id = str(self._next_id)
        message = {"v": PROTOCOL_VERSION, "id": request_id, **payload}

        future: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        try:
            await socket.send_json(message)
            async with asyncio.timeout(REQUEST_TIMEOUT):
                reply = await future
        except TimeoutError as err:
            raise ProxyError(f"{self._url} did not answer in time") from err
        except aiohttp.ClientError as err:
            raise ProxyError(f"failed to reach {self._url}: {err}") from err
        finally:
            self._pending.pop(request_id, None)

        if reply.get("type") == "error":
            raise ProxyError(str(reply.get("message", "unspecified error")))
        return reply

    async def _run(self) -> None:
        """Keep a connection up, reconnecting with a widening backoff."""
        backoff = _INITIAL_BACKOFF
        while not self._closing:
            try:
                async with self._session.ws_connect(self._url, heartbeat=30) as socket:
                    _LOGGER.debug("connected to %s", self._url)
                    self._socket = socket
                    backoff = _INITIAL_BACKOFF
                    await self._read_until_closed(socket)
            except asyncio.CancelledError:
                raise
            except (aiohttp.ClientError, OSError) as err:
                _LOGGER.debug("connection to %s failed: %s", self._url, err)
            finally:
                self._socket = None
                # Only a connection that was actually established counts as a
                # drop. Otherwise a daemon that is simply not running yet would
                # add one per backoff interval and read as a flapping link.
                if self.connected_since is not None:
                    self.disconnects += 1
                self.connected_since = None
                self.state = None
                self._set_available(False)
                self._notify()
                self._fail_pending(ProxyError(f"disconnected from {self._url}"))

            if self._closing:
                return
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, _MAX_BACKOFF)

    async def _read_until_closed(self, socket: aiohttp.ClientWebSocketResponse) -> None:
        """Dispatch messages until the connection ends."""
        # Only report available once the daemon has actually answered, rather
        # than when the socket opens: a TCP connection to a wedged daemon would
        # otherwise look healthy.
        try:
            status = await asyncio.wait_for(_first_status(socket), timeout=10)
        except (TimeoutError, aiohttp.ClientError):
            _LOGGER.debug("%s did not answer a status request", self._url)
            return
        self.device = status.get("device")
        self.daemon_version = status.get("daemon_version")
        self.state = status.get("state")
        self.connected_since = dt_util.utcnow()
        self._set_available(True)
        self._notify()

        async for message in socket:
            if message.type is not aiohttp.WSMsgType.TEXT:
                continue
            try:
                payload = message.json()
            except ValueError:
                _LOGGER.warning("%s sent malformed JSON", self._url)
                continue
            self._dispatch(payload)

    def _dispatch(self, payload: dict[str, Any]) -> None:
        """Route a message to whoever is waiting for it."""
        request_id = payload.get("id")
        if request_id is not None and (future := self._pending.get(str(request_id))):
            if not future.done():
                future.set_result(payload)
            return

        kind = payload.get("type")
        if kind == "rx_frame":
            self.last_rx_frame = dt_util.utcnow()
            self._notify()
            if self._on_rx_frame is not None:
                self._on_rx_frame(payload)
        elif kind == "device_state":
            # The radio can fault while the connection stays up. Availability
            # follows the radio, because a transmitter that cannot transmit is
            # not available in any sense a consumer cares about.
            self.state = payload.get("state")
            self._set_available(self.state != "faulted")
            self._notify()
        elif kind == "error":
            _LOGGER.warning("%s reported: %s", self._url, payload.get("message"))

    def _set_available(self, available: bool) -> None:
        if available == self._connected.is_set():
            return
        if available:
            self._connected.set()
        else:
            self._connected.clear()
        for listener in list(self._availability_listeners):
            listener(available)

    def _fail_pending(self, error: Exception) -> None:
        for future in self._pending.values():
            if not future.done():
                future.set_exception(error)
        self._pending.clear()


async def _first_status(socket: aiohttp.ClientWebSocketResponse) -> dict[str, Any]:
    """Send one status request and read until its reply arrives.

    Used only during connection setup, where nothing else is in flight yet;
    events that arrive meanwhile are dropped rather than dispatched, which is
    harmless because the entity has not been told it is available.
    """
    await socket.send_json({"v": PROTOCOL_VERSION, "id": "hello", "type": "status"})
    async for message in socket:
        if message.type is not aiohttp.WSMsgType.TEXT:
            continue
        try:
            payload = message.json()
        except ValueError:
            continue
        if payload.get("id") == "hello":
            return payload
    raise aiohttp.ClientError("connection closed during handshake")
