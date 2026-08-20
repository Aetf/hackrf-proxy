# hackrf-proxy — Architecture

What this repository is: a network-attached OOK transceiver. A Rust daemon
(`hackrf-proxyd`, binary `hrf`) owns a HackRF One over USB and serves a
WebSocket API that moves raw signed-microsecond timings; a lockstep-versioned
Python client (`hackrf-proxy-client`) speaks that API. The daemon is
deliberately protocol-agnostic — it knows nothing about any appliance, which
is what makes it a shared radio rather than one device's bridge.

## The ecosystem

Four repositories, one seam each:

| repo | role |
|------|------|
| [hackrf-proxy](https://github.com/Aetf/hackrf-proxy) (this one) | the daemon and its Python client |
| [hass-hackrf-proxy](https://github.com/Aetf/hass-hackrf-proxy) | Home Assistant `radio_frequency` transmitter entity over the client |
| [proflame](https://github.com/Aetf/proflame) | the SIT Proflame 2 protocol, as a transceiver-agnostic library |
| [hass-proflame](https://github.com/Aetf/hass-proflame) | the fireplace as Home Assistant entities, over any OOK transmitter |

```
┌────────────────────────────── Home Assistant ────────────────────────────────┐
│  hass-proflame (consumer)                     other consumers (novy, ...)    │
│   encode state→timings   decode RX→state               │                     │
│        │ async_send_command                            │                     │
│  ════ radio_frequency platform (core, HA ≥2026.5) ═════════════              │
│        │                                     ▲ dispatcher signal             │
│        ▼                                     │ (RX frames, interim)          │
│  hass-hackrf-proxy (transmitter, thin)  ──── hackrf-proxy-client (PyPI)      │
└────────┼─────────────────────────────────────┼───────────────────────────────┘
         │            WebSocket (JSON)         │
┌─ hackrf-proxyd (Rust daemon, container or static binary) ────────────────────┐
│  half-duplex arbiter: RX-idle by default, TX preempts, auto-return to RX     │
│  TX: timings → OOK modulate → IQ → HackRF        RX: IQ → magnitude          │
│  threshold → pulse widths → raw timing frames → event stream                 │
└──────────────────────────────┬───────────────────────────────────────────────┘
                               │ USB (nusb, pure Rust userspace)
                           HackRF One
```

The `radio_frequency` platform (HA 2026.5, [architecture #1365][arch-1365])
decouples RF hardware from device logic the way Bluetooth proxies decouple
BLE radios from device integrations. This daemon slots in as one more
transmitter; any consumer filtered by frequency range and OOK can use it.
What the platform does not provide — RX, and a way to reach a radio hanging
off another machine's USB port — is exactly what the daemon adds. RX frames
are re-broadcast by the transmitter integration on a dispatcher signal as an
interim path; the payload shape matches what #1365 sketches for a future
receiver platform, so consumers migrate with little churn when one lands.

## Crate layering

Strict, and it is what keeps the project testable without a radio on the
bench:

- `ook.rs` — signal processing, IQ ↔ timings, offline and streaming. Pure.
- `wire.rs` — the WebSocket protocol's types and validation. Pure.
- `engine.rs` — the half-duplex arbiter, over a `Transceiver` trait, tested
  against a fake device: preemption ordering, retuning, fault recovery.
- `server.rs` — the WebSocket front end, one task per connection.
- `radio.rs` — the only module that needs a HackRF plugged in.

Decisions that were not obvious and are load-bearing:

- **A dedicated OS thread owns the radio, not a tokio task.** The driver's
  I/O is blocking, and a transmission holds it for the best part of a second.
  Arbitration then needs no locks, since it *is* that thread's control flow;
  and a parked tokio worker would stall unrelated connections.
- **The streaming receiver cannot reuse the offline threshold logic.** The
  offline path is two-pass. Live, the threshold must be bounded below by a
  statistic a burst cannot move (the median-to-third-quartile noise spread),
  or quiet windows read as signal and the threshold lands inside the noise.
- **Recovery is only believed once a transfer has actually arrived.** A
  HackRF with broken bulk streaming still answers control transfers, so an
  identity probe proves nothing; the backoff clears on data, and USB hotplug
  events merely reset the retry timer rather than declaring health.
- **Pure-Rust USB (nusb), zero C dependencies.** No libhackrf, no SoapySDR,
  no libudev: the container is a small static musl image and the binary
  relocates by copying.

## Wire protocol and versioning

`proxyd/README.md` is the wire reference. The envelope carries a protocol
version (`v`) the daemon refuses by name rather than misreading. Client and
daemon are released in lockstep from this repository's `vX.Y.Z` tags, and the
compatibility contract is **same semver major**; the client checks the
daemon's reported version at connect. Wire-protocol changes are breaking
changes (`feat!:`) by definition.

The two contracts a client must honor, both easy to get wrong in ways that
appear to work: match replies by `id` (a transmission's own `device_state`
event overtakes its reply), and treat availability as following the radio
rather than the socket (the daemon keeps serving with a faulted radio).

## Why not X

- **Rust-in-HA via a pip wheel** (maturin/PyO3): feasible, wrong boundary.
  It would demand the HackRF hang off the Home Assistant box, put DSP in
  HA's process, and delete the shared-network-radio property that motivates
  the project.
- **MQTT as the transport**: `async_send_command` must raise on failure, so
  the caller needs request/reply; MQTT would need response topics plus a
  broker in the control path. Discovery, MQTT's real advantage, would
  require publishing appliance semantics — the layering this design exists
  to avoid.
- **Squelching the receiver when nobody listens**: it costs only USB
  bandwidth, and following a handset is the one thing the receiver is for.

## Deployment

The `deploy/` directory carries the Containerfile (two-stage, static musl), a
rootless-podman quadlet unit pulling `ghcr.io/aetf/hackrf-proxyd`, the udev
rule (with the USB-autosuspend and TLP traps documented inline), and a
wrapper for running the containerized CLI at a bench. The daemon also runs
fine as a bare static binary. Host sizing: OOK at 2 Msps is a magnitude
threshold plus pulse widths — about 4% of one small core and 6 MB — but the
USB bus must sustain 4 MB/s, which rules out single-bus boards that share it
with their network interface.

## Open questions

- **Authentication.** The daemon is unauthenticated on the LAN — the ESPHome
  posture, except this can transmit. Documented in the README's Security
  section; a token handshake is the likely shape and will be a protocol
  major bump.
- **A second receiver.** The HackRF is deaf while it transmits, so questions
  about reply/echo behavior of appliances need a receiver that is never the
  transmitter.

[arch-1365]: https://github.com/home-assistant/architecture/discussions/1365
