//! hackrf-proxyd M1 spike CLI (`hrf`).
//!
//! Standalone research tool, not the daemon yet. It exists to answer one
//! question: can we capture the fireplace remote, understand its framing, and
//! replay it? Capture and demodulation are separate commands on purpose —
//! the remote gets pressed once, then the demodulator can be re-run offline
//! with different settings as often as needed.

mod ook;
mod radio;

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};

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

    /// Receive raw IQ to a cs8 file.
    Capture {
        #[arg(long, default_value_t = 315_000_000)]
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
    },

    /// Transmit OOK from a Flipper-RAW timings JSON array.
    Transmit {
        #[arg(long, default_value_t = 315_000_000)]
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match Cli::parse().command {
        Command::Info => info(),
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
        Command::Demod { r#in, rate, threshold, min_us, gap_us, min_edges, bucket_us, burst, out } => {
            demod(DemodArgs {
                input: &r#in,
                sample_rate: rate,
                threshold,
                min_us,
                gap_us,
                min_edges,
                bucket_us,
                burst,
                out: out.as_deref(),
            })
        }
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

    if let Some(path) = args.out {
        let mut file = BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut file, selected)?;
        file.flush()?;
        println!(
            "\nwrote burst {} ({} timings) to {}",
            args.burst,
            selected.len(),
            path.display()
        );
    }
    Ok(())
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
