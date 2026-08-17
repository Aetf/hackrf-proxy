# hackrf-proxy

Make a HackRF One a shared, network-attached RF transceiver for Home Assistant,
the way an ESPHome node is a Bluetooth proxy, and on top of it integrate a SIT
Proflame gas fireplace as first-class HA entities with two-way state sync.

## Where things stand (2026-08-16)

**M1 is done.** The protocol is solved and both directions are proven on real
hardware: a frame captured from the remote, replayed by the HackRF, ignited the
fireplace from cold.

What exists:

- `proxyd/` — a library crate with `hrf`, the research CLI, on top: `info`,
  `scan`, `capture`, `demod`, `decode`, `transmit`. Signal processing
  (`ook.rs`) and the Proflame protocol (`proflame.rs`, both directions) are
  hardware-free and tested, with `tests/` frozen in as regression data;
  device handling is in `radio.rs`. This is not the daemon yet, but it is the
  library the daemon will link.
- `docs/PROTOCOL.md` — the Proflame protocol, solved and verified.
- `docs/DESIGN.md` — architecture, host selection, milestones.
- `tools/` — `decode_proflame.py` (reference decoder) and
  `analyze_cmd_csv.py` (re-derives the checksum from the inherited table).
- `tests/` — `cmd.csv` (220 inherited packets, 5 remotes) and
  `frames/*.timings.json` (our own demodulated captures).
- `deploy/` — Containerfile, podman wrapper, udev rule.

What does not exist yet: the daemon, its WebSocket protocol, and both Home
Assistant integrations. See the milestones in `docs/DESIGN.md`.

## Safety: we can ignite but not extinguish

Every frame captured so far encodes the fireplace *on*, so the bit meaning
"off" is unknown and RF can currently only start the appliance. Stopping it
depends on the physical remote. **Capturing an off press is the next task**,
ahead of any other work.

The rule followed so far, and worth keeping: replaying a captured frame
verbatim is safe, because it can only reproduce a state the remote itself just
asked for. Synthesising a command that has never been observed means guessing
bits on a gas appliance, and is not something to settle by experiment.
Thermostat mode deserves particular care, since it makes the appliance cycle on
its own and will fight Home Assistant.

## Running it

The radio currently lives on the XPS laptop, because the garage server is out
of range of the living room and its own switching noise raises the 315 MHz floor
fivefold. `hrf` is there at `~/.local/bin/hrf` as a static binary.

    ssh xps
    cd /dev/shm/aetf/workspace
    hrf capture --freq 315M --seconds 30 --out power.cs8   # press ON, then OFF
    hrf demod --in power.cs8 --gap-us 3000 --threshold 0.3 --out-all power.json
    hrf decode --in power.json

`--gap-us 3000` is not optional: the inter-frame gap is 4.15 ms, so the 10 ms
default merges repeats into one blob and smears the histogram.

On the homelab server the same tool runs containerised — see
`proxyd/README.md`, which also documents the udev and rootless-podman traps
that cost real time here.

## Next

1. Capture the off command, for the safety reason above.
2. Map the remaining fields with controlled captures: one button per capture,
   compare against a known state. Fan, accent light, aux, thermostat.
3. ~~M4: port the protocol to Rust with `tests/` as regression data.~~ Done
   (2026-08-16): `proxyd/src/proflame.rs`, both directions, checked against
   the reference decoder and pinned by regression tests; `hrf decode` replaces
   the Python tool in the field workflow.
4. M2: grow the daemon out of the `proxyd` library — WebSocket API,
   half-duplex arbitration, quadlet deployment.
5. Decide where the radio finally lives. The garage cannot hear the fireplace;
   candidates are a small host in the living room or an ESP32-C6 with a CC1101
   beside the fireplace, which would be a native HA transmitter needing no
   daemon at all.
