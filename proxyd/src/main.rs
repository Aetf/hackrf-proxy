//! hackrf-proxyd M1 spike CLI (`hrf`).
//!
//! Standalone research tool, not the daemon yet. It exists to answer one
//! question: can we capture the fireplace remote, understand its framing, and
//! replay it? Capture and demodulation are separate commands on purpose —
//! the remote gets pressed once, then the demodulator can be re-run offline
//! with different settings as often as needed.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use hackrf_proxyd::{ook, proflame, radio};

#[derive(Parser)]
#[command(name = "hrf", about = "HackRF M1 spike: capture, demodulate and replay OOK")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open the first HackRF and print board id and firmware version.
    Info,

    /// Watch received power on several frequencies at once, as a live meter.
    ///
    /// Hold a remote button down while this runs: the frequency it uses will
    /// stand out. Use it when a capture comes back as pure noise, since that
    /// looks identical whether the frequency is wrong, the remote is out of
    /// range, or the button was pressed outside the window.
    Scan {
        /// Comma-separated frequencies, e.g. `315M,433.92M`. Defaults to the
        /// FCC and CE Proflame bands plus their neighbours.
        #[arg(long, default_value = "315M,318M,390M,433.92M")]
        freqs: String,
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        #[arg(long, default_value_t = 24)]
        lna: u16,
        #[arg(long, default_value_t = 20)]
        vga: u16,
        /// Enable the front-end RX amplifier (+14 dB). Only for distant
        /// signals: near a transmitter it saturates the front end and every
        /// band reads full scale, which measures nothing.
        #[arg(long)]
        amp: bool,
        /// Seconds to listen on each frequency before moving on.
        #[arg(long, default_value_t = 0.3)]
        dwell: f64,
        #[arg(long, default_value_t = 40)]
        passes: u32,
    },

    /// Receive raw IQ to a cs8 file.
    Capture {
        #[arg(long, default_value = "315M", value_parser = parse_frequency)]
        freq: u64,
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        /// RX LNA gain, 0..=40 dB in 8 dB steps.
        #[arg(long, default_value_t = 40)]
        lna: u16,
        /// RX VGA gain, 0..=62 dB in 2 dB steps.
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

    /// Offline: turn a cs8 capture into per-burst timings and a pulse-width
    /// histogram.
    Demod {
        #[arg(long, value_name = "FILE")]
        r#in: PathBuf,
        /// Must match the rate the capture was taken at.
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        /// Slicing level between noise floor and signal, as a fraction.
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// Fold mark/space runs shorter than this many microseconds into their
        /// neighbours.
        #[arg(long, default_value_t = 40)]
        min_us: u32,
        /// A space at least this long separates one burst from the next.
        #[arg(long, default_value_t = 10_000)]
        gap_us: i64,
        /// Ignore bursts with fewer edges than this.
        #[arg(long, default_value_t = 8)]
        min_edges: usize,
        /// Histogram bucket width.
        #[arg(long, default_value_t = 50)]
        bucket_us: i64,
        /// Which burst to write out, 1-indexed.
        #[arg(long, default_value_t = 1)]
        burst: usize,
        /// Write the selected burst as a Flipper-RAW timings JSON array.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write every burst as a JSON array of timing arrays, for protocol
        /// analysis across repeats.
        #[arg(long, value_name = "FILE")]
        out_all: Option<PathBuf>,
    },

    /// Decode Proflame frames from demodulated timings.
    ///
    /// Accepts the JSON written by `demod --out` (one burst) or `--out-all`
    /// (a list of bursts). Prints every burst's fields, the framing rules
    /// that failed, and the per-remote checksum constants derived from the
    /// clean frames.
    Decode {
        /// Timings JSON file(s); repeatable.
        #[arg(long, value_name = "FILE", required = true)]
        r#in: Vec<PathBuf>,
    },

    /// Transmit OOK from a Flipper-RAW timings JSON array.
    Transmit {
        #[arg(long, default_value = "315M", value_parser = parse_frequency)]
        freq: u64,
        #[arg(long, default_value_t = 2_000_000)]
        rate: u32,
        /// TX VGA gain, 0..=47 dB.
        #[arg(long, default_value_t = 40)]
        txvga: u16,
        /// Enable the TX power amplifier. Leave off for close-range tests.
        #[arg(long)]
        amp: bool,
        /// Extra repetitions of the frame (0 sends it once).
        #[arg(long, default_value_t = 4)]
        repeat: u32,
        /// Silence inserted between repetitions.
        #[arg(long, default_value_t = 10_000)]
        gap_us: u32,
        /// JSON array of signed microseconds: positive is carrier on.
        #[arg(long)]
        file: PathBuf,
    },
}

