# hackrf-proxyd (M1 spike)

Standalone HackRF research CLI (`hrf`) for milestone M1. Not the daemon yet —
this proves TX/RX work and re-derives the (untrusted) Proflame protocol from a
fresh capture. Driver: `seify-hackrfone` (pure-Rust nusb; chosen over
`rs-hackrf`, which is RX-only).

## Build

```sh
cargo build --release   # binary at target/release/hrf
```

## One-time device access (no sudo for the tool itself, but rule install needs root)

```sh
sudo cp ../deploy/53-hackrf.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
# ensure your user is in the group named in the rule (plugdev), then re-plug
```

Confirm: `hrf info` should print a board id and firmware version.

## M1 workflow

```sh
# 1. Sanity
hrf info

# 2. Capture the physical remote. Start this, then press ONE button on the
#    Proflame remote once, cleanly. 315 MHz, 2 Msps, ~5 s window.
hrf capture --freq 315000000 --rate 2000000 --seconds 5 --out captures/flame_up.cs8

# 3. Demodulate offline. The histogram is the deliverable: it reveals the
#    short/long pulse widths (=> bit clock), preamble, and frame gaps. Iterate
#    --threshold / --min-us until the buckets are clean.
hrf demod --in captures/flame_up.cs8 --rate 2000000 --out captures/flame_up.json

# 4. Replay it back to the fireplace (keep --amp OFF at close range first).
hrf transmit --freq 315000000 --file captures/flame_up.json --repeat 4
```

Go/no-go for the whole project: does step 4 make the fireplace react?

## Notes / caveats
- OOK synthesis uses DC-baseband (mark = constant amplitude via LO carrier). If
  LO leakage during "space" is a problem, add a small IF offset — deferred.
- `--rate` must match between capture and demod.
- The demod self-test (synthetic 500 µs square wave -> clean ±500 buckets) is
  the sanity check that the pipeline is wired correctly.
