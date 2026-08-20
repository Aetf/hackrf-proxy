//! HackRF device handling: configuration validation and streaming I/O.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use seify_hackrfone::{Config, HackRf};

use crate::ook;

/// USB bulk transfers must be a multiple of 512 bytes.
pub const XFER_ALIGN: usize = 512;
/// Per-transfer size; the driver keeps three of these in flight.
pub const TRANSFER_SIZE: usize = 32_768;
/// Transfers the driver keeps queued, which is also how much tail can be lost
/// when a stream is dropped.
const IN_FLIGHT_TRANSFERS: usize = 3;
/// |I| + |Q| saturates at 256; treat anything at the top as clipped.
const SATURATED: u16 = 250;

/// Open the first HackRF, distinguishing "absent" from "present but not
/// permitted".
///
/// The driver's `open_first` discards the underlying error and reports the
/// device as missing, so a permission problem — the common case in a container
/// or before the udev rule is installed — is indistinguishable from an unplugged
/// radio. Enumeration only reads sysfs and needs no permissions, so it can tell
/// the two apart and name the node that needs fixing.
pub fn open() -> Result<Arc<HackRf>> {
    match HackRf::open_first() {
        Ok(radio) => Ok(Arc::new(radio)),
        Err(err) => {
            let present = HackRf::scan().unwrap_or_default();
            let Some((bus, address)) = present.first() else {
                return Err(anyhow::Error::new(err)
                    .context("no HackRF found — check that it is plugged in"));
            };
            Err(anyhow::Error::new(err).context(format!(
                "HackRF is present at /dev/bus/usb/{bus:03}/{address:03} but could not be opened; \
                 the node needs read/write access (install deploy/60-hackrf-access.rules; the \
                 distro rule only sets uaccess, which grants nothing over SSH), \
                 and in a container it must be passed through and the process must run as root"
            )))
        }
    }
}

/// Reject gains the hardware cannot represent.
///
/// The driver silently rounds LNA down to a multiple of 8 dB and would report a
/// gain that was never applied, so catch it here instead.
pub fn validate_rx_gains(lna_db: u16, vga_db: u16) -> Result<()> {
    ensure!(
        lna_db <= 40 && lna_db.is_multiple_of(8),
        "LNA gain must be 0..=40 dB in 8 dB steps (got {lna_db})"
    );
    ensure!(
        vga_db <= 62 && vga_db.is_multiple_of(2),
        "VGA gain must be 0..=62 dB in 2 dB steps (got {vga_db})"
    );
    Ok(())
}

pub fn validate_tx_gain(txvga_db: u16) -> Result<()> {
    ensure!(txvga_db <= 47, "TX VGA gain must be 0..=47 dB (got {txvga_db})");
    Ok(())
}

/// HackRF One accepts 2–20 Msps.
pub fn validate_sample_rate(sample_rate: u32) -> Result<()> {
    ensure!(
        (2_000_000..=20_000_000).contains(&sample_rate),
        "sample rate must be 2–20 Msps (got {sample_rate})"
    );
    Ok(())
}

pub struct CaptureParams {
    pub frequency_hz: u64,
    pub sample_rate: u32,
    pub lna_db: u16,
    pub vga_db: u16,
    pub amp_enable: bool,
    pub seconds: f64,
}