/// HackRF One tunes from 1 MHz to 6 GHz.
const MIN_FREQUENCY_HZ: u64 = 1_000_000;
const MAX_FREQUENCY_HZ: u64 = 6_000_000_000;

/// Parse a frequency, accepting `315M`, `433.92MHz`, `315000000` and friends.
///
/// Bare numbers are hertz, which makes `--freq 315` mean 315 Hz — a mistake
/// that is easy to make and, without this check, produces a capture of noise
/// from a radio tuned nowhere near the intended band.
fn parse_frequency(input: &str) -> Result<u64> {
    let text = input.trim().to_ascii_lowercase();
    let text = text.strip_suffix("hz").unwrap_or(&text).trim();
    let (digits, multiplier) = match text.strip_suffix('g') {
        Some(rest) => (rest, 1e9),
        None => match text.strip_suffix('m') {
            Some(rest) => (rest, 1e6),
            None => match text.strip_suffix('k') {
                Some(rest) => (rest, 1e3),
                None => (text, 1.0),
            },
        },
    };
    let value: f64 = digits.trim().parse().with_context(|| format!("not a frequency: {input}"))?;
    ensure!(value.is_finite() && value > 0.0, "not a frequency: {input}");
    let hz = (value * multiplier).round() as u64;

    if hz < MIN_FREQUENCY_HZ {
        anyhow::bail!(
            "{input} is {hz} Hz, below the HackRF's 1 MHz minimum — \
             bare numbers are hertz, so write {digits}M for megahertz"
        );
    }
    ensure!(hz <= MAX_FREQUENCY_HZ, "{input} is above the HackRF's 6 GHz maximum");
    Ok(hz)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match Cli::parse().command {
        Command::Info => info(),
        Command::Scan { freqs, rate, lna, vga, amp, dwell, passes } => {
            let frequencies = freqs
                .split(',')
                .map(parse_frequency)
                .collect::<Result<Vec<_>>>()
                .context("--freqs must be a comma-separated frequency list")?;
            radio::scan(&radio::ScanParams {
                frequencies,
                sample_rate: rate,
                lna_db: lna,
                vga_db: vga,
                amp_enable: amp,
                dwell_seconds: dwell,
                passes,
            })
        }
        Command::Capture { freq, rate, lna, vga, amp, seconds, out } => radio::capture(
            &radio::CaptureParams {
                frequency_hz: freq,
                sample_rate: rate,
                lna_db: lna,
                vga_db: vga,
                amp_enable: amp,
                seconds,
            },
            &out,
        ),
        Command::Demod {
            r#in,
            rate,
            threshold,
            min_us,
            gap_us,
            min_edges,
            bucket_us,
            burst,
            out,
            out_all,
        } => demod(DemodArgs {
            input: &r#in,
            sample_rate: rate,
            threshold,
            min_us,
            gap_us,
            min_edges,
            bucket_us,
            burst,
            out: out.as_deref(),
            out_all: out_all.as_deref(),
        }),
        Command::Decode { r#in } => decode(&r#in),
        Command::Transmit { freq, rate, txvga, amp, repeat, gap_us, file } => {
            transmit(freq, rate, txvga, amp, repeat, gap_us, &file)
        }
    }
}

fn info() -> Result<()> {
    let radio = radio::open()?;
    println!("board id : {}", radio.board_id()?);
    println!("firmware : {}", radio.version()?);
    println!("usb api  : {:?}", radio.device_version());
    Ok(())
}

struct DemodArgs<'a> {
    input: &'a Path,
    sample_rate: u32,
    threshold: f64,
    min_us: u32,
    gap_us: i64,
    min_edges: usize,
    bucket_us: i64,
    burst: usize,
    out: Option<&'a Path>,
    out_all: Option<&'a Path>,
}

