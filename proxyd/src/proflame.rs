//! The SIT Proflame 2 protocol: Flipper-RAW timings <-> frames.
//!
//! A Rust port of `tools/decode_proflame.py`, which stays as the independent
//! reference; `tests/` holds the shared regression data and docs/PROTOCOL.md
//! the derivation. This module is pure protocol: it never touches the radio,
//! and both directions are total functions over their inputs.
//!
//! Safety: the encoder makes it trivial to synthesise a frame no remote has
//! ever sent, which on a gas appliance is not something to settle by
//! experiment. Only transmit field values that have been observed from the
//! physical remote — see the safety note in docs/PROTOCOL.md.

use std::fmt;

/// Manchester half-bit duration. Measured pulse widths sit ~100 µs off this
/// (marks short, spaces long, a slicing-threshold artefact), which is why
/// decoding quantises to the nearest whole symbol.
pub const SYMBOL_US: i64 = 450;

/// Sync (3 mark + 1 space) plus 11 Manchester bits of 2 symbols each.
pub const BLOCK_SYMBOLS: usize = 26;

const SYNC_SYMBOLS: usize = 4;
/// Three consecutive equal symbols cannot occur in Manchester data; the sync
/// is a deliberate code violation.
const SYNC: [bool; SYNC_SYMBOLS] = [true, true, true, false];

/// serial1, serial2, version, cmd1, cmd2, checksum1, checksum2.
pub const FRAME_BLOCKS: usize = 7;

/// Block names, in air order.
pub const FIELD_NAMES: [&str; FRAME_BLOCKS] =
    ["serial1", "serial2", "version", "cmd1", "cmd2", "checksum1", "checksum2"];

/// Gap between repeats of a held button, as measured from the remote.
pub const INTER_FRAME_GAP_US: i64 = 4_150;

/// Repetition count proven on air: the M1 ignition sent ten frames, and the
/// remote itself sends five per state step.
pub const PROVEN_REPEATS: u32 = 10;

fn nibble(n: u8) -> u8 {
    n ^ (n << 5)
}

/// The linear map shared by both checksum halves: `cs = mix(cmd) ^ K`.
///
/// Invertible but not a CRC — polynomial search over the inherited table
/// found nothing, and nothing here depends on it being one.
fn mix(byte: u8) -> u8 {
    (byte & 0xF0) ^ nibble(byte & 0x0F) ^ nibble(byte >> 4)
}

/// Checksum for one half, given that half's per-remote constant.
pub fn checksum(cmd: u8, key: u8) -> u8 {
    mix(cmd) ^ key
}

/// Recover a half's per-remote constant from any observed (cmd, checksum)
/// pair. One valid frame fixes both constants, so the serial-to-key mapping
/// never has to be solved to transmit.
pub fn derive_key(cmd: u8, checksum: u8) -> u8 {
    mix(cmd) ^ checksum
}

/// Per-remote checksum constants, one per frame half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    pub k1: u8,
    pub k2: u8,
}

/// One frame's payload. Checksums are derived from [`Keys`], not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub serial1: u8,
    pub serial2: u8,
    pub version: u8,
    pub cmd1: u8,
    pub cmd2: u8,
}

impl Frame {
    /// The seven block values as they go on air.
    pub fn blocks(&self, keys: Keys) -> [u8; FRAME_BLOCKS] {
        [
            self.serial1,
            self.serial2,
            self.version,
            self.cmd1,
            self.cmd2,
            checksum(self.cmd1, keys.k1),
            checksum(self.cmd2, keys.k2),
        ]
    }

    /// Encode as a Flipper-RAW timing array (positive = mark, negative =
    /// space), symbols exactly [`SYMBOL_US`] long.
    ///
    /// The result ends on a mark. The frame's true final space is
    /// indistinguishable on air from the silence that follows it, so it
    /// belongs to the inter-frame gap the caller inserts; this also makes
    /// encode and demodulated captures byte-comparable.
    pub fn to_timings(&self, keys: Keys) -> Vec<i64> {
        let mut symbols = Vec::with_capacity(FRAME_BLOCKS * BLOCK_SYMBOLS);
        for (index, value) in self.blocks(keys).into_iter().enumerate() {
            symbols.extend_from_slice(&SYNC);
            let mut bits = [false; BITS_PER_BLOCK];
            for (bit, slot) in bits.iter_mut().take(8).enumerate() {
                *slot = value & (0x80 >> bit) != 0;
            }
            bits[8] = index == 0; // start-of-frame flag
            bits[9] = bits[..9].iter().filter(|&&b| b).count() % 2 == 1; // even parity
            bits[10] = true; // stop bit
            for bit in bits {
                symbols.extend_from_slice(if bit { &[true, false] } else { &[false, true] });
            }
        }

        let mut timings: Vec<i64> = Vec::new();
        for symbol in symbols {
            let signed = if symbol { SYMBOL_US } else { -SYMBOL_US };
            match timings.last_mut() {
                Some(last) if (*last > 0) == symbol => *last += signed,
                _ => timings.push(signed),
            }
        }
        while timings.last().is_some_and(|&t| t < 0) {
            timings.pop();
        }
        timings
    }
}

