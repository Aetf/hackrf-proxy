# hackrf-proxy

Make a HackRF One a shared, network-attached RF transceiver for Home Assistant,
the way an ESPHome node is a Bluetooth proxy, and on top of it integrate a SIT
Proflame gas fireplace as first-class HA entities with two-way state sync.

## Where things stand (2026-08-18)

**M1 through M5 are done, and the fireplace is driven from Home Assistant in
daily use.** The protocol is solved and both directions are proven on real
hardware: a frame captured from the remote, replayed by the HackRF, ignited the
fireplace from cold. The daemon exists, runs, and has been exercised against
the real radio over the network — receiving for hours, retuning, transmitting
and handing the radio back, and recovering from several hundred real USB faults
without help. Both Home Assistant integrations are installed and working: the
fireplace appears as a switch, a flame, a blower, a light, a thermostat and an
auto-off timer, it follows the handset by decoding what the receiver hears, and
it re-asserts its state on a timer so the appliance cannot quietly drift away
from what Home Assistant believes.

What is left is M6 — polish. The transmitter integration has no diagnostic
entities, so a connection problem is only visible in the log; the daemon is
unauthenticated on the LAN while being able to transmit, which wants a decision
before it moves anywhere less trusted; and nothing has been offered upstream.

What exists:

- `proxyd/` — a library crate with `hrf` on top. `hrf serve` is the daemon: a
  protocol-agnostic WebSocket radio proxy with a half-duplex arbiter, a
  streaming receiver and fault recovery. The rest of the subcommands are the
  bench tools — `info`, `scan`, `capture`, `demod`, `decode`, `transmit`.
  Everything but `radio.rs` is tested without hardware, the arbiter included.
  See `proxyd/README.md` for the wire protocol.
- `docs/PROTOCOL.md` — the Proflame protocol, solved: framing, checksums and
  every command field.
- `docs/MAPPING.md` — how to confirm the remaining command fields, and why
  that procedure cannot miss one.
- `docs/STATE.md` — the integration as a state machine: every event, every
  edge, and the seven defects that enumerating them exposed. All are fixed;
  the diagnoses are kept because they are the part worth re-reading.
- `docs/DESIGN.md` — architecture, host selection, milestones.
- `tools/` — `wsprobe.py` (dependency-free WebSocket client for the daemon),
  `decode_proflame.py` (reference decoder) and `analyze_cmd_csv.py`
  (re-derives the checksum from the inherited table).
- `tests/` — `cmd.csv` (220 inherited packets, 5 remotes),
  `frames/*.timings.json` (bench captures) and `frames/*.frames.jsonl`
  (recorded by the daemon).
- `integrations/` — the Home Assistant side: `hackrf_proxy` (the transmitter)
  and `proflame` (the fireplace). See `integrations/README.md`.
- `deploy/` — Containerfile, quadlet unit, podman wrapper, udev rule.

Two command fields, `aux` and split flame, are documented as unconfirmable
rather than unmapped: this appliance does not have them. `docs/MAPPING.md`
says why that is a conclusion and not a gap.

## Safety: both directions are now reachable

The off command was captured on 2026-08-16, which retires the asymmetry this
section used to warn about. `cmd1` bit 0 is on/off, and a verbatim off frame is
on file, so the fireplace can be both started and stopped by replaying frames
the remote itself has sent.

The rule that got us here still binds: replaying a captured frame verbatim is
safe, because it can only reproduce a state the remote just asked for.
Synthesising a command that has never been observed means guessing bits on a
gas appliance, and is not something to settle by experiment. Thermostat mode
deserves particular care whenever it is mapped, since it makes the appliance
cycle on its own and will fight Home Assistant.

## Running it

The radio currently lives on the XPS laptop, because the garage server is out
of range of the living room and its own switching noise raises the 315 MHz floor
fivefold. `hrf` is there at `~/.local/bin/hrf` as a static binary.

As the daemon:

    hrf serve --listen 0.0.0.0:8765 --rx-freq 315M
    tools/wsprobe.py --host xps --listen        # watch what it hears

At the bench:

    ssh xps
    cd /dev/shm/aetf/workspace
    hrf capture --freq 315M --seconds 30 --out power.cs8   # press ON, then OFF
    hrf demod --in power.cs8 --gap-us 3000 --threshold 0.3 --out-all power.json
    hrf decode --in power.json

`--gap-us 3000` is not optional: the inter-frame gap is 4.15 ms, so the 10 ms
default merges repeats into one blob and smears the histogram.

On the homelab server the same tool runs containerised, and the daemon deploys
as a rootless quadlet unit — see `proxyd/README.md`, which also documents the
udev and rootless-podman traps that cost real time here.

## Next

1. **Diagnostics for `hackrf_proxy`.** The fireplace integration can now be
   asked what the radio has and has not managed to send; the transmitter
   cannot be asked anything at all, so a connection problem is visible only by
   reading the log.
2. **Decide on authentication.** The daemon is unauthenticated on the LAN,
   which is the same posture as an ESPHome node except that this one can
   *transmit*. Worth settling before it moves to a less trusted network.
3. **Decide where the radio finally lives.** The garage cannot hear the
   fireplace; candidates are a small host in the living room or an ESP32-C6
   with a CC1101 beside it, which would be a native HA transmitter needing no
   daemon at all.
4. **Settle the echo question**, which needs a second receiver — the HackRF is
   half-duplex and deaf while it transmits, so it cannot hear a reply to its
   own frame. See `docs/PROTOCOL.md`.

Done: M1 (protocol solved, replay ignites), M2 (the daemon), M3 (the
transmitter integration), M4 (`proflame.rs` pinned by regression tests, and the
consumer integration), M5 (following the handset off the air), and the
command-field mapping.

The one thing to know before writing the integrations: **the appliance is
stateless and the handset holds the state**, so Home Assistant and the handset
are two state holders that cannot hear each other. `docs/PROTOCOL.md` explains
what that costs.
