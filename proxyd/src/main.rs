//! hackrf-proxyd M1 spike CLI (`hrf`).
//!
//! Standalone research tool, not the daemon yet. Three jobs:
//!   * `info`      — prove we can talk to the HackRF at all.
//!   * `capture`   — RX raw IQ to a `.cs8` file (interleaved i8 I/Q).
//!   * `demod`     — offline: `.cs8` -> OOK envelope -> Flipper-RAW timings
//!                   (+ pulse-width histogram to help re-derive the protocol).
//!   * `transmit`  — Flipper-RAW timings JSON -> OOK IQ -> TX (replay).
//!
//! Capture and demod are split on purpose: press the remote once, then iterate
//! on demodulation offline. The Proflame protocol notes are untrusted, so the
//! histogram is the real deliverable of M1.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use seify_hackrfone::{Config, HackRf};

/// Interleaved i8 I/Q ("cs8"): each sample is two bytes.
const BYTES_PER_SAMPLE: usize = 2;
/// HackRF USB bulk transfers must be a multiple of 512 bytes.
const XFER_ALIGN: usize = 512;

#[derive(Parser)]
#[command(name = "hrf", about = "HackRF M1 spike: capture / demod / transmit OOK")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the first HackRF and print board id / firmware version.
    Info,

    /// Receive raw IQ to a .cs8 file for `--seconds`.
    Capture {
        #[arg(long, default_value_t = 315_000_000)]
        freq: u64,
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        #[arg(long, default_value_t = 40)]
        lna: u16,
        #[arg(long, default_value_t = 40)]
        vga: u16,
        /// Enable the front-end RX amplifier (+14 dB).
        #[arg(long)]
        amp: bool,
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
        #[arg(long)]
        out: PathBuf,
    },

    /// Offline: turn a .cs8 capture into OOK timings + a pulse-width histogram.
    Demod {
        #[arg(long)]
        r#in: PathBuf,
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        /// Threshold as a fraction (0..1) of the observed peak magnitude.
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// Ignore mark/space runs shorter than this many microseconds (deglitch).
        #[arg(long, default_value_t = 40)]
        min_us: u32,
        /// Write timings JSON here (Flipper RAW signed-µs array).
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Transmit OOK from a Flipper-RAW timings JSON (signed alternating µs).
    Transmit {
        #[arg(long, default_value_t = 315_000_000)]
        freq: u64,
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        /// TX VGA (IF) gain, 0..47 dB.
        #[arg(long, default_value_t = 40)]
        txvga: u16,
        /// Enable the TX power amplifier (+~11 dB). Keep off for close-range tests.
        #[arg(long)]
        amp: bool,
        /// Repeat the whole frame this many extra times (0 = send once).
        #[arg(long, default_value_t = 4)]
        repeat: u32,
        /// Gap in µs inserted between repeats.
        #[arg(long, default_value_t = 10_000)]
        gap_us: u32,
        /// JSON file: `[123, -456, ...]` (positive = carrier on, negative = off).
        #[arg(long)]
        file: PathBuf,
    },
}

fn open() -> Result<HackRf> {
    HackRf::open_first().context("no HackRF found (plugged in? udev permissions?)")
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match Cli::parse().cmd {
        Cmd::Info => info(),
        Cmd::Capture { freq, rate, lna, vga, amp, seconds, out } => {
            capture(freq, rate, lna, vga, amp, seconds, &out)
        }
        Cmd::Demod { r#in, rate, threshold, min_us, out } => {
            demod(&r#in, rate, threshold, min_us, out.as_deref())
        }
        Cmd::Transmit { freq, rate, txvga, amp, repeat, gap_us, file } => {
            transmit(freq, rate, txvga, amp, repeat, gap_us, &file)
        }
    }
}

fn info() -> Result<()> {
    let radio = open()?;
    println!("board_id : {}", radio.board_id()?);
    println!("version  : {}", radio.version()?);
    println!("usb ver  : {:?}", radio.device_version());
    Ok(())
}

fn rx_config(freq: u64, rate: u32, lna: u16, vga: u16, amp: bool) -> Config {
    Config {
        lna_db: lna,
        vga_db: vga,
        txvga_db: 0,
        amp_enable: amp,
        antenna_enable: false,
        frequency_hz: freq,
        sample_rate_hz: rate,
        sample_rate_div: 1,
    }
}

fn capture(freq: u64, rate: u32, lna: u16, vga: u16, amp: bool, seconds: f64, out: &PathBuf) -> Result<()> {
    let radio = open()?;
    radio.start_rx(&rx_config(freq, rate, lna, vga, amp))?;

    let total_samples = (seconds * rate as f64) as usize;
    let mut remaining_bytes = total_samples * BYTES_PER_SAMPLE;
    let mut file = BufWriter::new(File::create(out).with_context(|| format!("create {}", out.display()))?);
    let mut buf = vec![0u8; 262_144]; // multiple of 512

    log::info!("capturing {seconds:.1}s @ {freq} Hz, {rate} sps -> {}", out.display());
    while remaining_bytes > 0 {
        let n = radio.read(&mut buf)?;
        let take = n.min(remaining_bytes);
        file.write_all(&buf[..take])?;
        remaining_bytes -= take;
    }
    radio.stop()?;
    file.flush()?;
    log::info!("done: {} samples", total_samples);
    Ok(())
}

/// A contiguous run of "carrier on" (mark) or "carrier off" (space).
struct Run {
    on: bool,
    us: i64,
}

fn demod(input: &PathBuf, rate: u32, threshold_frac: f64, min_us: u32, out: Option<&std::path::Path>) -> Result<()> {
    let mut reader = BufReader::new(File::open(input).with_context(|| format!("open {}", input.display()))?);
    let mut raw = Vec::new();
    reader.read_to_end(&mut raw)?;
    let n_samples = raw.len() / BYTES_PER_SAMPLE;
    anyhow::ensure!(n_samples > 0, "empty capture");

    // Envelope: |I| + |Q| (cheap L1 magnitude, plenty for OOK).
    let mut mag = vec![0u16; n_samples];
    let mut peak = 0u16;
    for i in 0..n_samples {
        let ii = raw[i * 2] as i8 as i32;
        let qq = raw[i * 2 + 1] as i8 as i32;
        let m = (ii.abs() + qq.abs()) as u16;
        mag[i] = m;
        peak = peak.max(m);
    }
    let thr = (peak as f64 * threshold_frac).round() as u16;
    log::info!("{} samples, peak L1 magnitude {peak}, threshold {thr}", n_samples);

    let us_per_sample = 1_000_000.0 / rate as f64;
    let min_samples = ((min_us as f64) / us_per_sample).round() as i64;

    // Walk the envelope into on/off runs, deglitching short runs by merging.
    let mut runs: Vec<Run> = Vec::new();
    let mut cur_on = mag[0] >= thr;
    let mut run_len: i64 = 0;
    for &m in &mag {
        let on = m >= thr;
        if on == cur_on {
            run_len += 1;
        } else {
            push_run(&mut runs, cur_on, run_len, min_samples, us_per_sample);
            cur_on = on;
            run_len = 1;
        }
    }
    push_run(&mut runs, cur_on, run_len, min_samples, us_per_sample);

    // Trim leading/trailing silence so the histogram reflects the burst only.
    while runs.first().is_some_and(|r| !r.on) {
        runs.remove(0);
    }
    while runs.last().is_some_and(|r| !r.on) {
        runs.pop();
    }

    // Signed timings (Flipper RAW convention).
    let timings: Vec<i64> = runs.iter().map(|r| if r.on { r.us } else { -r.us }).collect();

    // Pulse-width histogram bucketed to 50 µs — the actual protocol-analysis aid.
    let mut hist: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
    for &t in &timings {
        let bucket = (t / 50) * 50;
        *hist.entry(bucket).or_default() += 1;
    }
    println!("edges: {}", timings.len());
    println!("pulse-width histogram (bucket µs -> count; +mark / -space):");
    for (bucket, count) in &hist {
        println!("  {bucket:>7} : {count}");
    }

    if let Some(path) = out {
        let mut f = BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut f, &timings)?;
        f.flush()?;
        log::info!("wrote {} timings -> {}", timings.len(), path.display());
    }
    Ok(())
}

