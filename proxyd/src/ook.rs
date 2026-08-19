//! OOK signal processing: IQ <-> Flipper-RAW timings.
//!
//! Pure functions, no hardware. This is where protocol analysis actually
//! happens, so it carries the unit tests: any conclusion drawn about a
//! protocol's framing is only as trustworthy as this code.

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

    /// Magnitude at the given quantile of the window.
    pub fn percentile(&self, p: f64) -> u16 {
        percentile(&self.counts, self.total, p)
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

/// Assembles completed runs into bursts, splitting on spaces of at least
/// `gap_us`.
///
/// Shared by the offline splitter and the streaming [`Detector`] so the two
/// cannot drift apart: burst boundaries, the minimum edge count and the
/// trailing-space rule are defined once, here.
///
/// A burst always ends on a mark. This is a property of OOK, not of this
/// code: a frame's final space has no terminating edge, so it merges
/// indistinguishably into the silence that follows. A frame whose last symbol
/// is a space — a Manchester frame ending in a one, for instance — can only be
/// recovered by padding it out from the known symbol clock, which is a job for
/// the protocol decoder, not the demodulator.
#[derive(Debug, Clone)]
pub struct BurstAccumulator {
    gap_us: i64,
    min_edges: usize,
    max_edges: usize,
    current: Vec<i64>,
    overflows: u64,
}

/// Ceiling on edges held for one burst, so a jammed band cannot grow the
/// buffer without bound. A Proflame frame is 364 symbols; this is ample.
const MAX_BURST_EDGES: usize = 4096;

impl BurstAccumulator {
    pub fn new(gap_us: i64, min_edges: usize) -> Self {
        Self { gap_us, min_edges, max_edges: MAX_BURST_EDGES, current: Vec::new(), overflows: 0 }
    }

    /// Feed one completed run; returns a burst if the run ended one.
    pub fn push(&mut self, run: Run) -> Option<Vec<i64>> {
        if !run.on && run.us >= self.gap_us {
            return self.finish();
        }
        if self.current.is_empty() && !run.on {
            return None; // leading silence
        }
        self.current.push(if run.on { run.us } else { -run.us });
        if self.current.len() > self.max_edges {
            // Continuous edges with no gap: not a frame this tool can use, and
            // keeping it would grow without bound.
            self.current.clear();
            self.overflows += 1;
        }
        None
    }

    /// Close the burst in progress, whether or not a gap ended it. The
    /// streaming detector calls this once a silence has run past `gap_us`,
    /// without waiting for the carrier to return; the offline path calls it
    /// when the recording ends.
    pub fn finish(&mut self) -> Option<Vec<i64>> {
        // Only reachable when the input ends mid-silence, since a separator
        // gap is never appended in the first place.
        while self.current.last().is_some_and(|&t| t < 0) {
            self.current.pop();
        }
        if self.current.len() >= self.min_edges {
            Some(std::mem::take(&mut self.current))
        } else {
            self.current.clear();
            None
        }
    }

    /// Whether a burst is currently being accumulated.
    pub fn is_active(&self) -> bool {
        !self.current.is_empty()
    }

    /// Bursts discarded for exceeding [`MAX_BURST_EDGES`].
    pub fn overflows(&self) -> u64 {
        self.overflows
    }
}

/// Split runs into bursts on spaces of at least `gap_us`, returning each burst
/// as a Flipper-RAW timing array (positive = mark, negative = space).
///
/// A remote keypress typically emits the same frame several times; splitting
/// keeps the inter-frame gaps out of the pulse-width histogram and lets the
/// repeats be compared against each other.
pub fn split_bursts(runs: &[Run], gap_us: i64, min_edges: usize) -> Vec<Vec<i64>> {
    let mut accumulator = BurstAccumulator::new(gap_us, min_edges);
    let mut bursts: Vec<Vec<i64>> = runs.iter().filter_map(|&run| accumulator.push(run)).collect();
    bursts.extend(accumulator.finish());
    bursts
}

/// A burst found in a live stream, with the level it arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Burst {
    /// Flipper-RAW timings: positive is mark, negative is space.
    pub timings: Vec<i64>,
    /// Peak L1 magnitude during the burst, 0..=256. Uncalibrated: this is
    /// raw ADC amplitude, not dBm, and depends on the configured gains.
    pub peak: u16,
}

