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
        assert_eq!(frame.cmd1, 0x01, "{name}: neither captured button changed cmd1");
    }
    clean
}

#[test]
fn flame_up_capture_steps_the_level_up() {
    let clean = decode_capture("frames/flame_up.timings.json");
    let levels: BTreeSet<u8> = clean.iter().map(|(f, _)| f.cmd2).collect();
    assert_eq!(levels, BTreeSet::from([0x32, 0x33, 0x34]));
}

#[test]
fn flame_down_capture_steps_the_level_down() {
    let clean = decode_capture("frames/flame_down.timings.json");
    let levels: BTreeSet<u8> = clean.iter().map(|(f, _)| f.cmd2).collect();
    assert_eq!(levels, BTreeSet::from([0x33, 0x34, 0x35]));
}

/// Re-encoding a captured frame and decoding it again must reproduce the
/// frame — the byte-level guarantee that lets the encoder transmit exactly
/// what the remote sent.
#[test]
fn captured_frames_survive_reencoding() {
    for name in ["frames/flame_up.timings.json", "frames/flame_down.timings.json"] {
        for (frame, keys) in decode_capture(name) {
            let round_trip = proflame::decode(&frame.to_timings(keys));
            assert_eq!(round_trip.frame(), Some(frame));
            assert_eq!(round_trip.keys(), Some(keys));
        }
    }
}
