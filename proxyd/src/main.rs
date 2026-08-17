//! `hrf`: the daemon and the research tools that built it.
//!
//! `serve` is the daemon proper — a WebSocket radio proxy on the LAN. The rest
//! are the bench tools the protocol was solved with, and they remain useful for
//! exactly that: `capture` and `demod` are separate commands on purpose, so the
//! remote gets pressed once and the demodulator can be re-run offline with
//! different settings as often as needed.
//!
//! This binary is a thin shell. Everything worth testing lives in the library.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use hackrf_proxyd::{engine, ook, proflame, radio, server};

#[derive(Parser)]
#[command(
    name = "hrf",
    version,
    about = "A network-attached HackRF: serve it to Home Assistant, or capture, \
             demodulate, decode and replay OOK by hand"
)]
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
        /// Use the daemon's streaming detector instead of the two-pass
        /// offline path, to see what a live receiver would have made of the
        /// same signal.
        #[arg(long)]
        stream: bool,
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

    /// Run the daemon: a WebSocket radio proxy on the LAN.
    ///
    /// Receives continuously by default, publishing every burst it hears as an
    /// `rx_frame` event, and lets clients preempt with `transmit` requests.
    /// Protocol-agnostic: it moves raw OOK timings and knows nothing about any
    /// particular appliance.
    Serve {
        /// Address to listen on. Defaults to every interface, since the point
        /// is to be reachable from Home Assistant.
        #[arg(long, default_value = "0.0.0.0:8765")]
        listen: String,
        /// Frequency the receiver starts on.
        #[arg(long, default_value = "315M", value_parser = parse_frequency)]
        rx_freq: u64,
        /// Start with the receiver silent, for a transmit-only deployment.
        #[arg(long)]
        no_rx: bool,
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
        rx_amp: bool,
        /// Default TX VGA gain for requests that do not specify one. 30 dB
        /// with the amplifier off is what ignited the fireplace at close
        /// range; more is rarely useful indoors.
        #[arg(long, default_value_t = 30)]
        txvga: u16,
        /// A space at least this long ends a received burst. The default suits
        /// remotes that repeat a frame while a button is held.
        #[arg(long, default_value_t = 3_000)]
        gap_us: i64,
        /// Ignore received bursts with fewer edges than this.
        #[arg(long, default_value_t = 8)]
        min_edges: usize,
        /// Append every received frame to this file, one JSON object per line,
        /// flushed as it arrives. Read it back with `hrf decode --in`.
        ///
        /// Mapping a remote's unknown fields means pressing buttons and
        /// comparing frames, often across days; this keeps that from depending
        /// on a client staying connected.
        #[arg(long, value_name = "FILE")]
        record: Option<PathBuf>,
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
            stream,
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
            stream,
            out: out.as_deref(),
            out_all: out_all.as_deref(),
        }),
        Command::Decode { r#in } => decode(&r#in),
        Command::Serve {
            listen,
            rx_freq,
            no_rx,
            rate,
            lna,
            vga,
            rx_amp,
            txvga,
            gap_us,
            min_edges,
            record,
        } => {
            let settings =
                radio::DeviceSettings { sample_rate: rate, lna_db: lna, vga_db: vga, rx_amp };
            radio::validate_tx_gain(txvga)?;

            let mut config = engine::Config::new(rate, rx_freq);
            config.rx_enabled = !no_rx;
            config.txvga_db = txvga;
            config.detector.gap_us = gap_us;
            config.detector.min_edges = min_edges;
            config.record = record;

            serve(&listen, settings, config)
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
    stream: bool,
    out: Option<&'a Path>,
    out_all: Option<&'a Path>,
}

fn demod(args: DemodArgs<'_>) -> Result<()> {
    let mut raw = Vec::new();
    File::open(args.input)
        .with_context(|| format!("open {}", args.input.display()))?
        .read_to_end(&mut raw)?;
    ensure!(raw.len() >= ook::BYTES_PER_SAMPLE, "capture is empty");

    let bursts = if args.stream { stream_demod(&raw, &args) } else { offline_demod(&raw, &args) };
    ensure!(
        !bursts.is_empty(),
        "no bursts found — try --threshold 0.3, a smaller --min-edges, or check the capture"
    );
    report_bursts(&bursts, &args)
}

/// Two-pass: characterise the whole capture, then slice it at one threshold.
/// Only a recording can do this, and it is the more sensitive of the two.
fn offline_demod(raw: &[u8], args: &DemodArgs<'_>) -> Vec<Vec<i64>> {
    let magnitude = ook::envelope(raw);
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
    ook::split_bursts(&runs, args.gap_us, args.min_edges)
}

/// Run the capture through the daemon's own streaming detector, in chunks the
/// size of a USB transfer.
///
/// This is what the receiver would have made of the same signal, so a capture
/// that decodes offline but comes up empty here is a receiver problem worth
/// knowing about before it shows up as a missed keypress on the air.
fn stream_demod(raw: &[u8], args: &DemodArgs<'_>) -> Vec<Vec<i64>> {
    let mut config = ook::DetectorConfig::new(args.sample_rate);
    config.threshold_fraction = args.threshold;
    config.min_us = args.min_us;
    config.gap_us = args.gap_us;
    config.min_edges = args.min_edges;

    let mut detector = ook::Detector::new(config);
    let mut bursts = Vec::new();
    let mut peaks = Vec::new();
    for chunk in raw.chunks(radio::TRANSFER_SIZE) {
        for burst in detector.push(chunk) {
            peaks.push(burst.peak);
            bursts.push(burst.timings);
        }
    }

    println!(
        "{} samples ({:.2} s), streaming detector, final threshold {}",
        raw.len() / ook::BYTES_PER_SAMPLE,
        (raw.len() / ook::BYTES_PER_SAMPLE) as f64 / f64::from(args.sample_rate),
        detector.threshold().map_or("none".to_string(), |t| t.to_string())
    );
    if !peaks.is_empty() {
        println!(
            "burst peaks {}..{} of 256",
            peaks.iter().min().unwrap(),
            peaks.iter().max().unwrap()
        );
    }
    bursts
}

fn report_bursts(bursts: &[Vec<i64>], args: &DemodArgs<'_>) -> Result<()> {
    println!("\n{} burst(s):", bursts.len());
    for (index, burst) in bursts.iter().enumerate() {
        println!(
            "  {:>2}: {:>4} edges, {:.2} ms",
            index + 1,
            burst.len(),
            ook::duration_us(burst) as f64 / 1000.0
        );
    }

    if let Some(agreement) = ook::compare_bursts(bursts).filter(|_| bursts.len() > 1) {
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
        serde_json::to_writer(&mut file, bursts)?;
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

/// How many commands may queue for the radio thread.
///
/// This is the single-flight transmit queue: one transmission is in progress,
/// a few may wait, and beyond that a client is told to back off rather than
/// building an unbounded backlog of stale requests for a shared appliance.
const COMMAND_QUEUE: usize = 16;

/// Events buffered for clients that have fallen behind. A slow client loses
/// old frames; it never slows the radio down.
const EVENT_QUEUE: usize = 1024;

/// Start the radio thread and the WebSocket server, and run until signalled.
///
/// The radio thread is a plain OS thread rather than a tokio task on purpose:
/// the driver's I/O is blocking, and a transmission holds it for the best part
/// of a second. Parking a runtime worker for that long would stall unrelated
/// connections that happen to share it.
fn serve(listen: &str, settings: radio::DeviceSettings, config: engine::Config) -> Result<()> {
    let device = radio::Device::new(settings)?;

    let (commands, command_queue) = tokio::sync::mpsc::channel(COMMAND_QUEUE);
    let (events, _) = tokio::sync::broadcast::channel(EVENT_QUEUE);

    let radio_thread = {
        let events = events.clone();
        std::thread::Builder::new()
            .name("radio".into())
            .spawn(move || engine::run(device, config, command_queue, events))
            .context("failed to start the radio thread")?
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;

    let outcome = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("failed to listen on {listen}"))?;
        let server = std::sync::Arc::new(server::Server { commands, events });
        server::serve(listener, server, shutdown_signal()).await
    });

    // Dropping the runtime drops the command sender, which is what tells the
    // radio thread to stop and put the transceiver down.
    drop(runtime);
    radio_thread.join().map_err(|_| anyhow::anyhow!("the radio thread panicked"))?;
    outcome
}

/// Resolve on SIGINT or SIGTERM, so a container stops promptly and leaves the
/// transmitter off.
async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                log::warn!("cannot listen for SIGTERM: {error}");
                let _ = interrupt.await;
                return;
            }
        };
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
}