/// How the streaming detector slices a live stream.
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    pub sample_rate: u32,
    /// Slicing level between noise floor and signal, as a fraction.
    pub threshold_fraction: f64,
    /// Runs shorter than this are folded into their neighbours.
    pub min_us: u32,
    /// A space at least this long ends a burst.
    ///
    /// This must be shorter than the shortest inter-frame gap worth keeping
    /// apart: Proflame repeats sit 4.15 ms apart, so the offline default of
    /// 10 ms would merge a whole keypress into one blob.
    pub gap_us: i64,
    /// Bursts with fewer edges than this are noise, not frames.
    pub min_edges: usize,
    /// How far above the noise floor, in robust standard deviations, the
    /// threshold is never allowed to sit below. This is what keeps a quiet
    /// band from slicing its own noise into frames.
    pub noise_sigmas: f64,
    /// How much signal the adaptive threshold is derived from at a time.
    pub window_samples: usize,
}

impl DetectorConfig {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            threshold_fraction: 0.5,
            min_us: 40,
            gap_us: 3_000,
            min_edges: 8,
            noise_sigmas: 8.0,
            // One second: long enough that a keypress is a small fraction of
            // the window, so the 99.9th percentile still finds the noise
            // floor rather than the signal.
            window_samples: sample_rate as usize,
        }
    }
}

/// Finds bursts in a continuous stream, adapting its threshold as it goes.
///
/// The offline [`levels`] path is two-pass: it reads the whole capture, then
/// slices it. A live receiver cannot do that, so the threshold here is
/// recomputed once per window from that window's amplitude distribution.
///
/// The threshold stays armed at all times, which is what lets the *first*
/// keypress after a long silence be received rather than merely teaching the
/// detector what a keypress looks like. The exception is the first window
/// after startup, during which there is no measurement at all.
///
/// Deciding where to put the threshold without knowing whether the window
/// contains signal is the whole problem. Asking whether the 99.9th percentile
/// clears the median does not work: on a real band the *noise alone* spans far
/// more than any fixed margin, so quiet windows look like signal and the
/// threshold lands inside the noise. The fix is to bound the threshold from
/// below with a statistic a burst cannot move — the noise spread measured
/// between the median and the 75th percentile, which a signal occupying a
/// minority of the window leaves untouched.
///
/// So the threshold is the higher of a level sliced between noise and signal
/// (sensitive, and what the offline path computes) and a floor some multiple
/// of the noise spread above the median (robust, and all a quiet window can
/// support). On a band with signal the first wins and reproduces the offline
/// threshold; on a quiet band the second wins and nothing crosses it.
pub struct Detector {
    config: DetectorConfig,
    histogram: AmplitudeHistogram,
    window_seen: usize,
    /// `None` only until the first window has been measured.
    threshold: Option<u16>,
    min_samples: u64,
    gap_samples: u64,
    on: bool,
    run_samples: u64,
    /// One run of lookahead, so a glitch can still be charged to the run it
    /// interrupted — the streaming equivalent of mutating `out.last_mut()`.
    pending: Option<Run>,
    accumulator: BurstAccumulator,
    burst_peak: u16,
}

impl Detector {
    pub fn new(config: DetectorConfig) -> Self {
        let us_per_sample = 1_000_000.0 / f64::from(config.sample_rate);
        Self {
            histogram: AmplitudeHistogram::new(),
            window_seen: 0,
            threshold: None,
            min_samples: ((f64::from(config.min_us) / us_per_sample).round()).max(1.0) as u64,
            gap_samples: ((config.gap_us as f64 / us_per_sample).round()).max(1.0) as u64,
            on: false,
            run_samples: 0,
            pending: None,
            accumulator: BurstAccumulator::new(config.gap_us, config.min_edges),
            burst_peak: 0,
            config,
        }
    }

    /// The current slicing threshold, or `None` before the first window has
    /// been measured.
    pub fn threshold(&self) -> Option<u16> {
        self.threshold
    }

    /// Bursts discarded for having more edges than a frame plausibly holds —
    /// the signature of a jammed band or a badly placed threshold.
    pub fn overflows(&self) -> u64 {
        self.accumulator.overflows()
    }

