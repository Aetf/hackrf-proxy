# hackrf-proxyd

`hrf`: the daemon that makes a HackRF a network-attached radio, and the bench
tools to solve OOK protocols with. `hrf serve` is the daemon; `capture`,
`demod`, `transmit`, `scan` and `info` are the tools.

Driver: `seify-hackrfone` (pure Rust, nusb, no C dependencies). `rs-hackrf` was
rejected: it is receive-only and cannot drive the transmit path.

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
timings and knows nothing about Proflame or fireplaces, which is what makes it a
shared radio proxy rather than one appliance's bridge.

Poke it with `tools/wsprobe.py` (no dependencies, for boxes without websocat):

    tools/wsprobe.py --host radio-host --port 8765         # status
    tools/wsprobe.py --host radio-host --listen            # watch rx_frame events

### Recording what it hears

    hrf serve --record frames.jsonl

One JSON object per line, flushed as each frame arrives, so the file is
readable while the radio keeps running; protocol tooling (for example the
[proflame](https://github.com/Aetf/proflame) library) reads the shape
directly.

Use this rather than a listening client for any protocol work. Mapping a
remote's unknown fields means pressing buttons and comparing frames, often
across days, and making that depend on a client staying connected is how
captures get lost — which is exactly how a set of them was lost here.

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

### Verified on the real radio (2026-08-17, bench laptop)

Everything the daemon does has now run against a HackRF One, over the network:
the receiver streams continuously for hours and learns its threshold from live
315 MHz noise (54–55 here, and 16 on 433.92 MHz — independently rediscovering
that this site's 315 MHz floor is the noisier of the two); retuning between the
two bands works; a transmission preempts and hands the radio back with the
receiver demonstrably streaming again afterwards; SIGTERM shuts down cleanly.
Continuous receive costs about 4% of one core and 6 MB.

Two results worth keeping. Over ninety minutes of live 315 MHz ambient noise the
detector produced **zero spurious frames and zero burst overflows** — that was
the exact failure mode of the first threshold design, and it does not reproduce
on real signal. And the fault machinery earned its keep: 435 real USB faults,
435 recoveries, no manual intervention.

**Not yet verified: an actual received frame.** That needs somebody to press the
fireplace remote while the daemon is listening.

### USB autosuspend will bite you

The HackRF must not be autosuspended. It is a known problem with this device,
and for a daemon that streams continuously it is worse than the usual symptom:
a suspended radio fails its next bulk transfer, while still answering control
transfers, so a naive recovery check restarts straight back into the failure.

`deploy/60-hackrf-access.rules` disables the autosuspend timer. **On a laptop
running TLP that is not enough** — TLP applies its own USB policy afterwards and
will turn it back on:

    # /etc/tlp.d/10-hackrf.conf
    USB_DENYLIST="1d50:6089"

then `systemctl restart tlp`. Check with:

    cat /sys/bus/usb/devices/*/power/control     # want "on" for the HackRF

### What it refuses

Requests the wire layer can judge are refused without troubling the radio: an
empty timing list, a zero timing, a frequency outside 1 MHz–6 GHz, a TX gain
above 47 dB, and more than 30 seconds of air time in one request. That last one
is the ceiling that matters operationally — the radio is half-duplex, so a long
transmission is a long deafness for every other client.

### Behaviour worth knowing

- **A missing or failing radio does not stop the daemon.** It serves in a
  `faulted` state, reports the reason on every request, and keeps retrying with
  a backoff that doubles to a minute. A USB re-enumeration permanently kills the
  old handle, so the device is dropped and reopened rather than retried on a
  dead one.
- **Recovery is only believed once a transfer arrives.** A HackRF whose bulk
  streaming is broken still answers control transfers, so "it identified
  itself" is not evidence that it works. Getting this wrong produced 435 faults
  in four hours on the bench laptop, 408 of them spaced at exactly the retry interval.
- **The receiver needs one window (a second by default) to learn the band**
  before it can slice it, so nothing is detected in the first second.
- **A slow client loses events rather than slowing the radio down.** Falling
  behind is logged with the number dropped.

## Build

    cargo build --release      # binary at target/release/hrf
    cargo test                 # unit + regression tests, no hardware needed
    cargo fmt && cargo clippy  # style is pinned by rustfmt.toml

## Build

    cargo build --release      # binary at target/release/hrf
    cargo test                 # unit + regression tests, no hardware needed
    cargo fmt && cargo clippy  # style is pinned by rustfmt.toml

Or build the container, which is how it is meant to be run — see below.

## Container

Preferred way to run: the image is a static musl binary on Alpine, about 12 MB,
with no libusb or SoapySDR on either side of the boundary.

    podman build -f deploy/Containerfile -t hackrf-proxyd .
    deploy/hrf-podman.sh info
    deploy/hrf-podman.sh capture --seconds 5 --out flame_up.cs8
    deploy/hrf-podman.sh demod --in flame_up.cs8 --out flame_up.json

The daemon is deployed as a rootless quadlet unit:

    cp deploy/hackrf-proxyd.container ~/.config/containers/systemd/
    systemctl --user daemon-reload
    systemctl --user start hackrf-proxyd

`loginctl enable-linger $USER` is what makes it survive logout and start at
boot. The unit carries the same USB caveats as the wrapper script, and the
reasons are in its comments.

Captures are written to `./captures` on the host (override with
`HRF_CAPTURES`). Two things about the USB passthrough are worth knowing:

- **The container must run as root.** Under rootless podman its root maps to the
  invoking user on the host, which is the identity the udev rule grants access
  to. Any other user inside the container maps into the subuid range
  (`aetf:100000:65536` here) and cannot open the node at all.
- **`--group-add keep-groups` is required**, and its absence is the confusing
  case: the host user can open the device while the container cannot, even
  though the container is that same user. A rootless container maps only the
  uid and the primary gid, so membership of the group the udev rule grants
  access through — `wheel` here — is dropped. `keep-groups` asks crun to keep
  the host's supplementary groups for the kernel's permission check. Inside the
  container they then show up as `65534`, since they have no mapping in the
  namespace; that is expected and access still works.
- **`/dev/bus/usb` is bind-mounted as a directory** rather than passed as a
  single `--device`. The HackRF changes its device number whenever it
  re-enumerates — on re-plug, and after a device reset — and a stale `--device`
  path fails in a way that looks like missing hardware.

## Running it on another machine

The release binary is a static-pie musl executable with no shared libraries, so
moving the radio to a better listening post costs one copy and no toolchain:

    podman create --name hrfx hackrf-proxyd
    podman cp hrfx:/usr/local/bin/hrf ~/.local/bin/hrf   # or scp to the target
    podman rm hrfx

On a laptop with an active local session the distro's `uaccess` rule is enough
on its own, which is why the same package that grants nothing on a headless
server works there without further setup. Check with `loginctl list-sessions`:
a row with a real seat (`seat0`) rather than `-` means the ACL will be granted.

## One-time device access

Required for both the container and the native binary; the rule install needs
root, the tool itself does not.

**Installing the distro `hackrf` package is not enough.** Its
`53-hackrf.rules` grants access with `TAG+="uaccess"` alone, which is an ACL
logind hands to a user with an active *local seat*. Over SSH there is no seat,
so nothing is granted: `getfacl` on the node shows no ACL entries and the node
stays `root:root 0664` — readable, not writable, so the radio cannot be opened.

    sudo cp deploy/60-hackrf-access.rules /etc/udev/rules.d/
    sudo udevadm control --reload-rules && sudo udevadm trigger

The rule supplements the packaged one rather than replacing it, so new device
ids keep coming from the package. It uses the `wheel` group because this host
has no `plugdev`; change it to any group you belong to.

`hrf info` should then print a board id and firmware version. If it instead
reports that the HackRF is present but could not be opened, and names a node
under `/dev/bus/usb`, the rule has not taken effect yet.

## Bench workflow

How the protocol was solved, and how to map a field that is still unknown.

    # 1. Sanity check the device.
    hrf info

    # 1b. If a capture comes back as pure noise, scan instead of guessing.
    #     Hold a remote button down while this runs; the band it uses stands out.
    hrf scan --amp

    # 2. Capture the remote. Start this, then press one button, once, cleanly.
    #    Peak level is reported once a second, so a bad gain setting is obvious
    #    immediately rather than at demodulation time.
    hrf capture --freq 315000000 --seconds 5 --out captures/flame_up.cs8

    # 3. Demodulate offline, as often as you like. For Proflame use
    #    --gap-us 3000: the inter-frame gap is 4.15 ms, so the 10 ms default
    #    merges repeats into one blob.
    hrf demod --in captures/flame_up.cs8 --gap-us 3000 --out captures/flame_up.json

    # 3b. Decode the framing with the protocol's own tooling — for Proflame,
    #     the decoder in the proflame repository reads this file directly.

    # 4. Replay. Keep --amp off at close range.
    hrf transmit --freq 315000000 --file captures/flame_up.json --repeat 4

Step 4 was the project's go/no-go, and the fireplace did react.

To check what a *live receiver* would have made of a capture — rather than the
more sensitive two-pass offline path — run the same file through the daemon's
own detector:

    hrf demod --in captures/flame_up.cs8 --stream --gap-us 3000

A capture that decodes offline but comes up empty here is a receiver problem
worth knowing about before it shows up as a missed keypress on the air. The
streaming path is slightly less sensitive by nature: it cannot know the signal
level in advance, and on the power capture it recovers all 25 bursts but decodes
24 of them cleanly against the offline path's 25. Since a remote sends five
identical repeats per press, losing one frame does not lose the press.

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

For reference, three seconds of ambient 315 MHz with nothing transmitting, at
the default `--lna 40 --vga 40`, measured a noise floor of 24 and a 99.9th
percentile of 110, and `demod` correctly found no bursts. A remote keypress
should stand well clear of that.

### Site noise matters, and 315 MHz is the bad case here

An idle `scan` from a rack server whose own switching noise lands in-band,
same gains throughout:

| band | peak | 99.9% |
|------|------|-------|
| 315.000 MHz | 164–194 | 110 |
| 318.000 MHz | 138–237 | 93  |
| 390.000 MHz | ~102    | 40  |
| 433.920 MHz | ~31     | 20  |

The noise floor at 315 MHz is over five times that at 433.92 MHz. That is the
local environment, not a gain artefact — switching supplies and USB in a server
put broadband hash right where this remote lives. A weak signal has to clear
that, so where the radio sits matters as much as whether it can hear at all.

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
