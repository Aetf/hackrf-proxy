//! OOK signal processing: IQ <-> Flipper-RAW timings.
//!
//! Pure functions, no hardware. This is where protocol analysis actually
//! happens, so it carries the unit tests: the M1 conclusions about the
//! Proflame framing are only as trustworthy as this code.

use std::collections::BTreeMap;

/// Interleaved i8 I/Q ("cs8"): each complex sample is two bytes.
pub const BYTES_PER_SAMPLE: usize = 2;

/// |I| + |Q| is at most 128 + 128; index 0..=257 covers every value.
const MAG_LEVELS: usize = 258;

/// A contiguous run of carrier-on (mark) or carrier-off (space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub on: bool,
    pub us: i64,
}

/// Amplitude statistics of a capture, used to place the slicing threshold.
#[derive(Debug, Clone, Copy)]
pub struct Levels {
    pub noise_floor: u16,
    pub signal: u16,
    pub threshold: u16,
}

/// L1 magnitude envelope (|I| + |Q|) of an interleaved cs8 buffer.
///
/// L1 rather than sqrt(I²+Q²) because OOK only needs a monotone function of
/// amplitude, and this keeps the demodulator integer-only.
pub fn envelope(iq: &[u8]) -> Vec<u16> {
    iq.chunks_exact(BYTES_PER_SAMPLE)
        .map(|s| (i32::from(s[0] as i8).abs() + i32::from(s[1] as i8).abs()) as u16)
        .collect()
}

/// Peak magnitude of a raw cs8 buffer, without materializing an envelope.
pub fn peak_magnitude(iq: &[u8]) -> u16 {
    iq.chunks_exact(BYTES_PER_SAMPLE)
        .map(|s| (i32::from(s[0] as i8).abs() + i32::from(s[1] as i8).abs()) as u16)
        .max()
        .unwrap_or(0)
}

/// Derive a slicing threshold from the amplitude distribution.
///
/// The noise floor is the median and the signal level is the 99.9th
/// percentile, both read off an exact 258-bucket histogram. Using a
/// percentile rather than the raw maximum means one stray spike — an
/// unrelated burst, a USB glitch — cannot drag the threshold above the
/// signal we care about.
///
/// Taking the median as the noise floor assumes carrier is present for well
/// under half the capture, which holds for short bursts in a multi-second
/// recording. When it does not hold, the noise floor and signal level collapse
/// onto each other, which the caller can detect by comparing the two.
pub fn levels(mag: &[u16], frac: f64) -> Levels {
    let mut hist = AmplitudeHistogram::new();
    for &m in mag {
        hist.add_magnitude(m);
    }
    hist.levels(frac)
}

/// Streaming amplitude statistics, so a long capture never has to be held in
/// memory to be characterised.
#[derive(Clone)]
pub struct AmplitudeHistogram {
    counts: [usize; MAG_LEVELS],
    total: usize,
    peak: u16,
}

impl Default for AmplitudeHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl AmplitudeHistogram {
    pub fn new() -> Self {
        Self { counts: [0; MAG_LEVELS], total: 0, peak: 0 }
    }

    pub fn add_magnitude(&mut self, magnitude: u16) {
        self.counts[usize::from(magnitude).min(MAG_LEVELS - 1)] += 1;
        self.total += 1;
        self.peak = self.peak.max(magnitude);
    }

    /// Accumulate straight from an interleaved cs8 buffer.
    pub fn add_iq(&mut self, iq: &[u8]) {
        for sample in iq.chunks_exact(BYTES_PER_SAMPLE) {
            let magnitude =
                (i32::from(sample[0] as i8).abs() + i32::from(sample[1] as i8).abs()) as u16;
            self.add_magnitude(magnitude);
        }
    }

    pub fn peak(&self) -> u16 {
        self.peak
    }

    pub fn levels(&self, frac: f64) -> Levels {
        let noise_floor = percentile(&self.counts, self.total, 0.50);
        let signal = percentile(&self.counts, self.total, 0.999);
        let span = f64::from(signal.saturating_sub(noise_floor));
        let threshold = noise_floor.saturating_add((span * frac).round() as u16);
        Levels { noise_floor, signal, threshold }
    }
}

fn percentile(hist: &[usize; MAG_LEVELS], total: usize, p: f64) -> u16 {
    if total == 0 {
        return 0;
    }
    let target = ((total as f64) * p).ceil() as usize;
    let mut acc = 0usize;
    for (value, &count) in hist.iter().enumerate() {
        acc += count;
        if acc >= target {
            return value as u16;
        }
    }
    (MAG_LEVELS - 1) as u16
}