    /// Feed one chunk of interleaved cs8, returning every burst it completed.
    pub fn push(&mut self, iq: &[u8]) -> Vec<Burst> {
        let mut bursts = Vec::new();
        for sample in iq.chunks_exact(BYTES_PER_SAMPLE) {
            let magnitude =
                (i32::from(sample[0] as i8).abs() + i32::from(sample[1] as i8).abs()) as u16;
            self.histogram.add_magnitude(magnitude);
            self.window_seen += 1;

            if let Some(threshold) = self.threshold {
                self.push_sample(magnitude, threshold, &mut bursts);
            }

            if self.window_seen >= self.config.window_samples {
                self.retune();
            }
        }
        bursts
    }

    fn push_sample(&mut self, magnitude: u16, threshold: u16, bursts: &mut Vec<Burst>) {
        let on = magnitude >= threshold;
        if on == self.on {
            self.run_samples += 1;
        } else {
            let run = self.finished_run();
            self.emit_run(run, bursts);
            self.on = on;
            self.run_samples = 1;
        }
        if on {
            self.burst_peak = self.burst_peak.max(magnitude);
        }

        // A silence that has already outrun the gap ends the burst now. Waiting
        // for the carrier to return would mean a frame is only delivered when
        // the *next* one arrives, and never at all once the band goes quiet.
        if !self.on && self.run_samples == self.gap_samples {
            if let Some(run) = self.pending.take() {
                if let Some(timings) = self.accumulator.push(run) {
                    bursts.push(self.take_burst(timings));
                }
            }
            if let Some(timings) = self.accumulator.finish() {
                bursts.push(self.take_burst(timings));
            }
        }
    }

    fn finished_run(&self) -> Run {
        let us_per_sample = 1_000_000.0 / f64::from(self.config.sample_rate);
        Run { on: self.on, us: (self.run_samples as f64 * us_per_sample).round() as i64 }
    }

    /// Hand a completed run to the accumulator, folding glitches and
    /// coalescing same-polarity runs exactly as the offline path does.
    fn emit_run(&mut self, run: Run, bursts: &mut Vec<Burst>) {
        let glitch = self.run_samples < self.min_samples;
        match &mut self.pending {
            Some(previous) if glitch || previous.on == run.on => {
                previous.us += run.us;
                return;
            }
            _ => {}
        }
        if let Some(previous) = self.pending.replace(run) {
            if let Some(timings) = self.accumulator.push(previous) {
                bursts.push(self.take_burst(timings));
            }
        }
    }

    fn take_burst(&mut self, timings: Vec<i64>) -> Burst {
        Burst { timings, peak: std::mem::take(&mut self.burst_peak) }
    }

    /// Recompute the threshold from the window just gathered, then start a
    /// fresh window so the detector keeps tracking the band rather than
    /// averaging over all time.
    fn retune(&mut self) {
        let floor = self.histogram.percentile(0.50);
        let quartile = self.histogram.percentile(0.75);
        let signal = self.histogram.percentile(0.999);

        // For a normal distribution the median-to-third-quartile distance is
        // 0.674 sigma. Unlike the 99.9th percentile it is immune to a burst
        // that occupies less than a quarter of the window, which is what
        // makes it usable when it is not yet known whether one is present.
        let sigma = f64::from(quartile.saturating_sub(floor)) / 0.674;
        let robust_floor = floor.saturating_add((sigma * self.config.noise_sigmas).round() as u16);

        let span = f64::from(signal.saturating_sub(floor));
        let sliced = floor.saturating_add((span * self.config.threshold_fraction).round() as u16);

        // Never zero: a threshold of zero would classify silence as carrier.
        self.threshold = Some(robust_floor.max(sliced).max(1));
        self.histogram = AmplitudeHistogram::new();
        self.window_seen = 0;
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

/// Render a transmission — a frame, repeated, with silence between the
/// repetitions — as a baseband cs8 buffer ready for the radio.
///
/// Shared by the CLI and the daemon so that what the research tool proved on
/// air and what the daemon sends are the same waveform.
pub fn render_transmission(
    timings: &[i64],
    repeat: u32,
    gap_us: u32,
    sample_rate: u32,
    amplitude: i8,
) -> Vec<u8> {
    let frame = synthesize(timings, sample_rate, amplitude);
    let gap = synthesize(&[-i64::from(gap_us)], sample_rate, amplitude);

    let mut samples = Vec::with_capacity((frame.len() + gap.len()) * (repeat as usize + 1));
    for iteration in 0..=repeat {
        samples.extend_from_slice(&frame);
        if iteration < repeat {
            samples.extend_from_slice(&gap);
        }
    }
    samples
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

    /// A detector with a short window, so tests do not have to synthesize a
    /// second of signal to get past the learning window.
    fn detector() -> Detector {
        let mut config = DetectorConfig::new(RATE);
        config.window_samples = 100_000; // 50 ms
        config.gap_us = 3_000;
        config.min_edges = 3;
        Detector::new(config)
    }

    /// Silence long enough to complete `windows` learning windows.
    fn quiet(windows: usize) -> Vec<u8> {
        vec![0u8; 100_000 * windows * BYTES_PER_SAMPLE]
    }

    const FRAME: [i64; 5] = [500, -500, 500, -500, 500];

    #[test]
    fn a_burst_is_detected_once_the_first_window_has_been_measured() {
        let mut detector = detector();

        assert!(detector.push(&quiet(1)).is_empty(), "nothing before a threshold exists");
        assert!(detector.threshold().is_some(), "the first window arms the threshold");

        let mut bursts = detector.push(&synthesize(&FRAME, RATE, 100));
        bursts.extend(detector.push(&quiet(1)));

        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].timings, FRAME);
        assert_eq!(bursts[0].peak, 100);
    }