/// data[8] + start-of-frame + parity + stop.
const BITS_PER_BLOCK: usize = 11;

/// A framing rule violated while decoding one burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// The block does not open with the 3-mark, 1-space sync pattern.
    Sync {
        block: usize,
    },
    /// A symbol pair was neither `10` nor `01`; the block value is lost.
    Manchester {
        block: usize,
    },
    StopBit {
        block: usize,
    },
    Parity {
        block: usize,
    },
    /// The start-of-frame flag must be set in block 0 and clear elsewhere.
    StartOfFrame {
        block: usize,
        value: bool,
    },
    /// The burst did not quantise to exactly [`FRAME_BLOCKS`] blocks.
    BlockCount {
        blocks: usize,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Sync { block } => write!(f, "block {block}: bad sync"),
            Problem::Manchester { block } => write!(f, "block {block}: Manchester violation"),
            Problem::StopBit { block } => write!(f, "block {block}: stop bit not set"),
            Problem::Parity { block } => write!(f, "block {block}: parity"),
            Problem::StartOfFrame { block, value } => {
                write!(f, "block {block}: start-of-frame flag is {}", u8::from(*value))
            }
            Problem::BlockCount { blocks } => {
                write!(f, "{blocks} blocks, expected {FRAME_BLOCKS}")
            }
        }
    }
}

/// One decoded burst: every block value that survived Manchester decoding,
/// and every framing rule that failed.
///
/// Partial results matter here — a burst with one damaged block still says a
/// lot during protocol analysis — so this is a report, not a `Result`.
#[derive(Debug, Clone, Default)]
pub struct Decoded {
    /// Per-block value, in air order; `None` where Manchester decoding failed.
    pub blocks: Vec<Option<u8>>,
    pub problems: Vec<Problem>,
}

impl Decoded {
    /// Every framing rule held and all seven blocks decoded.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty() && self.blocks.len() == FRAME_BLOCKS
    }

    /// The frame, if the burst decoded cleanly.
    pub fn frame(&self) -> Option<Frame> {
        if !self.is_clean() {
            return None;
        }
        Some(Frame {
            serial1: self.blocks[0]?,
            serial2: self.blocks[1]?,
            version: self.blocks[2]?,
            cmd1: self.blocks[3]?,
            cmd2: self.blocks[4]?,
        })
    }

    /// The per-remote checksum constants, if the burst decoded cleanly.
    pub fn keys(&self) -> Option<Keys> {
        if !self.is_clean() {
            return None;
        }
        Some(Keys {
            k1: derive_key(self.blocks[3]?, self.blocks[5]?),
            k2: derive_key(self.blocks[4]?, self.blocks[6]?),
        })
    }
}