/// Stream IQ to a cs8 file, reporting the peak level once per second.
///
/// The live level readout matters because a capture that was too weak or
/// clipped is only otherwise discovered at demodulation time, by which point
/// the keypress is long gone.
pub fn capture(params: &CaptureParams, out: &Path) -> Result<()> {
    validate_rx_gains(params.lna_db, params.vga_db)?;
    validate_sample_rate(params.sample_rate)?;

    let radio = open()?;
    radio.start_rx(&Config {
        lna_db: params.lna_db,
        vga_db: params.vga_db,
        txvga_db: 0,
        amp_enable: params.amp_enable,
        antenna_enable: false,
        frequency_hz: params.frequency_hz,
        sample_rate_hz: params.sample_rate,
        sample_rate_div: 1,
    })?;
    // Streaming rather than one-shot reads: at 2 Msps the device produces
    // 4 MB/s, and with no transfer in flight while we write to disk the
    // hardware FIFO overruns. Dropped samples silently shorten pulse widths,
    // which is exactly the measurement this tool exists to make.
    let mut stream = radio.start_rx_stream(TRANSFER_SIZE).context("failed to start RX stream")?;

    let total_bytes =
        (params.seconds * f64::from(params.sample_rate)) as usize * ook::BYTES_PER_SAMPLE;
    let bytes_per_second = params.sample_rate as usize * ook::BYTES_PER_SAMPLE;
    let mut file =
        BufWriter::new(File::create(out).with_context(|| format!("create {}", out.display()))?);

    log::info!(
        "capturing {:.1}s at {:.3} MHz, {} sps -> {}",
        params.seconds,
        params.frequency_hz as f64 / 1e6,
        params.sample_rate,
        out.display()
    );

    let mut written = 0usize;
    let mut interval_peak = 0u16;
    let mut interval_bytes = 0usize;
    let mut overall_peak = 0u16;
    while written < total_bytes {
        let want = (total_bytes - written).min(TRANSFER_SIZE);
        let chunk = stream.read_sync(want)?;
        file.write_all(chunk)?;

        let peak = ook::peak_magnitude(chunk);
        interval_peak = interval_peak.max(peak);
        overall_peak = overall_peak.max(peak);
        written += chunk.len();
        interval_bytes += chunk.len();

        if interval_bytes >= bytes_per_second {
            log::info!(
                "  {:>4.1}s  peak {:>3}/256{}",
                written as f64 / bytes_per_second as f64,
                interval_peak,
                if interval_peak >= SATURATED { "  (clipping — lower the gain)" } else { "" }
            );
            interval_peak = 0;
            interval_bytes = 0;
        }
    }

    drop(stream);
    file.flush()?;
    log::info!(
        "captured {} samples, overall peak {overall_peak}/256",
        written / ook::BYTES_PER_SAMPLE
    );
    if overall_peak < 20 {
        log::warn!("peak is very low — was a button pressed? try --lna 40 --vga 40 or --amp");
    }
    Ok(())
}

pub struct ScanParams {
    pub frequencies: Vec<u64>,
    pub sample_rate: u32,
    pub lna_db: u16,
    pub vga_db: u16,
    pub amp_enable: bool,
    pub dwell_seconds: f64,
    pub passes: u32,
}

