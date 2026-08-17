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

/// The mapping session of docs/MAPPING.md: manual mode, then the blower, the
/// accent light and the flame each swept from off to high and back.
///
/// This is what confirms smartfire's field labels on our own hardware. Each
/// sweep moves exactly one field and leaves the rest alone, which is the whole
/// argument — a layout that merely fit the data would not survive three
/// independent sweeps landing in three different places.
///
/// It also settles two open questions. `fan` reaches 0, so the blower can be
/// commanded off over RF rather than only turned down. And `fan` reading 3 in
/// every earlier capture was the blower genuinely sitting at level 3.
#[test]
fn the_sweeps_confirm_the_blower_light_and_flame_fields() {
    let frames = recorded_frames("frames/manual_sweeps.frames.jsonl");
    assert_eq!(frames.len(), 266);

    let states: Vec<proflame::State> = frames
        .iter()
        .map(|b| proflame::decode(b))
        .filter_map(|d| Some(d.frame()?.state()))
        .collect();

    // 236 of 266. The shortfall is fragmentary bursts — the edge counts run
    // down to 15 where a whole frame is 131 to 143 — so the signal dropped
    // mid-frame rather than being too weak, since all but eleven frames
    // arrived saturated. It costs nothing: five identical frames carry each
    // state, and every step below survived.
    assert_eq!(states.len(), 236, "fragmentary bursts are expected over a 73-minute session");

    let mut timeline: Vec<proflame::State> = Vec::new();
    for state in states {
        if timeline.last() != Some(&state) {
            timeline.push(state);
        }
    }

    // Each sweep moves one field and nothing else. A consecutive pair
    // differing in more than one field would mean either a coupled pair of
    // functions or a wrong layout — and across a 73-minute session there is
    // exactly one, the deliberate switch out of thermostat mode, which also
    // reset the blower and flame the thermostat had been driving.
    //
    // The power-off at the end is *not* among them: it moved only `power`,
    // because the sweeps had already left everything else at zero.
    let multi: Vec<_> = timeline
        .windows(2)
        .map(|pair| pair[1].differences(&pair[0]))
        .filter(|changed| changed.len() > 1)
        .collect();
    assert_eq!(multi, vec![vec!["thermostat", "fan", "flame"]], "{multi:?}");

    let manual: Vec<_> = timeline.iter().filter(|s| !s.thermostat && s.power).collect();
    let seen =
        |f: fn(&proflame::State) -> u8| manual.iter().map(|s| f(s)).collect::<BTreeSet<u8>>();
    let full = BTreeSet::from([0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(seen(|s| s.light), full, "the light swept its whole range");
    assert_eq!(seen(|s| s.fan), full, "the blower swept its whole range, including off");
    assert!(seen(|s| s.flame).is_superset(&BTreeSet::from([0, 1, 2, 3, 4, 5, 6])));

    // Nothing in this session should have touched the fields it did not test.
    assert!(timeline.iter().all(|s| !s.aux && !s.front && !s.pilot && s.reserved == 0));
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