/// `demod --out` writes one burst, `--out-all` a list of bursts; accept both.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum TimingsFile {
    Many(Vec<Vec<i64>>),
    One(Vec<i64>),
}

/// One line of `serve --record`: an rx_frame event as it went out on the wire.
#[derive(serde::Deserialize)]
struct RecordedFrame {
    timings: Vec<i64>,
}

/// Read a file of bursts, whichever of the three shapes it is in.
///
/// The daemon's recording is JSON Lines rather than one array, so that it can
/// be appended to for as long as the radio runs and still be readable while it
/// does. Detecting it by the leading brace keeps `decode` a single command for
/// everything the project produces.
fn read_bursts(path: &Path) -> Result<Vec<Vec<i64>>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if text.trim_start().starts_with('{') {
        return text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<RecordedFrame>(line)
                    .map(|frame| frame.timings)
                    .with_context(|| format!("{}: not a recorded frame: {line}", path.display()))
            })
            .collect();
    }
    match serde_json::from_str(&text)
        .with_context(|| format!("{}: not a timings JSON file", path.display()))?
    {
        TimingsFile::Many(bursts) => Ok(bursts),
        TimingsFile::One(burst) => Ok(vec![burst]),
    }
}

fn decode(paths: &[PathBuf]) -> Result<()> {
    let mut all_good = true;
    for path in paths {
        println!("=== {}", path.display());
        let bursts = read_bursts(path)?;
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
    for (blocks, count) in &by_count {
        print!("  ");
        for (name, value) in proflame::FIELD_NAMES.iter().zip(blocks) {
            print!(" {name}=0x{value:02x}");
        }
        println!("   x{count}");
    }

    // The appliance state each distinct frame commands, and — the thing that
    // actually maps a button — what changed between consecutive ones.
    println!("\nappliance state:");
    let mut previous: Option<proflame::State> = None;
    let mut in_air_order: Vec<_> = by_count.iter().collect();
    in_air_order.sort_by_key(|(blocks, _)| (blocks[3], blocks[4]));
    for (blocks, count) in in_air_order {
        let state = proflame::State::from_commands(blocks[3], blocks[4]);
        print!("  {state}   x{count}");
        if let Some(previous) = previous {
            let changed = state.differences(&previous);
            if !changed.is_empty() {
                print!("   <- changed: {}", changed.join(", "));
            }
        }
        println!();
        previous = Some(state);
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

    let mut samples = ook::render_transmission(&timings, repeat, gap_us, rate, 100);
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