/// Watch received power on several frequencies in turn, as a live meter.
///
/// A blind capture cannot distinguish "wrong frequency" from "out of range"
/// from "button missed the window", because all three produce a file of noise.
/// Sweeping while the button is held makes the answer visible as it happens.
pub fn scan(params: &ScanParams) -> Result<()> {
    validate_rx_gains(params.lna_db, params.vga_db)?;
    validate_sample_rate(params.sample_rate)?;
    ensure!(!params.frequencies.is_empty(), "no frequencies to scan");

    let radio = open()?;
    radio.start_rx(&Config {
        lna_db: params.lna_db,
        vga_db: params.vga_db,
        txvga_db: 0,
        amp_enable: params.amp_enable,
        antenna_enable: false,
        frequency_hz: params.frequencies[0],
        sample_rate_hz: params.sample_rate,
        sample_rate_div: 1,
    })?;
    let mut stream = radio.start_rx_stream(TRANSFER_SIZE)?;

    let dwell_bytes =
        (params.dwell_seconds * f64::from(params.sample_rate)) as usize * ook::BYTES_PER_SAMPLE;

    print!("pass ");
    for frequency in &params.frequencies {
        print!("{:>18}", format!("{:.3} MHz", *frequency as f64 / 1e6));
    }
    println!("\n     {}", "  peak (99.9%)   ".repeat(params.frequencies.len()));

    let mut saturated_bands = 0usize;
    for pass in 1..=params.passes {
        print!("{pass:>4} ");
        for frequency in &params.frequencies {
            radio.set_freq(*frequency)?;
            // Discard one transfer so the retune transient is not measured.
            stream.read_sync(TRANSFER_SIZE)?;

            let mut histogram = ook::AmplitudeHistogram::new();
            let mut collected = 0usize;
            while collected < dwell_bytes {
                let chunk = stream.read_sync((dwell_bytes - collected).min(TRANSFER_SIZE))?;
                histogram.add_iq(chunk);
                collected += chunk.len();
            }
            let levels = histogram.levels(0.5);
            // A band whose 99.9th percentile is already at full scale is not
            // "strong", it is overloaded: the front end is clipping almost all
            // the time and nothing can be distinguished within it.
            if levels.signal >= SATURATED {
                saturated_bands += 1;
            }
            print!("{:>18}", format!("{:>3} ({:>3})", histogram.peak(), levels.signal));
        }
        println!();
        use std::io::Write as _;
        std::io::stdout().flush().ok();
    }

    drop(stream);
    if saturated_bands > 0 {
        println!(
            "\nSaturated: the 99.9th percentile reached full scale, so the front end is\n\
             clipping and these readings cannot be compared. Drop --amp first, then lower\n\
             --lna (steps of 8) and --vga (steps of 2) until the quiet bands fall back to\n\
             double digits. Only then does a band standing out mean anything."
        );
    } else {
        println!(
            "\nA transmitting remote close to the antenna should lift one band far above\n\
             the others. If nothing moves while a button is held, that band is not the\n\
             one in use."
        );
    }
    Ok(())
}

pub struct TransmitParams {
    pub frequency_hz: u64,
    pub sample_rate: u32,
    pub txvga_db: u16,
    pub amp_enable: bool,
}

/// Transmit a prepared cs8 baseband buffer.
pub fn transmit(params: &TransmitParams, samples: &[u8]) -> Result<()> {
    validate_tx_gain(params.txvga_db)?;
    validate_sample_rate(params.sample_rate)?;
    ensure!(!samples.is_empty(), "nothing to transmit");

    let radio = open()?;
    radio.start_tx(&Config {
        lna_db: 0,
        vga_db: 0,
        txvga_db: params.txvga_db,
        amp_enable: params.amp_enable,
        antenna_enable: false,
        frequency_hz: params.frequency_hz,
        sample_rate_hz: params.sample_rate,
        sample_rate_div: 1,
    })?;
    let mut stream = radio.start_tx_stream().context("failed to start TX stream")?;

    log::info!(
        "transmitting {} samples ({:.1} ms) at {:.3} MHz, txvga {} dB, amp {}",
        samples.len() / ook::BYTES_PER_SAMPLE,
        (samples.len() / ook::BYTES_PER_SAMPLE) as f64 / f64::from(params.sample_rate) * 1e3,
        params.frequency_hz as f64 / 1e6,
        params.txvga_db,
        if params.amp_enable { "on" } else { "off" }
    );

    for chunk in samples.chunks(TRANSFER_SIZE) {
        stream.write_sync(chunk)?;
    }
    // Dropping the stream switches the transceiver off, and write_sync only
    // waits on completion once the queue is full, so the last few transfers can
    // still be pending. Push silence through afterwards so anything truncated
    // is silence rather than the tail of the frame.
    let flush = vec![0u8; TRANSFER_SIZE];
    for _ in 0..IN_FLIGHT_TRANSFERS + 1 {
        stream.write_sync(&flush)?;
    }

    drop(stream);
    log::info!("transmit complete");
    Ok(())
}

/// USB ids the HackRF family enumerates as.
const HACKRF_VENDOR_ID: u16 = 0x1d50;
const HACKRF_PRODUCT_IDS: [u16; 3] = [
    0x6089, // HackRF One
    0x604b, // Jawbreaker
    0xcc15, // rad1o
];

