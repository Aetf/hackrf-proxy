# HackRF Proxy — Design Document

Status: **Draft for review** (2026-08-16)

Goal: make a HackRF One a shared, network-attached RF transceiver for Home
Assistant — analogous to an ESPHome Bluetooth proxy — and on top of it,
integrate a SIT Proflame fireplace (Proflame Pro remote, 315 MHz) as
first-class HA entities with bidirectional state sync.

This supersedes the earlier `proflame-mqtt` prototype (Rust + MQTT discovery,
on the XPS laptop). That design predates HA's native RF platform; its protocol
research (packet capture, checksum, Manchester/OOK parameters, remote ID
`0x008602`) carries over, its MQTT transport does not.

---

## 1. Upstream landscape (researched 2026-08-16)

### 1.1 The `radio_frequency` entity platform (HA 2026.5)

Approved in [architecture discussion #1365][arch-1365], shipped in
[HA 2026.5][release-2026-5], documented in the
[developer blog post][dev-blog]. Key properties, verified against
`homeassistant/components/radio_frequency/` in core:

- **Transmitter side**: hardware integrations subclass
  `RadioFrequencyTransmitterEntity` and implement:
  - `supported_frequency_ranges -> list[tuple[int, int]]` (Hz)
  - `async_send_command(command: RadioFrequencyCommand)`
  - Entity state = ISO timestamp of last transmitted command
    (restored across restarts via `RestoreEntity`).
  - `supports_modulation()` is `@final` and hardcoded to **OOK only** for now.
- **Command format**: `RadioFrequencyCommand` from the external
  [`rf-protocols`][rf-protocols] library (PyPI, pure Python):
  `frequency`, `modulation` (OOK), `repeat_count` (additional repeats),
  `symbol_rate?`, `output_power?`, and `get_raw_timings() -> list[int]` —
  **signed alternating microseconds, Flipper RAW `.sub` convention**
  (positive = mark/carrier-on, negative = space/carrier-off).
- **Consumer side**: device integrations declare `radio_frequency` as a
  dependency, call `async_get_transmitters(hass, frequency, modulation)` in
  their config flow to offer a transmitter picker (EntitySelector), store the
  transmitter's **entity registry id**, and derive entities from
  `RadioFrequencyTransmitterConsumerEntity` (tracks transmitter availability
  and renames; `_send_command()` helper).
- **RX is explicitly out of scope** upstream. #1365 sketches a future
  "Radio Frequency Receiver Entity Platform" based on events carrying raw
  timings + frequency + RSSI/LQI, but as of 2026-08-16 no concrete proposal
  exists in home-assistant/architecture. We must bridge RX ourselves for now
  (see §3.4) and keep the shape migration-friendly.

### 1.2 Existing transmitters and consumers

- Transmitters: **ESPHome** (native API `radio_frequency_transmit_raw_timings`,
  advertises frequency range + TRANSMITTER capability; e.g. CC1101-based
  nodes) and **Broadlink RM4 Pro**.
- Consumers shipped with 2026.5: `honeywell_string_lights`, `novy` (cooker
  hood). `honeywell_string_lights/config_flow.py` is the canonical consumer
  pattern to copy.
- `rf-protocols` today: encoders for CAME, EV1527, PT2262, KaKu, Somfy RTS,
  Hörmann, Marantec, Harbor Breeze, Novy, Pilota Casa + fixed code tables.
  Encoders are classes returning timings at runtime, so **dynamic,
  state-dependent packets (like Proflame's) fit fine** — it is not just a
  static code database. Note the repo has an `AI_POLICY.md` — read it before
  preparing an upstream PR.
- No HackRF/SDR transmitter integration exists upstream or (as far as
  searched) in the community. We are first.

### 1.3 Proflame protocol (from previous iteration, verified by capture)

- 315.00 MHz, OOK, Manchester encoding. No rolling code; every packet carries
  the **full state** + checksum keyed on state + remote serial.
- User's remote ID: `0x008602`. Reference capture (manual mode, fire on,
  flame 5) recorded in the old `project_context.md`.
- Reference implementation: [smartfire][smartfire] (Python, Proflame 2).
  Marketing names (Proflame 2 / Proflame Pro / GTM…) are noisy.
- **Trust level: low.** Per user (2026-08-16), the previous protocol research
  has known issues and unresolved gaps. Treat every bullet above as a
  hypothesis; M1 includes a fresh wideband capture + analysis pass to
  re-derive framing/checksum before any encoder is written.
- Physical remote must stay in **manual mode** (thermostat mode makes it
  transmit autonomously and fight HA).

---

## 2. Where HackRF fits

The platform decouples "RF hardware" from "device logic" exactly like
Bluetooth proxies decouple BLE radios from device integrations. HackRF slots
in as **one more transmitter entity** — any present or future consumer
integration (Novy, Honeywell, our Proflame, …) can then use it, filtered by
frequency range (HackRF: 1 MHz – 6 GHz, covers every sub-GHz band) and OOK.

What the platform does *not* give us: RX, and a transport for a radio that
hangs off another machine's USB port. Hence a three-layer design.

---

## 3. Architecture

```
┌────────────────────────────── Home Assistant (HAOS) ─────────────────────────┐
│                                                                              │
│  proflame (consumer custom integration)        novy / honeywell / future...  │
│   light·fan·light·switch entities                      │                     │
│   encode state→timings   decode RX→state               │                     │
│        │            ▲                                  │                     │
│        ▼ async_send_command                            ▼                     │
│  ════ radio_frequency platform (core) ═══════════════════════                │
│        │                                     ▲ dispatcher signal             │
│        ▼                                     │ (RX frames, interim)          │
│  hackrf_proxy (transmitter custom integration, thin pure-Python client)      │
└────────┼─────────────────────────────────────┼───────────────────────────────┘
         │            WebSocket (JSON)         │
         ▼                                     │
┌─ hackrf-proxyd (Rust daemon, homelab server, quadlet container) ─────────────┐
│  half-duplex arbiter: RX-idle by default, TX preempts, auto-return to RX     │
│  TX: timings → OOK modulate → IQ → HackRF        RX: IQ → magnitude          │
│  threshold → pulse widths → raw timing frames → event stream                 │
└──────────────────────────────┬───────────────────────────────────────────────┘
                               │ USB (nusb, pure Rust userspace)
                           HackRF One
```

### 3.1 `hackrf-proxyd` — Rust daemon (owns the hardware)

- **Protocol-agnostic**: moves raw timings only; knows nothing about Proflame.
  This is what makes it a shared proxy rather than a fireplace bridge.
- **Driver**: pure-Rust nusb-based HackRF driver — evaluate
  [`seify-hackrfone`][seify-hackrfone] (also usable standalone, without the
  seify layer) vs [`rs-hackrf`][rs-hackrf] (claims mature multi-transfer RX
  streaming). Zero C dependencies (no libhackrf/SoapySDR) keeps the container
  image and build trivial. Decision deferred to a short spike (M1).
- **DSP kept in the daemon**, hand-rolled (no FutureSDR): OOK at 2 Msps is a
  magnitude threshold + pulse-width measurement; network carries compact
  timing frames, not IQ.
- **Runtime**: tokio. TX path: timings → sample synthesis → burst TX.
  RX path: continuous stream → squelch/threshold → frames
  `{frequency, timings[], rssi, timestamp}` — deliberately the same shape
  #1365 sketches for future upstream receiver events.
- **API**: WebSocket + JSON on the LAN (same style as thread-dashboard;
  debuggable with websocat). Requests: `transmit{frequency, timings,
  repeat_count, output_power?}`, `configure_rx{frequency, enabled}`,
  `status`. Server-push: `rx_frame`, `device_state`. Versioned envelope from
  day one. Binary framing (msgpack) only if JSON ever measures too slow — it
  won't at these rates.
- **Half-duplex arbitration** lives here (single owner of the device):
  default RX-idle, TX requests preempt, hardware settle, resume RX.
  Single-flight TX queue.
- **Deployment**: container via quadlet on the homelab server (HackRF on its
  USB), like thread-dashboard. Config: listen addr, RX frequency default,
  gains.
- **Host selection (decided 2026-08-16)**: NOT the Pi 1 (`rpi`). It is a
  Model B Rev 2: single-core ARMv6 700 MHz, no NEON, and one dwc_otg USB bus
  shared by the LAN9512 Ethernet AND the Sonoff RCP driving an OTBR. HackRF's
  minimum 2 Msps ≈ 4 MB/s sustained would saturate CPU with USB interrupts,
  starve the RCP (spinel timeouts → OTBR flaps) and drop Ethernet. The
  interference risk with Thread is bus/CPU contention, not RF (2.4 GHz vs
  315 MHz). Fallback hosts if homelab RF range is insufficient: Pi Zero 2
  W / Pi 3/4 (quad A53 + NEON handle 2 Msps OOK easily). ESP32 (any, incl.
  C6) cannot host a HackRF: no/insufficient USB host (C6 has none; S2/S3 OTG
  is 12 Mbps full-speed < 32 Mbps required).

### 3.2 `hackrf_proxy` — HA transmitter custom integration (thin)

- Config flow: host/port (+ zeroconf later). Pure-Python WS client
  (`aiohttp`, already in HA) — **no native dependencies in HA**.
- One `RadioFrequencyTransmitterEntity`: `supported_frequency_ranges =
  [(1_000_000, 6_000_000_000)]`; `async_send_command` forwards
  `command.get_raw_timings()` + frequency + repeats over WS.
- Availability tracks the WS connection (auto-reconnect with backoff).
- RX bridge (interim, until upstream receiver platform exists): daemon
  `rx_frame` events → `async_dispatcher_send(hass,
  f"hackrf_proxy_rx_{entry_id}", frame)`. Consumers subscribe via dispatcher.
  When upstream lands, this becomes a receiver entity and consumers migrate
  with minimal churn (same payload shape).

### 3.3 `proflame` — consumer custom integration

Follows the `honeywell_string_lights` pattern exactly on the TX side:

- Config flow: `async_get_transmitters(hass, 315_000_000, OOK)` → transmitter
  picker; plus remote ID (default from capture) and installer-relevant
  options. Works with **any** 315 MHz-capable transmitter, not just ours.
- **CC1101 endgame option**: ESPHome ≥2025.12 has an official [`cc1101`
  component][esphome-cc1101] (SPI transceiver, 300–348/387–464/779–928 MHz,
  OOK via remote_transmitter/remote_receiver), and ESPHome is already a
  native `radio_frequency` transmitter. An ESP32-C6 + CC1101 (~$3 module)
  near the fireplace could later become the dedicated permanent transceiver
  — no daemon in the TX path at all — while the HackRF proxy remains the
  general research/proxy tool. `aioesphomeapi` already defines
  `RadioFrequencyCapability.RECEIVER`, so upstream ESPHome RX is being
  plumbed; if/when it lands, RX state sync could migrate there too. The
  transmitter-picker pattern makes the swap a config-flow change, not a
  rewrite.
- Entities (mirroring the fireplace's actual controls):
  - `light` main flame (brightness ↔ flame 0–6)
  - `fan` blower (percentage ↔ 0–6)
  - `light` accent light (0–6)
  - `switch` aux / split-flow
  - later: `climate` for thermostat mode, `switch` pilot
- Full-state packets: integration holds a `ProflameState` dataclass; every
  command re-encodes the whole state → Manchester → timings → `OOKCommand`
  → `_send_command()`.
- **RX state sync** (the reason we bother with RX at all): subscribe to the
  hackrf_proxy dispatcher signal, decode Manchester → packet → checksum →
  if remote ID matches, update `ProflameState` and all entity states. The
  physical remote stays a fully-functional fallback and HA never drifts.
  Loop protection: ignore frames matching our own just-transmitted packet
  within a short window.
- **Protocol encoder/decoder: pure Python**, in its own module
  (`proflame_protocol/`) with unit tests against the recorded captures. Pure
  Python because (a) it's trivial bit-twiddling, (b) it is the upstream
  candidate for `rf-protocols` (pure-Python repo) — the decoder half stays
  local until upstream RX exists.

### 3.4 What we answer to "Rust but pip-installable?"

Feasible — maturin + PyO3 abi3 + nusb gives self-contained manylinux wheels
with zero system deps — but **not the right boundary here**. Putting the
radio in HA's process via a pip requirement would demand the HackRF be
plugged into the HAOS box (USB passthrough + udev pain), put real-time DSP
inside HA's event loop host, and kill the "shared proxy on the network"
property that motivated the project. Rust stays in the daemon; both HA
integrations stay pure Python. (A maturin wheel of the DSP core remains an
option later for a debugging/capture CLI, not for HA.)

---

## 4. Repository & maintenance strategy

Monorepo `~/hackrf-proxy`:

```
hackrf-proxy/
├── docs/DESIGN.md              # this file
├── proxyd/                     # Rust daemon (cargo workspace member)
├── custom_components/
│   ├── hackrf_proxy/           # transmitter integration
│   └── proflame/               # consumer integration
├── proflame_protocol/          # pure-python encode/decode + tests
└── deploy/                     # quadlet unit, container build
```

- Monorepo keeps daemon API and client in lockstep while the WS protocol is
  young. If HACS distribution is ever wanted, integrations split out cleanly
  (HACS wants one integration per repo) — defer until someone else wants it.
- Long-term upstreaming path, in order of plausibility:
  1. Proflame encoder → `rf-protocols` (mind `AI_POLICY.md`);
  2. `proflame` consumer integration → HA core (quality-scale rules);
  3. transmitter integration → core only if/when a receiver platform exists
     and the WS protocol is stable. None of this blocks local use.
- Deployment of custom components to HAOS: rsync/`hab` into
  `/config/custom_components/` (same access path as existing HAOS work).

## 5. Milestones

1. ~~**M1 — spike: capture, re-verify protocol, TX**~~ **done 2026-08-16.**
   Driver is seify-hackrfone; `proxyd/` carries the `hrf` CLI. The remote was
   captured fresh and the protocol derived from scratch (docs/PROTOCOL.md),
   which independently reproduced the inherited checksum model and the remote
   id. Replaying a captured frame ignited the fireplace, so RX and TX are both
   proven. The off command was captured on 2026-08-16, closing the one gap this
   phase left: `cmd1` bit 0 is on/off, so RF can now both start and stop the
   appliance by verbatim replay.
2. ~~**M2 — daemon proper**~~ **done 2026-08-17.** `hrf serve`: WS API with a
   versioned envelope, half-duplex arbiter on a dedicated thread, streaming RX
   to timing frames, quadlet unit. Protocol documented in `proxyd/README.md`.
   The arbiter is tested against a fake device, so preemption, retuning and
   fault recovery are covered without hardware. Two decisions worth recording
   here because they were not obvious from this document:
   - **A dedicated OS thread owns the radio, not a tokio task.** The driver's
     I/O is blocking and a transmission holds it for the best part of a second.
     Arbitration then needs no locks, since it is that thread's control flow.
   - **The streaming receiver cannot reuse the offline threshold logic.** The
     offline path is two-pass. Live, the threshold has to be bounded below by a
     statistic a burst cannot move (the median-to-third-quartile noise spread),
     or quiet windows read as signal and the threshold lands inside the noise.
3. **M3 — `hackrf_proxy` integration**: transmitter entity + availability;
   verify a stock consumer would accept it (config-flow filter).
4. **M4 — `proflame_protocol` + `proflame` TX**: the Rust protocol half is
   ~~done 2026-08-16~~ (`proxyd/src/proflame.rs`, both directions, pinned by
   regression tests against `tests/`). Remaining: the HA consumer integration —
   config flow with a transmitter picker, and entities.
5. **M5 — RX state sync**: decoder, dispatcher bridge, loop protection;
   physical remote and HA converge. The daemon side already exists: `rx_frame`
   events carry the timings, and `proflame::decode` turns them into frames.
6. **M6 — polish**: reconnect robustness, diagnostics, docs, upstream PRs.

## 6. Open questions

Resolved:

- ~~Driver crate choice~~ — seify-hackrfone; `rs-hackrf` is receive-only.
- ~~TX sample synthesis rate & gain defaults~~ — 2 Msps, TX VGA 30 dB with the
  amplifier off, which is what ignited the fireplace at living-room range. The
  daemon takes both as options and a request may override the gain.
- ~~Multiple simultaneous RX frequencies~~ — no, the HackRF is single-tuner.
  `configure_rx` retunes the one receiver; the per-frame `frequency` field keeps
  the API honest for a future multi-radio deployment.

Still open:

- Whether the receiver should ever be squelched automatically while Home
  Assistant is not listening. It costs nothing but USB bandwidth today.
- Authentication. The daemon is unauthenticated on the LAN, which is the same
  posture as an ESPHome node, but it can *transmit* — the exposure is worth a
  decision before it moves to a less trusted network.

[arch-1365]: https://github.com/home-assistant/architecture/discussions/1365
[release-2026-5]: https://www.home-assistant.io/blog/2026/05/06/release-20265/
[dev-blog]: https://developers.home-assistant.io/blog/2026/04/24/radio-frequency-entity-platform/
[rf-protocols]: https://github.com/home-assistant-libs/rf-protocols
[smartfire]: https://github.com/johnellinwood/smartfire
[esphome-cc1101]: https://esphome.io/components/cc1101/
[seify-hackrfone]: https://github.com/MerchGuardian/seify-hackrfone
[rs-hackrf]: https://lib.rs/crates/rs-hackrf