/// Slice the envelope into alternating mark/space runs.
///
/// Runs shorter than `min_us` are treated as glitches and folded into the run
/// they interrupted.
pub fn runs(mag: &[u16], threshold: u16, sample_rate: u32, min_us: u32) -> Vec<Run> {
    if mag.is_empty() {
        return Vec::new();
    }
    let us_per_sample = 1_000_000.0 / f64::from(sample_rate);
    let min_samples = ((f64::from(min_us) / us_per_sample).round()).max(1.0) as usize;

    let mut out: Vec<Run> = Vec::new();
    let mut current_on = mag[0] >= threshold;
    let mut len = 0usize;
    for &m in mag {
        let on = m >= threshold;
        if on == current_on {
            len += 1;
        } else {
            push_run(&mut out, current_on, len, min_samples, us_per_sample);
            current_on = on;
            len = 1;
        }
    }
    push_run(&mut out, current_on, len, min_samples, us_per_sample);
    out
}

fn push_run(out: &mut Vec<Run>, on: bool, len: usize, min_samples: usize, us_per_sample: f64) {
    let us = (len as f64 * us_per_sample).round() as i64;
    match out.last_mut() {
        // Too short to be a real symbol: charge its duration to the run it
        // interrupted rather than emitting it.
        Some(prev) if len < min_samples => prev.us += us,
        // Same polarity as the previous run, which happens once a glitch has
        // been folded in. Without coalescing here the timings would stop
        // alternating and every downstream consumer (histogram, Flipper RAW,
        // the transmitter) would misread the frame.
        Some(prev) if prev.on == on => prev.us += us,
        _ => out.push(Run { on, us }),
    }
}

/// Split runs into bursts on spaces of at least `gap_us`, returning each burst
/// as a Flipper-RAW timing array (positive = mark, negative = space).
///
/// A remote keypress typically emits the same frame several times; splitting
/// keeps the inter-frame gaps out of the pulse-width histogram and lets the
/// repeats be compared against each other.
///
/// A burst always ends on a mark. This is a property of OOK, not of this
/// function: a frame's final space has no terminating edge, so it merges
/// indistinguishably into the silence that follows. A frame whose last symbol
/// is a space — a Manchester frame ending in a one, for instance — can only be
/// recovered by padding it out from the known symbol clock, which is a job for
/// the protocol decoder, not the demodulator.
pub fn split_bursts(runs: &[Run], gap_us: i64, min_edges: usize) -> Vec<Vec<i64>> {
    let mut bursts = Vec::new();
    let mut current: Vec<i64> = Vec::new();
    for run in runs {
        if !run.on && run.us >= gap_us {
            finish_burst(&mut current, &mut bursts, min_edges);
            continue;
        }
        if current.is_empty() && !run.on {
            continue; // leading silence
        }
        current.push(if run.on { run.us } else { -run.us });
    }
    finish_burst(&mut current, &mut bursts, min_edges);
    bursts
}