/// Yield once for every HackRF that appears on the USB bus.
///
/// This is what lets a replugged radio be picked up in the moment rather than
/// on the next retry. It does **not** replace the arbiter's backoff, because
/// the two cover different failures: a device that vanishes and returns raises
/// a hotplug event, while a device that stays enumerated but stops streaming —
/// which is what USB autosuspend produces, and what cost us three and a half
/// hours of a two-second retry loop — never raises one at all.
///
/// nusb's Linux implementation is a raw `AF_NETLINK`/`KOBJECT_UEVENT` socket
/// that parses udev's message format itself, so this keeps the project's "no C
/// dependencies" property: no libudev, and nusb is already underneath the
/// driver we use.
pub fn watch_for_arrivals() -> Result<impl futures_util::Stream<Item = ()>> {
    use futures_util::StreamExt as _;

    let watch = nusb::watch_devices().context("failed to watch USB for hotplug events")?;
    Ok(watch.filter_map(|event| async move {
        match event {
            nusb::hotplug::HotplugEvent::Connected(device)
                if device.vendor_id() == HACKRF_VENDOR_ID
                    && HACKRF_PRODUCT_IDS.contains(&device.product_id()) =>
            {
                Some(())
            }
            _ => None,
        }
    }))
}

/// Gains and rate the daemon drives the radio with.
#[derive(Debug, Clone, Copy)]
pub struct DeviceSettings {
    pub sample_rate: u32,
    pub lna_db: u16,
    pub vga_db: u16,
    pub rx_amp: bool,
}

/// A real HackRF behind the arbiter's [`Transceiver`](crate::engine::Transceiver)
/// trait.
///
/// The handle is opened lazily and dropped on failure, because the failure
/// that matters here is a re-enumeration: after a USB reset the old handle is
/// permanently dead and only a fresh open recovers. Holding on to it would
/// turn a recoverable glitch into a daemon that never receives again.
pub struct Device {
    settings: DeviceSettings,
    radio: Option<Arc<HackRf>>,
    rx: Option<seify_hackrfone::RxStream>,
}

impl Device {
    pub fn new(settings: DeviceSettings) -> Result<Self> {
        validate_rx_gains(settings.lna_db, settings.vga_db)?;
        validate_sample_rate(settings.sample_rate)?;
        Ok(Self { settings, radio: None, rx: None })
    }

    fn handle(&mut self) -> Result<Arc<HackRf>> {
        if let Some(radio) = &self.radio {
            return Ok(Arc::clone(radio));
        }
        let radio = open()?;
        self.radio = Some(Arc::clone(&radio));
        Ok(radio)
    }

    /// Forget the handle so the next attempt opens the device afresh.
    fn invalidate(&mut self) {
        self.rx = None;
        self.radio = None;
    }

    fn config(&self, frequency_hz: u64, txvga_db: u16, amp_enable: bool) -> Config {
        Config {
            lna_db: self.settings.lna_db,
            vga_db: self.settings.vga_db,
            txvga_db,
            amp_enable,
            antenna_enable: false,
            frequency_hz,
            sample_rate_hz: self.settings.sample_rate,
            sample_rate_div: 1,
        }
    }
}

impl crate::engine::Transceiver for Device {
    fn start_rx(&mut self, frequency_hz: u64) -> Result<()> {
        let radio = self.handle()?;
        let result = (|| {
            radio.start_rx(&self.config(frequency_hz, 0, self.settings.rx_amp))?;
            radio.start_rx_stream(TRANSFER_SIZE).context("failed to start RX stream")
        })();
        match result {
            Ok(stream) => {
                self.rx = Some(stream);
                Ok(())
            }
            Err(error) => {
                self.invalidate();
                Err(error)
            }
        }
    }

