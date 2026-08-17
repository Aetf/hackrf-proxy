//! Regression tests against the shared protocol data in `../tests/`.
//!
//! Two independent sources pin the Rust port down: `cmd.csv` (220 packets
//! from five remotes, inherited from the earlier prototype) checks the
//! checksum model, and `frames/*.timings.json` (our own demodulated captures
//! of remote 0086) check the full air-interface decode. The expected counts
//! and values are frozen — this data never changes, so any drift is a bug in
//! the code, not the data.

use std::collections::{BTreeMap, BTreeSet};

use hackrf_proxyd::proflame::{self, Frame, Keys};

fn data(name: &str) -> String {
    let path = format!("{}/../tests/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The key derived from any one packet of a remote must reproduce every
/// checksum byte that remote ever sent: 440 of 440 across five remotes.
#[test]
fn cmd_csv_checksums_all_reproduce() {
    let csv = data("cmd.csv");
    let mut rows = 0usize;
    let mut keys_by_remote: BTreeMap<(u8, u8), Keys> = BTreeMap::new();

    for line in csv.lines().skip(1) {
        let fields: Vec<u8> = line
            .split(',')
            .map(|f| u8::from_str_radix(f.trim(), 16).unwrap_or_else(|e| panic!("{line}: {e}")))
            .collect();
        let [serial1, serial2, _version, cmd1, cmd2, cs1, cs2] = fields[..] else {
            panic!("bad row: {line}");
        };

        let keys = keys_by_remote.entry((serial1, serial2)).or_insert_with(|| Keys {
            k1: proflame::derive_key(cmd1, cs1),
            k2: proflame::derive_key(cmd2, cs2),
        });
        assert_eq!(proflame::checksum(cmd1, keys.k1), cs1, "half 1 of {line}");
        assert_eq!(proflame::checksum(cmd2, keys.k2), cs2, "half 2 of {line}");
        rows += 1;
    }

    assert_eq!(rows, 220);
    assert_eq!(keys_by_remote.len(), 5, "the table spans five remotes");
}

/// Bursts from a `serve --record` file, one JSON object per line.
fn recorded_frames(name: &str) -> Vec<Vec<i64>> {
    data(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["timings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_i64().unwrap())
                .collect()
        })
        .collect()
}

fn decode_capture(name: &str) -> Vec<(Frame, Keys)> {
    let bursts: Vec<Vec<i64>> = serde_json::from_str(&data(name)).unwrap();
    let clean: Vec<_> = bursts
        .iter()
        .map(|b| proflame::decode(b))
        .filter_map(|d| Some((d.frame()?, d.keys()?)))
        .collect();
    assert!(!clean.is_empty(), "{name}: no clean frames");

    for (frame, keys) in &clean {
        assert_eq!(
            (frame.serial1, frame.serial2, frame.version),
            (0x00, 0x86, 0x02),
            "{name}: wrong identity fields"
        );
        assert_eq!(*keys, Keys { k1: 0x0a, k2: 0x86 }, "{name}: keys must be constant");
    }
    clean
}

#[test]
fn flame_up_capture_steps_the_level_up() {
    let clean = decode_capture("frames/flame_up.timings.json");
    let levels: BTreeSet<u8> = clean.iter().map(|(f, _)| f.cmd2).collect();
    assert_eq!(levels, BTreeSet::from([0x32, 0x33, 0x34]));
    assert!(clean.iter().all(|(f, _)| f.cmd1 == 0x01), "the flame buttons never left it on");
}

#[test]
fn flame_down_capture_steps_the_level_down() {
    let clean = decode_capture("frames/flame_down.timings.json");
    let levels: BTreeSet<u8> = clean.iter().map(|(f, _)| f.cmd2).collect();
    assert_eq!(levels, BTreeSet::from([0x33, 0x34, 0x35]));
    assert!(clean.iter().all(|(f, _)| f.cmd1 == 0x01), "the flame buttons never left it on");
}

/// The capture that resolved the safety asymmetry: pressing on and off with
/// nothing else touched moved exactly one bit, `cmd1` bit 0.
///
/// The flame level rides along unchanged at `cmd2 = 0x36` through both
/// states, so off is not "level zero" — the appliance remembers the level it
/// was at. Anything that wants to turn the fireplace off at a *different*
/// level would be synthesising an unobserved frame, which the project rules
/// out; see docs/PROTOCOL.md.
#[test]
fn power_capture_isolates_the_on_off_bit() {
    let clean = decode_capture("frames/power_on_off.timings.json");

    assert_eq!(clean.len(), 25, "every frame of the press sequence decoded");
    assert!(
        clean.iter().all(|(f, _)| f.cmd2 == 0x36),
        "the flame level survives being switched off"
    );

    let states: BTreeSet<u8> = clean.iter().map(|(f, _)| f.cmd1).collect();
    assert_eq!(states, BTreeSet::from([0x00, 0x01]), "on/off is the only field that moved");
}

/// The first capture taken through the daemon rather than the bench tools,
/// and the first with the appliance in thermostat ("smart") mode.
///
/// Two things it pins down. The checksum model predicts `cmd1 = 0x03`, a
/// command byte no earlier capture contained, which is independent evidence
/// that it generalises rather than fitting what we had. And in thermostat mode
/// the remote drives the flame level itself — here stepping `0x31` to `0x30`
/// three seconds apart, unprompted — which is the behaviour that will fight
/// Home Assistant if M5 treats received frames as user intent.
#[test]
fn thermostat_mode_frames_decode_and_carry_a_new_command_byte() {
    let bursts = recorded_frames("frames/smart_mode.frames.jsonl");
    assert_eq!(bursts.len(), 10);

    let clean: Vec<_> = bursts
        .iter()
        .map(|b| proflame::decode(b))
        .filter_map(|d| Some((d.frame()?, d.keys()?)))
        .collect();

    assert_eq!(clean.len(), 10, "every frame the daemon recorded should decode");
    for (frame, keys) in &clean {
        assert_eq!(*keys, Keys { k1: 0x0a, k2: 0x86 }, "same remote as the bench captures");
        assert_eq!(frame.cmd1, 0x03, "on, with the bit that is only set in smart mode");
    }

    let levels: BTreeSet<u8> = clean.iter().map(|(f, _)| f.cmd2).collect();
    assert_eq!(levels, BTreeSet::from([0x30, 0x31]), "the remote stepped the flame down itself");
}

/// Steps 2 and 4 of docs/MAPPING.md, in one session: the handset was switched
/// to manual mode and then the blower stepped up one level at a time.
///
/// This is what confirms two of smartfire's field labels on our own hardware.
/// The blower sweep moves `fan` and nothing else through 1…6 — which also
/// settles why every earlier capture read `fan = 3`: the blower really was
/// sitting at level 3. And the manual-mode frames carry `thermostat = 0`
/// where the smart-mode ones carried 1, with the flame no longer drifting on
/// its own, which is the isolation the earlier capture could not provide.
#[test]
fn the_fan_sweep_confirms_the_blower_and_thermostat_fields() {
    let frames = recorded_frames("frames/manual_fan_sweep.frames.jsonl");
    let states: Vec<proflame::State> = frames
        .iter()
        .map(|b| proflame::decode(b))
        .filter_map(|d| Some(d.frame()?.state()))
        .collect();

    // 73 of 80. The rest arrived late in the session at a weaker level — the
    // recorded peak falls to 159 where the rest saturate — and at that level
    // the inter-frame gap stops being distinguishable, so pairs of frames
    // merge into one eight-block burst that cannot decode.
    //
    // Asserted rather than waved away, because it is the honest reception
    // rate at this range. It costs nothing here: the remote sends five
    // identical frames per state, so a lost frame is not a lost state, and
    // every step of the sweep below is present.
    assert_eq!(states.len(), 73, "corrupted frames are expected at the weak end");
    assert_eq!(frames.len(), 80);

    // Distinct states, in the order first heard.
    let mut sequence: Vec<proflame::State> = Vec::new();
    for state in states {
        if sequence.last() != Some(&state) {
            sequence.push(state);
        }
    }

    let manual: Vec<_> = sequence.iter().filter(|s| !s.thermostat).collect();
    assert_eq!(
        manual.iter().map(|s| s.fan).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 5, 4, 3, 2, 1],
        "the blower stepped one level at a time, up and back down"
    );
    for pair in manual.windows(2) {
        assert_eq!(pair[1].differences(pair[0]), vec!["fan"], "and moved nothing else");
    }
    assert!(manual.iter().all(|s| s.flame == 4), "the flame held still in manual mode");
    assert!(
        sequence.iter().any(|s| s.thermostat),
        "the session also contains the smart-mode frames it started from"
    );
}

/// Re-encoding a captured frame and decoding it again must reproduce the
/// frame — the byte-level guarantee that lets the encoder transmit exactly
/// what the remote sent.
#[test]
fn captured_frames_survive_reencoding() {
    for name in [
        "frames/flame_up.timings.json",
        "frames/flame_down.timings.json",
        "frames/power_on_off.timings.json",
    ] {
        for (frame, keys) in decode_capture(name) {
            let round_trip = proflame::decode(&frame.to_timings(keys));
            assert_eq!(round_trip.frame(), Some(frame));
            assert_eq!(round_trip.keys(), Some(keys));
        }
    }
}
