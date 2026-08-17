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

Or build the container, which is how it is meant to be run — see below.

## Container

Preferred way to run: the image is a static musl binary on Alpine, about 11 MB,
with no libusb or SoapySDR on either side of the boundary. This is also the
shape the M2 daemon will be deployed in.

    podman build -f deploy/Containerfile -t hackrf-proxyd .
    deploy/hrf-podman.sh info
    deploy/hrf-podman.sh capture --seconds 5 --out flame_up.cs8
    deploy/hrf-podman.sh demod --in flame_up.cs8 --out flame_up.json

Captures are written to `./captures` on the host (override with
`HRF_CAPTURES`). Two things about the USB passthrough are worth knowing:

- **The container must run as root.** Under rootless podman its root maps to the
  invoking user on the host, which is the identity the udev rule grants access
  to. Any other user inside the container maps into the subuid range
  (`aetf:100000:65536` here) and cannot open the node at all.
- **`/dev/bus/usb` is bind-mounted as a directory** rather than passed as a
  single `--device`. The HackRF changes its device number whenever it
  re-enumerates — on re-plug, and after a device reset — and a stale `--device`
  path fails in a way that looks like missing hardware.

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

On the aconfmgr-managed homelab, this belongs in the config rather than being
dropped into `/etc` by hand — it is `roles/sdr.bash` there, enabled from the
host config, and applied with `aconfmgr apply`.

`hrf info` should then print a board id and firmware version. If it instead
reports that the HackRF is present but could not be opened, and names a node
under `/dev/bus/usb`, the rule has not taken effect yet.

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