fn demod(args: DemodArgs<'_>) -> Result<()> {
    let mut raw = Vec::new();
    File::open(args.input)
        .with_context(|| format!("open {}", args.input.display()))?
        .read_to_end(&mut raw)?;
    ensure!(raw.len() >= ook::BYTES_PER_SAMPLE, "capture is empty");

    let magnitude = ook::envelope(&raw);
    let levels = ook::levels(&magnitude, args.threshold);
    println!(
        "{} samples ({:.2} s), noise floor {}, signal {}, threshold {}",
        magnitude.len(),
        magnitude.len() as f64 / f64::from(args.sample_rate),
        levels.noise_floor,
        levels.signal,
        levels.threshold
    );
    if levels.signal.saturating_sub(levels.noise_floor) < 10 {
        println!("warning: signal barely rises above the noise floor; the capture may be empty");
    }

    let runs = ook::runs(&magnitude, levels.threshold, args.sample_rate, args.min_us);
    let bursts = ook::split_bursts(&runs, args.gap_us, args.min_edges);
    ensure!(
        !bursts.is_empty(),
        "no bursts found — try --threshold 0.3, a smaller --min-edges, or check the capture"
    );

    println!("\n{} burst(s):", bursts.len());
    for (index, burst) in bursts.iter().enumerate() {
        println!(
            "  {:>2}: {:>4} edges, {:.2} ms",
            index + 1,
            burst.len(),
            ook::duration_us(burst) as f64 / 1000.0
        );
    }

    if let Some(agreement) = ook::compare_bursts(&bursts).filter(|_| bursts.len() > 1) {
        if agreement.same_edge_count {
            println!(
                "repeats agree: identical edge counts, max deviation {} µs{}",
                agreement.max_deviation_us,
                if agreement.max_deviation_us <= 100 {
                    " (consistent with a static frame, no rolling code)"
                } else {
                    " (frames differ — check for a rolling code or a second transmitter)"
                }
            );
        } else {
            println!("repeats differ in edge count — the bursts are not identical frames");
        }
    }

    let selected = bursts
        .get(args.burst - 1)
        .with_context(|| format!("burst {} does not exist", args.burst))?;

    println!("\nburst {} pulse widths (bucket µs -> count):", args.burst);
    for (bucket, count) in ook::width_histogram(selected, args.bucket_us) {
        println!("  {bucket:>7} : {count:>4}  {}", "#".repeat(count.min(60)));
    }

    if let Some(path) = args.out_all {
        let mut file = BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut file, &bursts)?;
        file.flush()?;
        println!("\nwrote all {} bursts to {}", bursts.len(), path.display());
    }
    if let Some(path) = args.out {
        let mut file = BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut file, selected)?;
        file.flush()?;
        println!("\nwrote burst {} ({} timings) to {}", args.burst, selected.len(), path.display());
    }
    Ok(())
}

/// `demod --out` writes one burst, `--out-all` a list of bursts; accept both.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum TimingsFile {
    Many(Vec<Vec<i64>>),
    One(Vec<i64>),
}

fn decode(paths: &[PathBuf]) -> Result<()> {
    let mut all_good = true;
    for path in paths {
        println!("=== {}", path.display());
        let json =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let bursts = match serde_json::from_str(&json)
            .with_context(|| format!("{}: not a timings JSON file", path.display()))?
        {
            TimingsFile::Many(bursts) => bursts,
            TimingsFile::One(burst) => vec![burst],
        };
        all_good &= report(&bursts);
        println!();
    }
    ensure!(all_good, "some files had no clean frames or inconsistent checksum constants");
    Ok(())
}

