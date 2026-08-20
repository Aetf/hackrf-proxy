# hackrf-proxy

[![CI](https://github.com/Aetf/hackrf-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/Aetf/hackrf-proxy/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hackrf-proxyd)](https://crates.io/crates/hackrf-proxyd)
[![PyPI](https://img.shields.io/pypi/v/hackrf-proxy-client)](https://pypi.org/project/hackrf-proxy-client/)

Make a HackRF One a shared, network-attached RF transceiver — the way an
ESPHome node is a Bluetooth proxy. A Rust daemon owns the radio and serves a
WebSocket API that moves raw OOK timings; a Python client library speaks it.
Built for Home Assistant's `radio_frequency` platform, useful to anything
that wants a sub-GHz OOK radio on the network.

Two artifacts, released in lockstep from this repository:

- **`hackrf-proxyd`** ([crates.io](https://crates.io/crates/hackrf-proxyd)) —
  the daemon and the `hrf` CLI. Also published as a container image,
  `ghcr.io/aetf/hackrf-proxyd`, and as static musl binaries on each release.
- **`hackrf-proxy-client`** ([PyPI](https://pypi.org/project/hackrf-proxy-client/))
  — an async Python client: reconnecting connection, id-matched replies,
  availability that follows the radio rather than the socket.

A client works with any daemon of the same semver major version.

Related repositories:
[hass-hackrf-proxy](https://github.com/Aetf/hass-hackrf-proxy) (the Home
Assistant transmitter integration),
[proflame](https://github.com/Aetf/proflame) and
[hass-proflame](https://github.com/Aetf/hass-proflame) (the first consumer:
a SIT Proflame gas fireplace). `docs/DESIGN.md` maps how the pieces fit.

## Security

The daemon has **no authentication**: anyone who can reach its WebSocket port
can tune the radio and, more importantly, **transmit**. This is the same trust
posture as `rtl_tcp` or an ESPHome node — it is designed to sit on a trusted
LAN segment, not to be exposed.

Deployment rules until authentication lands (it is on the roadmap):

- Bind to a specific trusted interface, or to `127.0.0.1` behind a reverse
  proxy/tunnel, rather than `0.0.0.0` on a machine with untrusted networks.
- Firewall the listen port so only the Home Assistant host can reach it.
- Do not port-forward or otherwise expose the daemon to the internet.

Transmitting is also regulated everywhere: what you may send, at which power,
on which band, is yours to know for your jurisdiction.

## Running the daemon

Install from a release asset, `cargo install hackrf-proxyd --locked`, or pull
the container; `deploy/` has a rootless-podman quadlet unit and the udev rule
the radio needs (with the USB-autosuspend trap documented inline).

    hrf serve --listen 0.0.0.0:8765 --rx-freq 315M
    tools/wsprobe.py --host radio-host --listen     # watch what it hears

The daemon receives continuously by default, publishing every burst it hears
as an `rx_frame` event, and lets clients preempt with `transmit` requests: a
half-duplex arbiter receives by default, hands the radio to a transmission,
and takes it back. It survives the radio faulting, re-enumerating or
unplugging, and reports what it is doing over the same API. The wire protocol
is documented in `proxyd/README.md`.

## The bench tools

The rest of the `hrf` subcommands are for protocol work against a captured
appliance: `info`, `scan` (a live meter across bands — hold the button down
and see which band moves), `capture` (IQ to disk), `demod` (offline OOK
demodulation with a pulse-width histogram, plus `--stream` to replay a
capture through the live receiver's own code path), and `transmit` (replay a
timings file).

One flag worth knowing before it costs an evening: `demod --gap-us` must be
below the inter-frame gap of the protocol at hand, or repeats merge into one
undecodable blob.

## Development

    cargo test --manifest-path proxyd/Cargo.toml       # 49 tests, no hardware needed
    cd clients/python && uv sync && uv run pytest

Everything except `radio.rs` is tested without a radio, the arbiter included.
The Python suite runs the client against a scripted daemon and pins the
protocol version against `wire.rs`, so the two ends of the wire cannot drift
apart in this repository unnoticed.

Commits follow conventional commits; releasing is
[release-plz](https://release-plz.dev/) — merging the standing release PR
publishes crates.io, PyPI, the container and the binaries in one motion.

## License

MIT OR Apache-2.0, at your option.
