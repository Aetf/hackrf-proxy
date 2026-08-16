# hackrf-proxyd (M1 spike)

Standalone HackRF research CLI (`hrf`) for milestone M1. Not the daemon yet —
it exists to prove TX/RX work and to re-derive the Proflame framing from a
fresh capture, since the earlier protocol notes are untrusted.

Driver: `seify-hackrfone` (pure Rust, nusb, no C dependencies). `rs-hackrf` was
rejected: it is receive-only and cannot drive the transmit path.

## Layout

    src/ook.rs     signal processing: IQ <-> timings, bursts, histograms (unit tested)
    src/radio.rs   device handling: gain validation, streaming capture and transmit
    src/main.rs    CLI

`ook.rs` is deliberately hardware-free, so the analysis that M1's conclusions
rest on can be tested against synthetic signals: `cargo test`.

## Build

    cargo build --release      # binary at target/release/hrf
    cargo test                 # 12 tests, no hardware needed

## One-time device access

The rule install needs root; the tool itself does not.

    sudo cp ../deploy/53-hackrf.rules /etc/udev/rules.d/
    sudo udevadm control --reload-rules && sudo udevadm trigger

Make sure your user is in the group named in the rule (`plugdev`), then re-plug
the HackRF. `hrf info` should then print a board id and firmware version.

## M1 workflow

    # 1. Sanity check the device.
    hrf info

    # 2. Capture the remote. Start this, then press one button, once, cleanly.
    #    Peak level is reported once a second, so a bad gain setting is obvious
    #    immediately rather than at demodulation time.
    hrf capture --freq 315000000 --seconds 5 --out captures/flame_up.cs8

    # 3. Demodulate offline, as often as you like.
    hrf demod --in captures/flame_up.cs8 --out captures/flame_up.json

    # 4. Replay. Keep --amp off at close range.
    hrf transmit --freq 315000000 --file captures/flame_up.json --repeat 4

Step 4 is the project's go/no-go: does the fireplace react?

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