/// Print one file's bursts; true when at least one frame decoded cleanly and
/// the derived checksum constants agree across all clean frames.
fn report(bursts: &[Vec<i64>]) -> bool {
    let decoded: Vec<_> = bursts.iter().map(|b| proflame::decode(b)).collect();

    println!("{} frame(s)\n", bursts.len());
    print!("  frame ");
    for name in proflame::FIELD_NAMES {
        print!("{name:>11}");
    }
    println!();
    for (number, burst) in decoded.iter().enumerate() {
        print!("  {:>5} ", number + 1);
        for block in &burst.blocks {
            match block {
                Some(value) => print!("{:>11}", format!("0x{value:02x}")),
                None => print!("{:>11}", "--"),
            }
        }
        if !burst.problems.is_empty() {
            let notes: Vec<_> = burst.problems.iter().map(ToString::to_string).collect();
            print!("   <- {}", notes.join("; "));
        }
        println!();
    }

    let clean: Vec<_> = decoded.iter().filter_map(|d| Some((d.frame()?, d.keys()?))).collect();
    if clean.is_empty() {
        println!("\nno cleanly decoded frames");
        return false;
    }
    println!(
        "\n{}/{} frames decoded with parity, stop and framing intact",
        clean.len(),
        bursts.len()
    );

    let mut distinct: BTreeMap<[u8; proflame::FRAME_BLOCKS], usize> = BTreeMap::new();
    for (frame, keys) in &clean {
        *distinct.entry(frame.blocks(*keys)).or_default() += 1;
    }
    println!("{} distinct frame value(s):", distinct.len());
    let mut by_count: Vec<_> = distinct.into_iter().collect();
    by_count.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (blocks, count) in by_count {
        print!("  ");
        for (name, value) in proflame::FIELD_NAMES.iter().zip(blocks) {
            print!(" {name}=0x{value:02x}");
        }
        println!("   x{count}");
    }

    let k1: std::collections::BTreeSet<u8> = clean.iter().map(|(_, k)| k.k1).collect();
    let k2: std::collections::BTreeSet<u8> = clean.iter().map(|(_, k)| k.k2).collect();
    let show = |set: &std::collections::BTreeSet<u8>| {
        let values: Vec<_> = set.iter().map(|k| format!("0x{k:02x}")).collect();
        format!(
            "K = {}  {}",
            values.join(", "),
            if set.len() == 1 { "consistent" } else { "INCONSISTENT" }
        )
    };
    println!("\nchecksum model  cs = M(cmd) ^ K");
    println!("   half 1: {}", show(&k1));
    println!("   half 2: {}", show(&k2));
    k1.len() == 1 && k2.len() == 1
}

fn transmit(
    freq: u64,
    rate: u32,
    txvga: u16,
    amp: bool,
    repeat: u32,
    gap_us: u32,
    file: &Path,
) -> Result<()> {
    let json = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let timings: Vec<i64> = serde_json::from_str(&json)
        .context("timings must be a JSON array of signed integers (microseconds)")?;
    ensure!(!timings.is_empty(), "no timings in {}", file.display());

    let frame = ook::synthesize(&timings, rate, 100);
    let gap = ook::synthesize(&[-i64::from(gap_us)], rate, 100);

    let mut samples = Vec::with_capacity((frame.len() + gap.len()) * (repeat as usize + 1));
    for iteration in 0..=repeat {
        samples.extend_from_slice(&frame);
        if iteration < repeat {
            samples.extend_from_slice(&gap);
        }
    }
    radio::pad_to_alignment(&mut samples);

    log::info!(
        "frame: {} timings, {:.2} ms, {} repetition(s)",
        timings.len(),
        ook::duration_us(&timings) as f64 / 1000.0,
        repeat + 1
    );
    radio::transmit(
        &radio::TransmitParams {
            frequency_hz: freq,
            sample_rate: rate,
            txvga_db: txvga,
            amp_enable: amp,
        },
        &samples,
    )
}
