# hackrf-proxy-client

Async Python client for the
[hackrf-proxyd](https://github.com/Aetf/hackrf-proxy) daemon's WebSocket
protocol: a reconnecting connection with id-matched replies, `rx_frame` event
delivery, and availability that follows the radio rather than the socket.

Released in lockstep with the daemon from the same repository; a client is
compatible with any daemon of the same semver major version, and
`ProxyClient.is_compatible` reports the check.

```python
import aiohttp
from hackrf_proxy_client import ProxyClient

async with aiohttp.ClientSession() as session:
    client = ProxyClient(session, "radio-host", 8765)
    await client.async_start()
    await client.async_wait_connected(timeout=10)
    await client.async_transmit(frequency=315_000_000, timings=[450, -450, 900])
    await client.async_stop()
```

## License

MIT OR Apache-2.0, at your option.