fn push_run(runs: &mut Vec<Run>, on: bool, len: i64, min_samples: i64, us_per_sample: f64) {
    let us = (len as f64 * us_per_sample).round() as i64;
    // Merge sub-min glitches into the previous run of the same-ish level by
    // dropping them: if a run is too short, fold its duration into its neighbor.
    if len < min_samples {
        if let Some(prev) = runs.last_mut() {
            prev.us += us;
            return;
        }
    }
    runs.push(Run { on, us });
}

fn transmit(freq: u64, rate: u32, txvga: u16, amp: bool, repeat: u32, gap_us: u32, file: &PathBuf) -> Result<()> {
    let json = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let timings: Vec<i64> = serde_json::from_str(&json).context("timings must be a JSON array of signed integers (µs)")?;
    anyhow::ensure!(!timings.is_empty(), "no timings");

    // Synthesize one frame of OOK baseband: mark = constant amplitude (carrier
    // via LO), space = zero. DC-baseband OOK is the simplest replay; if LO
    // leakage on "space" bleeds through we'll add a small IF offset later.
    const ON_I: i8 = 100;
    let samples_per_us = rate as f64 / 1_000_000.0;
    let mut frame: Vec<u8> = Vec::new();
    for &t in &timings {
        let on = t > 0;
        let dur_us = t.unsigned_abs();
        let n = (dur_us as f64 * samples_per_us).round() as usize;
        let (i, q) = if on { (ON_I as u8, 0u8) } else { (0u8, 0u8) };
        for _ in 0..n {
            frame.push(i);
            frame.push(q);
        }
    }

    // Assemble repeats with inter-frame gaps of silence.
    let gap_samples = (gap_us as f64 * samples_per_us).round() as usize;
    let gap: Vec<u8> = vec![0u8; gap_samples * BYTES_PER_SAMPLE];
    let mut buf: Vec<u8> = Vec::new();
    for r in 0..=repeat {
        buf.extend_from_slice(&frame);
        if r < repeat {
            buf.extend_from_slice(&gap);
        }
    }
    // Pad to 512-byte alignment required by the USB bulk endpoint.
    while buf.len() % XFER_ALIGN != 0 {
        buf.push(0);
    }

    let radio = open()?;
    radio.start_tx(&Config {
        lna_db: 0,
        vga_db: 0,
        txvga_db: txvga,
        amp_enable: amp,
        antenna_enable: false,
        frequency_hz: freq,
        sample_rate_hz: rate,
        sample_rate_div: 1,
    })?;
    log::info!(
        "TX {} bytes ({} frames) @ {freq} Hz, txvga {txvga}, amp {amp}",
        buf.len(),
        repeat + 1
    );
    let mut off = 0;
    while off < buf.len() {
        let end = (off + 262_144).min(buf.len());
        let end = off + ((end - off) / XFER_ALIGN) * XFER_ALIGN;
        let end = if end == off { buf.len() } else { end };
        let n = radio.write(&buf[off..end])?;
        off += n;
    }
    radio.stop()?;
    log::info!("TX done");
    Ok(())
}