    fn read(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let Some(stream) = self.rx.as_mut() else {
            anyhow::bail!("read called with no receiver running");
        };
        match stream.read_sync(TRANSFER_SIZE) {
            Ok(chunk) => {
                out.clear();
                out.extend_from_slice(chunk);
                Ok(())
            }
            Err(error) => {
                self.invalidate();
                Err(anyhow::Error::new(error).context("RX stream failed"))
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.rx = None;
        let Some(radio) = &self.radio else {
            return Ok(());
        };
        if let Err(error) = radio.stop() {
            self.invalidate();
            return Err(anyhow::Error::new(error).context("failed to stop the radio"));
        }
        Ok(())
    }

    fn transmit(&mut self, params: &crate::engine::TxParams, samples: &[u8]) -> Result<()> {
        validate_tx_gain(params.txvga_db)?;
        let radio = self.handle()?;

        let result = (|| -> Result<()> {
            radio.start_tx(&self.config(
                params.frequency_hz,
                params.txvga_db,
                params.amp_enable,
            ))?;
            let mut stream = radio.start_tx_stream().context("failed to start TX stream")?;
            for chunk in samples.chunks(TRANSFER_SIZE) {
                stream.write_sync(chunk)?;
            }
            // write_sync only waits on completion once the queue is full, so
            // the last transfers can still be pending when the stream drops
            // and switches the transceiver off. Push silence through, so
            // anything truncated is silence rather than the tail of a frame.
            let flush = vec![0u8; TRANSFER_SIZE];
            for _ in 0..IN_FLIGHT_TRANSFERS + 1 {
                stream.write_sync(&flush)?;
            }
            Ok(())
        })();

        // The transmitter must never be left running, whatever happened.
        if let Err(error) = radio.stop() {
            self.invalidate();
            return Err(anyhow::Error::new(error).context("failed to stop the transmitter"));
        }
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn describe(&mut self) -> Result<String> {
        let radio = self.handle()?;
        match (radio.board_id(), radio.version()) {
            (Ok(board), Ok(firmware)) => Ok(format!("board {board}, firmware {firmware}")),
            _ => {
                self.invalidate();
                anyhow::bail!("HackRF did not answer an identification request")
            }
        }
    }
}

/// Pad a baseband buffer so every USB transfer is 512-byte aligned.
pub fn pad_to_alignment(samples: &mut Vec<u8>) {
    let remainder = samples.len() % XFER_ALIGN;
    if remainder != 0 {
        samples.resize(samples.len() + (XFER_ALIGN - remainder), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_gain_validation_matches_hardware_steps() {
        assert!(validate_rx_gains(40, 62).is_ok());
        assert!(validate_rx_gains(0, 0).is_ok());
        // Silently rounded down to 8 dB by the driver, so reject it outright.
        assert!(validate_rx_gains(12, 20).is_err());
        assert!(validate_rx_gains(48, 20).is_err());
        assert!(validate_rx_gains(8, 21).is_err());
    }

    #[test]
    fn tx_gain_and_sample_rate_bounds() {
        assert!(validate_tx_gain(47).is_ok());
        assert!(validate_tx_gain(48).is_err());
        assert!(validate_sample_rate(2_000_000).is_ok());
        assert!(validate_sample_rate(1_999_999).is_err());
        assert!(validate_sample_rate(20_000_001).is_err());
    }

    #[test]
    fn padding_rounds_up_to_a_transfer_boundary() {
        let mut samples = vec![1u8; XFER_ALIGN + 1];
        pad_to_alignment(&mut samples);
        assert_eq!(samples.len(), XFER_ALIGN * 2);
        assert_eq!(samples[XFER_ALIGN], 1);
        assert_eq!(samples[XFER_ALIGN + 1], 0);

        let mut aligned = vec![1u8; XFER_ALIGN];
        pad_to_alignment(&mut aligned);
        assert_eq!(aligned.len(), XFER_ALIGN);
    }
}
