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
  Marketing names (Proflame 2 / Proflame Pro / GTM…) are noisy; what matters
  is our own captures already matched the smartfire packet structure.
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
  options. Works with **any** 315 MHz-capable transmitter, not just ours
  (e.g. a future CC1101 ESPHome node — nice resilience property).
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

1. **M1 — daemon TX spike**: driver crate bake-off (seify-hackrfone vs
   rs-hackrf), OOK burst TX from a timings JSON, CLI replay of the recorded
   Proflame capture → does the fireplace respond? (Go/no-go for everything.)
2. **M2 — daemon proper**: WS API, half-duplex arbiter, RX → timing frames,
   quadlet deployment.
3. **M3 — `hackrf_proxy` integration**: transmitter entity + availability;
   verify a stock consumer would accept it (config-flow filter).
4. **M4 — `proflame_protocol` + `proflame` TX**: encoder w/ capture-based
   unit tests; entities; control the fireplace from HA.
5. **M5 — RX state sync**: decoder, dispatcher bridge, loop protection;
   physical remote and HA converge.
6. **M6 — polish**: reconnect robustness, diagnostics, docs, upstream PRs.

## 6. Open questions

- Driver crate choice (M1 spike decides; both are nusb/pure-Rust).
- TX sample synthesis rate & gain defaults (start from old prototype's
  2 Msps; verify against capture SNR).
- Whether daemon should support multiple simultaneous RX frequencies
  (HackRF is single-tuner: no. Document as limitation; per-frame `frequency`
  field keeps the API honest for future multi-SDR).

[arch-1365]: https://github.com/home-assistant/architecture/discussions/1365
[release-2026-5]: https://www.home-assistant.io/blog/2026/05/06/release-20265/
[dev-blog]: https://developers.home-assistant.io/blog/2026/04/24/radio-frequency-entity-platform/
[rf-protocols]: https://github.com/home-assistant-libs/rf-protocols
[smartfire]: https://github.com/johnellinwood/smartfire
[seify-hackrfone]: https://github.com/MerchGuardian/seify-hackrfone
[rs-hackrf]: https://lib.rs/crates/rs-hackrf