/// Decode one demodulated burst (as produced by `hrf demod`).
///
/// Timings are quantised to the nearest whole symbol, then padded with space
/// symbols to a block boundary: a burst always ends on a mark because its
/// final space has no terminating edge, so the missing tail is restored from
/// the known block length rather than being a decode error.
pub fn decode(timings: &[i64]) -> Decoded {
    let mut symbols: Vec<bool> = Vec::new();
    for &t in timings {
        let count = ((t.abs() as f64 / SYMBOL_US as f64).round() as usize).max(1);
        symbols.extend(std::iter::repeat_n(t > 0, count));
    }
    while symbols.len() % BLOCK_SYMBOLS != 0 {
        symbols.push(false);
    }

    let mut decoded = Decoded::default();
    for (index, block) in symbols.chunks_exact(BLOCK_SYMBOLS).enumerate() {
        if block[..SYNC_SYMBOLS] != SYNC {
            decoded.problems.push(Problem::Sync { block: index });
        }

        let mut bits = [false; BITS_PER_BLOCK];
        let mut violated = false;
        for (bit, pair) in block[SYNC_SYMBOLS..].chunks_exact(2).enumerate() {
            match pair {
                [true, false] => bits[bit] = true,
                [false, true] => bits[bit] = false,
                _ => violated = true,
            }
        }
        if violated {
            decoded.problems.push(Problem::Manchester { block: index });
            decoded.blocks.push(None);
            continue;
        }

        if !bits[10] {
            decoded.problems.push(Problem::StopBit { block: index });
        }
        if bits[..10].iter().filter(|&&b| b).count() % 2 == 1 {
            decoded.problems.push(Problem::Parity { block: index });
        }
        if bits[8] != (index == 0) {
            decoded.problems.push(Problem::StartOfFrame { block: index, value: bits[8] });
        }
        decoded.blocks.push(Some(bits[..8].iter().fold(0u8, |acc, &b| acc << 1 | u8::from(b))));
    }

    if decoded.blocks.len() != FRAME_BLOCKS {
        decoded.problems.push(Problem::BlockCount { blocks: decoded.blocks.len() });
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This house's remote, as captured and verified on air.
    const HOUSE: Frame =
        Frame { serial1: 0x00, serial2: 0x86, version: 0x02, cmd1: 0x01, cmd2: 0x32 };
    const HOUSE_KEYS: Keys = Keys { k1: 0x0a, k2: 0x86 };

    #[test]
    fn checksum_matches_the_inherited_table() {
        // First two rows of tests/cmd.csv (remote 21dd): the key derived from
        // one row must reproduce the other row's checksums.
        let k1 = derive_key(0x02, 0x3f);
        let k2 = derive_key(0x3e, 0x16);
        assert_eq!(checksum(0x51, k1), 0xa9);
        assert_eq!(checksum(0xe2, k2), 0xe7);
    }

    #[test]
    fn a_frame_survives_an_encode_decode_round_trip() {
        let decoded = decode(&HOUSE.to_timings(HOUSE_KEYS));

        assert!(decoded.is_clean(), "problems: {:?}", decoded.problems);
        assert_eq!(decoded.frame(), Some(HOUSE));
        assert_eq!(decoded.keys(), Some(HOUSE_KEYS));
    }

    #[test]
    fn encoding_ends_on_a_mark_and_spans_seven_blocks() {
        let timings = HOUSE.to_timings(HOUSE_KEYS);

        assert!(*timings.last().unwrap() > 0);
        let symbols: i64 = timings.iter().map(|t| t.abs()).sum::<i64>() / SYMBOL_US;
        // The unobservable trailing space is the only symbol missing.
        assert_eq!(symbols, (FRAME_BLOCKS * BLOCK_SYMBOLS) as i64 - 1);
    }

    #[test]
    fn realistic_timing_error_does_not_change_the_decode() {
        // Captures read marks ~100 µs short and spaces ~100 µs long (slicing
        // threshold above the envelope midpoint). Decoding must be immune.
        // Subtracting 100 shortens marks and lengthens spaces, both by 100 µs.
        let skewed: Vec<i64> = HOUSE.to_timings(HOUSE_KEYS).into_iter().map(|t| t - 100).collect();

        assert_eq!(decode(&skewed).frame(), Some(HOUSE));
    }

    /// Expand timings into one symbol per entry, for surgical corruption.
    fn to_symbols(timings: &[i64]) -> Vec<bool> {
        timings
            .iter()
            .flat_map(|&t| std::iter::repeat_n(t > 0, (t.abs() / SYMBOL_US) as usize))
            .collect()
    }

    fn to_timings(symbols: &[bool]) -> Vec<i64> {
        symbols.iter().map(|&s| if s { SYMBOL_US } else { -SYMBOL_US }).collect()
    }

    #[test]
    fn a_flipped_data_bit_fails_parity_not_silently() {
        // Invert the first data bit of block 0: swapping a `01` pair to `10`
        // stays valid Manchester, so parity is the only check that can catch
        // the corruption.
        let mut symbols = to_symbols(&HOUSE.to_timings(HOUSE_KEYS));
        symbols.swap(SYNC_SYMBOLS, SYNC_SYMBOLS + 1);

        let decoded = decode(&to_timings(&symbols));
        assert!(decoded.problems.contains(&Problem::Parity { block: 0 }), "{:?}", decoded.problems);
        assert!(decoded.frame().is_none());
    }

    #[test]
    fn a_clobbered_stop_bit_is_a_manchester_violation() {
        // Extending the final mark across the stop bit's space makes the last
        // pair `11`, which must surface as a problem, not a silent success.
        let mut timings = HOUSE.to_timings(HOUSE_KEYS);
        *timings.last_mut().unwrap() += 2 * SYMBOL_US;

        let decoded = decode(&timings);
        assert!(!decoded.is_clean());
        assert!(decoded.frame().is_none());
    }

    #[test]
    fn a_truncated_burst_reports_block_count() {
        let timings = HOUSE.to_timings(HOUSE_KEYS);
        let half = &timings[..timings.len() / 2];

        let decoded = decode(half);
        assert!(decoded.problems.iter().any(|p| matches!(p, Problem::BlockCount { .. })));
        assert!(decoded.frame().is_none());
    }

    #[test]
    fn an_empty_burst_is_a_report_not_a_panic() {
        let decoded = decode(&[]);
        assert!(!decoded.is_clean());
        assert_eq!(decoded.blocks.len(), 0);
    }

    #[test]
    fn keys_recovered_from_any_frame_transfer_to_other_commands() {
        // The property that makes TX possible without solving serial->K.
        let observed = Frame { cmd2: 0x34, ..HOUSE };
        let decoded = decode(&observed.to_timings(HOUSE_KEYS));
        assert_eq!(decoded.keys(), Some(HOUSE_KEYS));
    }
}
