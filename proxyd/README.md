# hackrf-proxyd

The `hrf` binary: the daemon that makes a HackRF a network-attached radio,
and the bench tools to solve OOK protocols with. `hrf serve` is the daemon;
`capture`, `demod`, `transmit`, `scan` and `info` are the tools.

Driver: `seify-hackrfone` (pure Rust, nusb, no C dependencies). `rs-hackrf` was
rejected: it is receive-only and cannot drive the transmit path.

Part of the [hackrf-proxy](https://github.com/Aetf/hackrf-proxy) repository,
which also carries the lockstep-versioned Python client, a Containerfile, a
quadlet unit and a udev rule under `deploy/`, and a dependency-free WebSocket
probe under `tools/`.

## Layout

A library with a thin CLI on top. The daemon links the library rather than
growing out of the CLI, and the layering is what keeps the project testable
without a radio on the bench:

    src/lib.rs       crate root; the layering contract lives in its doc comment
    src/ook.rs       signal processing: IQ <-> timings, bursts, histograms, streaming
    src/wire.rs      the WebSocket protocol: types and validation
    src/engine.rs    the half-duplex arbiter, over a Transceiver trait
    src/server.rs    the WebSocket front end
    src/radio.rs     the only module that needs a HackRF plugged in
    src/main.rs      the hrf CLI

Everything except `radio.rs` is tested without hardware, including the arbiter,
which runs against a fake device. The daemon carries no appliance protocol:
frame decoding belongs to consumers — the first is the
[proflame](https://github.com/Aetf/proflame) library, whose golden tests pin
the daemon-recorded captures it was solved from.

## The daemon

    hrf serve --listen 0.0.0.0:8765 --rx-freq 315M

It receives continuously, publishing each burst it hears, and lets clients
preempt with transmissions. It is **protocol-agnostic**: it moves raw OOK
timings and knows nothing about any particular appliance, which is what makes
it a shared radio proxy rather than one appliance's bridge.

Poke it with
[`tools/wsprobe.py`](https://github.com/Aetf/hackrf-proxy/blob/main/tools/wsprobe.py)
(no dependencies, for boxes without websocat):

    tools/wsprobe.py --host radio-host --port 8765         # status
    tools/wsprobe.py --host radio-host --listen            # watch rx_frame events

### Recording what it hears

    hrf serve --record frames.jsonl

One JSON object per line, flushed as each frame arrives, so the file is
readable while the radio keeps running; protocol tooling (for example the
[proflame](https://github.com/Aetf/proflame) library) reads the shape
directly.

Use this rather than a listening client for any protocol work: mapping a
remote's unknown fields means pressing buttons and comparing frames, often
across days, and captures should survive a disconnect, a restart and a week.

### Protocol

JSON over WebSocket. Every message carries `v`, from day one, so a client that
predates a change is told so rather than silently misreading it. Requests may
carry an `id`, which is echoed on the reply.

Requests:

| type | fields |
|------|--------|
| `transmit` | `frequency`, `timings[]`, `repeat`, `gap_us`, `txvga_db?`, `amp?` |
| `configure_rx` | `frequency?`, `enabled` |
| `status` | — |

Replies are `transmitted{duration_us}`, `status{...}` or `error{message}`.
Server-pushed events are `rx_frame{frequency, timings, rssi, timestamp_ms}` and
`device_state{state}`, where state is `receiving`, `transmitting`, `idle` or
`faulted`.

Two things a client has to get right:

- **Match replies by `id`.** Events arrive at any time, and a transmission's own
  `device_state` event overtakes its reply. Reading "the next message" is a bug
  that will look like it works until the first transmission.
- **`rssi` is not dBm.** It is the peak L1 magnitude of the burst on a 0–256
  scale, uncalibrated and dependent on the configured gains. Compare bursts to
  each other with it; do not read it as absolute power.

`transmit` replies when the air time is over, not when the request is queued,
so a client knows the transmission actually happened.

### What it refuses

Requests the wire layer can judge are refused without troubling the radio: an
empty timing list, a zero timing, a frequency outside 1 MHz–6 GHz, a TX gain
above 47 dB, and more than 30 seconds of air time in one request. That last one
is the ceiling that matters operationally — the radio is half-duplex, so a long
transmission is a long deafness for every other client.

### Behavior worth knowing

- **A missing or failing radio does not stop the daemon.** It serves in a
  `faulted` state, reports the reason on every request, and keeps retrying with
  a backoff that doubles to a minute. A USB re-enumeration permanently kills the
  old handle, so the device is dropped and reopened rather than retried on a
  dead one.
- **Recovery is only believed once a transfer arrives.** A HackRF whose bulk
  streaming is broken still answers control transfers, so "it identified
  itself" is not evidence that it works. Trusting the identity probe turns one
  environmental fault into an endless declare-recovered/fail-again loop at the
  retry interval.
- **The receiver needs one window (a second by default) to learn the band**
  before it can slice it, so nothing is detected in the first second.
- **A slow client loses events rather than slowing the radio down.** Falling
  behind is logged with the number dropped.

## Build

    cargo build --release      # binary at target/release/hrf
    cargo test                 # no hardware needed
    cargo fmt && cargo clippy  # style is pinned by rustfmt.toml

Static musl builds (`--target x86_64-unknown-linux-musl` or aarch64) produce a
self-contained binary that relocates by copying; releases ship them
prebuilt, along with a container image — see the
[repository README](https://github.com/Aetf/hackrf-proxy#readme).

## Troubleshooting device access

The daemon needs **write access to the HackRF's USB node**; how to grant that
is between you and your distribution, and a udev rule granting your user's
group is the usual shape (the repository's `deploy/` directory carries an
example). Two failure modes are worth naming because they do not look like
permission problems:

- **`hrf info` sees the device but cannot open it.** Distro-packaged rules
  often grant access via `uaccess`, an ACL tied to an active local seat — it
  works on a desktop login and grants nothing over SSH. If the node is
  root-owned and not writable by any of your groups, the rule you are relying
  on is not the one being applied.
- **The radio works, then starts faulting when idle.** USB autosuspend is a
  known problem with the HackRF: a suspended radio fails bulk transfers while
  still answering control transfers. Keep `power/control` at `on` for the
  device, and mind that laptop power managers (for example TLP) apply their
  own USB policy on top of udev's.

Rootless containers add one more: the container's user must map to the host
user the rule grants, and supplementary groups are dropped unless explicitly
kept (`--group-add keep-groups` under podman/crun). The quadlet unit in the
repository's `deploy/` encodes this.

## Bench workflow

Solving an unknown OOK protocol with the tools, end to end:

    # 1. Sanity-check the device.
    hrf info

    # 1b. Find the band instead of guessing: hold a remote button down while
    #     this runs, and the band it uses stands out of the noise.
    hrf scan

    # 2. Capture one button press, cleanly. Peak level is reported once a
    #    second, so a bad gain setting is obvious immediately rather than at
    #    demodulation time.
    hrf capture --freq 315M --seconds 5 --out press.cs8

    # 3. Demodulate offline, as often as you like. --gap-us must be below the
    #    protocol's inter-frame gap, or repeats merge into one blob.
    hrf demod --in press.cs8 --gap-us 3000 --out press.json

    # 4. Decode the framing with the protocol's own tooling, and replay.
    #    Keep --amp off at close range.
    hrf transmit --freq 315M --file press.json --repeat 4

To check what a *live receiver* would have made of a capture — rather than the
more sensitive two-pass offline path — run the same file through the daemon's
own detector:

    hrf demod --in press.cs8 --stream --gap-us 3000

A capture that decodes offline but comes up empty here is a receiver problem
worth knowing about before it shows up as a missed keypress on the air. The
streaming path is slightly less sensitive by nature — it cannot know the
signal level in advance — which remote protocols tolerate: they repeat each
frame several times per press.

### Reading the demod output

    550000 samples (0.28 s), noise floor 7, signal 72, threshold 40

    5 burst(s):
       1:   29 edges, 22.50 ms
       ...
    repeats agree: identical edge counts, max deviation 0 µs (consistent with
    a static frame, no rolling code)

    burst 1 pulse widths (bucket µs -> count):
        -1000 :    7  #######
         -500 :    7  #######
          500 :    8  ########
         1000 :    6  ######
         2000 :    1  #

- **Bursts** are the repeats of one keypress, split on silences of at least
  `--gap-us`. Comparing them is how "no rolling code" gets verified rather than
  assumed: identical repeats mean a static frame.
- **The histogram** is the actual protocol-analysis tool. Two clusters at t and
  2t (here 500 and 1000 µs) is the signature of Manchester coding with a t µs
  half-bit; the lone long mark is a preamble.
- `--out` writes the selected burst (`--burst N`) as a Flipper-RAW JSON array,
  ready for `transmit`.

Tune `--threshold` (fraction between noise floor and signal), `--min-us`
(glitch floor) and `--gap-us` until the clusters are clean.

Site noise matters as much as gain settings: switching supplies and USB put
broadband hash into the sub-GHz bands, and a rack server can raise the noise
floor several-fold over a quiet room in exactly the band a remote uses. If a
capture comes back as noise, `scan` the same bands from somewhere else before
blaming the remote.

## Caveats worth knowing before decoding

- **A burst always ends on a mark.** A frame's final space has no terminating
  edge, so it merges into the silence that follows and cannot be observed. A
  Manchester frame ending in a one therefore looks one half-bit short; the
  decoder must restore it from the symbol clock. This is a property of OOK, not
  a demodulator limitation.
- **The noise floor is the median amplitude**, which assumes carrier is present
  for well under half the capture. If the two reported levels collapse onto each
  other, the capture is mostly carrier and the warning fires.
- **Gains are validated up front**: LNA 0–40 dB in 8 dB steps, VGA 0–62 dB in
  2 dB steps, TX VGA 0–47 dB. The driver would otherwise round LNA down
  silently and report a gain that was never applied.
- **TX uses DC-baseband OOK** (mark = constant amplitude, upconverted to an
  unmodulated carrier). If LO leakage during spaces turns out to matter, the fix
  is a small IF offset — deferred until measured.
- `--rate` must match between capture and demod.