fn finish_burst(current: &mut Vec<i64>, out: &mut Vec<Vec<i64>>, min_edges: usize) {
    // Only reachable when the recording ends mid-silence, since a separator gap
    // is never appended in the first place.
    while current.last().is_some_and(|&t| t < 0) {
        current.pop();
    }
    if current.len() >= min_edges {
        out.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

/// Bucketed pulse-width histogram. Distinct clusters reveal the symbol
/// durations (and therefore the bit clock and encoding).
pub fn width_histogram(timings: &[i64], bucket_us: i64) -> BTreeMap<i64, usize> {
    let mut hist = BTreeMap::new();
    for &t in timings {
        let bucket = t.div_euclid(bucket_us) * bucket_us;
        *hist.entry(bucket).or_default() += 1;
    }
    hist
}

/// How closely the repeats within a keypress agree.
///
/// Identical repeats mean the frame is static, which is the observable
/// signature of "no rolling code" — worth checking rather than assuming.
#[derive(Debug, Clone, Copy)]
pub struct BurstAgreement {
    pub same_edge_count: bool,
    pub max_deviation_us: i64,
}

pub fn compare_bursts(bursts: &[Vec<i64>]) -> Option<BurstAgreement> {
    let first = bursts.first()?;
    let mut same_edge_count = true;
    let mut max_deviation_us = 0;
    for other in &bursts[1..] {
        if other.len() != first.len() {
            same_edge_count = false;
            continue;
        }
        for (a, b) in first.iter().zip(other) {
            max_deviation_us = max_deviation_us.max((a - b).abs());
        }
    }
    Some(BurstAgreement { same_edge_count, max_deviation_us })
}

/// Render Flipper-RAW timings as baseband cs8 for transmission.
///
/// Mark is a constant-amplitude DC baseband, which upconverts to an unmodulated
/// carrier at the tuned frequency; space is zero.
pub fn synthesize(timings: &[i64], sample_rate: u32, amplitude: i8) -> Vec<u8> {
    let samples_per_us = f64::from(sample_rate) / 1_000_000.0;
    let mut out = Vec::new();
    for &t in timings {
        let count = (t.unsigned_abs() as f64 * samples_per_us).round() as usize;
        let i = if t > 0 { amplitude as u8 } else { 0 };
        for _ in 0..count {
            out.push(i);
            out.push(0);
        }
    }
    out
}

/// Total on-air duration of a timing array.
pub fn duration_us(timings: &[i64]) -> i64 {
    timings.iter().map(|t| t.abs()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 2_000_000;

    fn demod(iq: &[u8], min_us: u32) -> Vec<i64> {
        let mag = envelope(iq);
        let levels = levels(&mag, 0.5);
        let runs = runs(&mag, levels.threshold, RATE, min_us);
        runs.iter().map(|r| if r.on { r.us } else { -r.us }).collect()
    }

    #[test]
    fn timings_survive_a_synthesize_demodulate_round_trip() {
        let original = vec![500, -500, 250, -750, 1000, -500, 250];
        let iq = synthesize(&original, RATE, 100);
        assert_eq!(demod(&iq, 40), original);
    }

    #[test]
    fn glitches_are_folded_without_breaking_alternation() {
        // A 20 µs space inside a mark is below the 40 µs deglitch floor, so the
        // surrounding marks must merge into one run rather than emitting two
        // marks in a row.
        let iq = synthesize(&[500, -20, 500, -500, 500], RATE, 100);
        let timings = demod(&iq, 40);

        assert_eq!(timings, vec![1020, -500, 500]);
        for pair in timings.windows(2) {
            assert!(
                pair[0].signum() != pair[1].signum(),
                "timings must alternate mark/space, got {timings:?}"
            );
        }
    }

    #[test]
    fn bursts_split_on_long_gaps_and_drop_the_gap_itself() {
        let frame = [500i64, -500, 500];
        let mut timings = frame.to_vec();
        timings.push(-50_000);
        timings.extend_from_slice(&frame);

        let iq = synthesize(&timings, RATE, 100);
        let mag = envelope(&iq);
        let levels = levels(&mag, 0.5);
        let bursts = split_bursts(&runs(&mag, levels.threshold, RATE, 40), 10_000, 2);

        assert_eq!(bursts, vec![frame.to_vec(), frame.to_vec()]);
    }

    #[test]
    fn a_trailing_space_is_unobservable_so_bursts_end_on_a_mark() {
        // The frame's final -500 has no terminating edge: on air it is
        // continuous with the gap that follows, so the demodulator cannot see
        // it. The protocol decoder has to restore it from the symbol clock.
        let frame = [500i64, -500, 500, -500];
        let mut timings = frame.to_vec();
        timings.push(-50_000);
        timings.extend_from_slice(&frame);

        let iq = synthesize(&timings, RATE, 100);
        let mag = envelope(&iq);
        let levels = levels(&mag, 0.5);
        let bursts = split_bursts(&runs(&mag, levels.threshold, RATE, 40), 10_000, 2);

        assert_eq!(bursts.len(), 2);
        assert_eq!(bursts[0], vec![500, -500, 500]);
        assert_eq!(*bursts[0].last().unwrap(), 500);
    }

    #[test]
    fn a_lone_spike_does_not_capture_the_threshold() {
        // A realistic capture: mostly noise, a short weak burst, and one
        // full-scale outlier. The threshold must track the burst, not the
        // outlier.
        let mut mag = vec![4u16; 9_500];
        mag.extend(std::iter::repeat_n(40u16, 500));
        mag.push(250);

        let levels = levels(&mag, 0.5);

        assert_eq!(levels.noise_floor, 4);
        assert_eq!(levels.signal, 40, "the 250 outlier must not set the signal level");
        assert!(
            (4..40).contains(&levels.threshold),
            "threshold {} should sit between noise and signal",
            levels.threshold
        );
    }

    #[test]
    fn a_capture_that_is_mostly_carrier_collapses_the_levels() {
        // Documents the limit of the median-as-noise-floor assumption: with
        // carrier present most of the time the two levels converge, which is
        // what lets the CLI warn instead of silently slicing at the wrong place.
        let mut mag = vec![4u16; 2_000];
        mag.extend(std::iter::repeat_n(40u16, 8_000));

        let levels = levels(&mag, 0.5);

        assert_eq!(levels.noise_floor, levels.signal);
        assert!(levels.signal.saturating_sub(levels.noise_floor) < 10);
    }

    #[test]
    fn identical_repeats_report_zero_deviation() {
        let bursts = vec![vec![500, -500, 500], vec![500, -500, 500]];
        let agreement = compare_bursts(&bursts).unwrap();

        assert!(agreement.same_edge_count);
        assert_eq!(agreement.max_deviation_us, 0);
    }

    #[test]
    fn differing_repeats_are_reported() {
        let bursts = vec![vec![500, -500, 500], vec![500, -500, 620]];
        let agreement = compare_bursts(&bursts).unwrap();

        assert!(agreement.same_edge_count);
        assert_eq!(agreement.max_deviation_us, 120);
    }

    #[test]
    fn negative_widths_bucket_downwards() {
        // -510 µs belongs with -550..-501, not with -500..-451; getting this
        // wrong would smear mark and space clusters together in the histogram.
        let hist = width_histogram(&[-510, -500, 500, 510], 50);

        assert_eq!(hist.get(&-550), Some(&1));
        assert_eq!(hist.get(&-500), Some(&1));
        assert_eq!(hist.get(&500), Some(&2));
    }
}