    #[test]
    fn the_first_burst_after_a_quiet_band_is_not_missed() {
        // The reason the threshold is parked above the noise rather than
        // learned from the signal: a band can be silent for hours and the
        // very next keypress still has to be received, in the same window it
        // arrives in.
        let mut detector = detector();
        detector.push(&quiet(4));

        let mut iq = synthesize(&FRAME, RATE, 100);
        iq.extend(quiet(1));

        assert_eq!(detector.push(&iq).len(), 1, "the first burst must not be swallowed");
    }

    #[test]
    fn pure_noise_produces_no_bursts() {
        let mut detector = detector();
        // Noise that jitters around a floor, never a clean carrier.
        let noise: Vec<u8> =
            (0..400_000 * BYTES_PER_SAMPLE).map(|i| ((i * 7919) % 11) as u8).collect();

        let bursts = detector.push(&noise);

        assert!(bursts.is_empty(), "noise must not slice into frames: {bursts:?}");
    }

    #[test]
    fn a_burst_is_emitted_on_the_gap_without_waiting_for_the_next_one() {
        // The streaming property the offline splitter does not need: once the
        // silence outruns the gap the frame is delivered, rather than waiting
        // for a carrier that may never return.
        let mut detector = detector();
        detector.push(&quiet(1));

        detector.push(&synthesize(&FRAME, RATE, 100));
        // Exactly one gap of silence, and no more.
        let bursts = detector.push(&synthesize(&[-4_000], RATE, 100));

        assert_eq!(bursts.len(), 1, "the gap alone must complete the burst");
        assert_eq!(bursts[0].timings, FRAME);
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        // Runs, glitches and gaps all straddle transfer boundaries in a live
        // stream; feeding the same signal in small pieces must not change
        // what comes out.
        let mut signal = quiet(1);
        signal.extend(synthesize(&FRAME, RATE, 100));
        signal.extend(quiet(1));

        let whole = detector().push(&signal);

        let mut piecewise = detector();
        let mut pieces = Vec::new();
        for chunk in signal.chunks(777 * BYTES_PER_SAMPLE) {
            pieces.extend(piecewise.push(chunk));
        }

        assert_eq!(whole.len(), 1);
        assert_eq!(whole, pieces);
    }

    #[test]
    fn streaming_folds_glitches_exactly_as_the_offline_path_does() {
        let timings = [500i64, -20, 500, -500, 500];
        let mut signal = quiet(1);
        signal.extend(synthesize(&timings, RATE, 100));
        signal.extend(quiet(1));

        let streamed = detector().push(&signal);

        let magnitude = envelope(&signal);
        let levels = levels(&magnitude, 0.5);
        let offline = split_bursts(&runs(&magnitude, levels.threshold, RATE, 40), 3_000, 3);

        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].timings, vec![1020, -500, 500]);
        assert_eq!(streamed[0].timings, offline[0]);
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
