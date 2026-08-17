//! The half-duplex arbiter: one thread, one radio, one owner.
//!
//! A HackRF is a single-tuner, half-duplex device, and the driver's I/O is
//! blocking. Both facts point at the same shape: a dedicated OS thread owns
//! the radio and nothing else touches it. Arbitration then needs no locks at
//! all, because it *is* this thread's control flow — receive by default,
//! preempt for a transmission, return to receiving.
//!
//! Keeping that thread off the async runtime is the other half of the point.
//! Blocking a tokio worker for the second a transmission takes would stall
//! every other connection on that worker.
//!
//! The radio hides behind [`Transceiver`] so the state machine can be tested
//! against a fake device, in keeping with the rest of the crate: the logic is
//! hardware-free and tested, and only the implementation of this one trait
//! needs a HackRF to exercise.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{ook, wire};

/// Settings for a transmission, as the arbiter hands them to the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxParams {
    pub frequency_hz: u64,
    pub sample_rate: u32,
    pub txvga_db: u16,
    pub amp_enable: bool,
}

/// The radio, as the arbiter needs it.
///
/// Implementations must leave the device stopped after [`Self::transmit`]:
/// the arbiter restarts the receiver itself, and a device left transmitting
/// is a device jamming the band.
pub trait Transceiver {
    /// Begin receiving at this frequency. Called again to retune.
    fn start_rx(&mut self, frequency_hz: u64) -> Result<()>;
    /// Block until the next transfer of interleaved cs8 arrives, replacing
    /// the contents of `out`.
    ///
    /// The caller supplies the buffer, and reuses it, so that a failed read
    /// can invalidate the device without the borrow checker having to
    /// reconcile that against a slice handed back out of it.
    fn read(&mut self, out: &mut Vec<u8>) -> Result<()>;
    /// Stop receiving or transmitting; idempotent.
    fn stop(&mut self) -> Result<()>;
    /// Send a prepared baseband buffer, returning once it is off the antenna.
    fn transmit(&mut self, params: &TxParams, samples: &[u8]) -> Result<()>;
    /// Board id and firmware, for diagnostics.
    fn describe(&mut self) -> Result<String>;
}

/// What a client can ask the radio thread to do.
///
/// Every variant that can fail carries its own reply channel, so a caller
/// learns the outcome of *its* request rather than inferring it from state.
pub enum Command {
    Transmit {
        request: wire::Transmit,
        /// The air time actually sent, in microseconds.
        reply: oneshot::Sender<Result<i64>>,
    },
    ConfigureRx {
        frequency: Option<u64>,
        enabled: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Status {
        reply: oneshot::Sender<Box<wire::Status>>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub sample_rate: u32,
    pub rx_frequency: u64,
    pub rx_enabled: bool,
    pub txvga_db: u16,
    /// Baseband amplitude for a mark. Full scale would clip the DAC.
    pub tx_amplitude: i8,
    pub detector: ook::DetectorConfig,
}

impl Config {
    pub fn new(sample_rate: u32, rx_frequency: u64) -> Self {
        Self {
            sample_rate,
            rx_frequency,
            rx_enabled: true,
            txvga_db: 30,
            tx_amplitude: 100,
            detector: ook::DetectorConfig::new(sample_rate),
        }
    }
}

/// How long to wait before reopening a radio that failed.
const FAULT_BACKOFF: Duration = Duration::from_secs(2);
/// How long the thread parks while faulted before looking at its inbox again.
/// Short enough that a client's request is not noticeably delayed.
const FAULT_POLL: Duration = Duration::from_millis(100);

/// Run the arbiter until the command channel closes.
///
/// This blocks, and is meant to be the whole body of a dedicated thread.
pub fn run<T: Transceiver>(
    mut device: T,
    config: Config,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<wire::Message>,
) {
    let mut state = State::new(config, &mut device);
    let mut detector = ook::Detector::new(config.detector);
    // Reused across every transfer, so a receiving daemon does no allocation
    // in its steady state.
    let mut buffer = Vec::new();

    loop {
        // With the receiver wanted, look for work between transfers rather
        // than waiting for it: that is what lets a transmission preempt, and
        // it bounds the preemption delay at one transfer. With the receiver
        // off there is nothing to do until a request arrives, faulted or not,
        // so block rather than spin on a radio nobody is asking for.
        let command = if state.rx_enabled {
            match commands.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::error::TryRecvError::Empty) => None,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        } else {
            match commands.blocking_recv() {
                Some(command) => Some(command),
                None => break,
            }
        };

        if let Some(command) = command {
            state.handle(command, &mut device, &mut detector, &events);
        }

        if state.faulted {
            if state.rx_enabled {
                state.retry(&mut device, &events);
            }
            continue;
        }
        if state.receiving() {
            state.pump(&mut device, &mut detector, &mut buffer, &events);
        }
    }

    let _ = device.stop();
}

struct State {
    config: Config,
    rx_enabled: bool,
    rx_frequency: u64,
    rx_running: bool,
    faulted: bool,
    retry_at: Instant,
    device: Option<String>,
    counters: wire::Counters,
    published: Option<wire::DeviceState>,
}

impl State {
    fn new<T: Transceiver>(config: Config, device: &mut T) -> Self {
        Self {
            rx_enabled: config.rx_enabled,
            rx_frequency: config.rx_frequency,
            rx_running: false,
            faulted: false,
            retry_at: Instant::now(),
            device: device.describe().ok(),
            counters: wire::Counters::default(),
            published: None,
            config,
        }
    }

    fn receiving(&self) -> bool {
        self.rx_enabled && !self.faulted
    }

    fn device_state(&self) -> wire::DeviceState {
        if self.faulted {
            wire::DeviceState::Faulted
        } else if self.rx_running {
            wire::DeviceState::Receiving
        } else {
            wire::DeviceState::Idle
        }
    }

    /// Announce the current state, but only when it has actually changed —
    /// a client tracking availability should not have to filter a firehose.
    fn publish(&mut self, events: &broadcast::Sender<wire::Message>) {
        let state = self.device_state();
        if self.published != Some(state) {
            self.published = Some(state);
            let _ = events.send(wire::Message::event(wire::MessagePayload::DeviceState { state }));
        }
    }

    fn announce(&mut self, state: wire::DeviceState, events: &broadcast::Sender<wire::Message>) {
        self.published = Some(state);
        let _ = events.send(wire::Message::event(wire::MessagePayload::DeviceState { state }));
    }

    fn fault(&mut self, error: &anyhow::Error, events: &broadcast::Sender<wire::Message>) {
        log::error!("radio fault: {error:#}");
        self.faulted = true;
        self.rx_running = false;
        self.counters.device_faults += 1;
        self.retry_at = Instant::now() + FAULT_BACKOFF;
        self.publish(events);
    }

    fn handle<T: Transceiver>(
        &mut self,
        command: Command,
        device: &mut T,
        detector: &mut ook::Detector,
        events: &broadcast::Sender<wire::Message>,
    ) {
        match command {
            Command::Transmit { request, reply } => {
                let outcome = self.transmit(&request, device, events);
                if let Err(error) = &outcome {
                    self.fault(error, events);
                }
                let _ = reply.send(outcome);
            }
            Command::ConfigureRx { frequency, enabled, reply } => {
                if let Some(frequency) = frequency {
                    self.rx_frequency = frequency;
                }
                self.rx_enabled = enabled;
                // Retuning means restarting the receiver, and a stale
                // detector would carry the old band's threshold across.
                if self.rx_running {
                    let _ = device.stop();
                    self.rx_running = false;
                }
                *detector = ook::Detector::new(self.config.detector);
                // A retune is also a chance to recover: clear the fault rather
                // than making a client wait out the backoff it happened to
                // land in.
                self.faulted = false;

                // Bring the receiver up now, not on the next loop iteration,
                // so that the reply says whether it actually came up. Deferring
                // it would have "enable the receiver" answer with a cheerful
                // "idle" and only then fault, which tells the client nothing it
                // can act on.
                let outcome = if self.rx_enabled {
                    match device.start_rx(self.rx_frequency) {
                        Ok(()) => {
                            self.rx_running = true;
                            Ok(())
                        }
                        Err(error) => {
                            self.fault(&error, events);
                            Err(error)
                        }
                    }
                } else {
                    Ok(())
                };
                self.publish(events);
                let _ = reply.send(outcome);
            }
            Command::Status { reply } => {
                let _ = reply.send(Box::new(wire::Status {
                    daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: wire::PROTOCOL_VERSION,
                    state: self.device_state(),
                    device: self.device.clone(),
                    rx: wire::RxStatus {
                        enabled: self.rx_enabled,
                        frequency: self.rx_frequency,
                        sample_rate: self.config.sample_rate,
                        threshold: detector.threshold(),
                    },
                    counters: wire::Counters {
                        burst_overflows: detector.overflows(),
                        ..self.counters
                    },
                }));
            }
        }
    }

    /// Preempt the receiver, send, and hand the radio back to it.
    fn transmit<T: Transceiver>(
        &mut self,
        request: &wire::Transmit,
        device: &mut T,
        events: &broadcast::Sender<wire::Message>,
    ) -> Result<i64> {
        if self.rx_running {
            device.stop()?;
            self.rx_running = false;
        }
        self.announce(wire::DeviceState::Transmitting, events);

        let mut samples = ook::render_transmission(
            &request.timings,
            request.repeat,
            request.gap_us,
            self.config.sample_rate,
            self.config.tx_amplitude,
        );
        crate::radio::pad_to_alignment(&mut samples);

        let params = TxParams {
            frequency_hz: request.frequency,
            sample_rate: self.config.sample_rate,
            txvga_db: request.txvga_db.unwrap_or(self.config.txvga_db),
            amp_enable: request.amp,
        };
        let result = device.transmit(&params, &samples);

        // Whatever happened, the transmitter must not be left running.
        let _ = device.stop();
        result?;

        self.counters.transmissions += 1;
        // With the receiver wanted, say nothing here: the next loop iteration
        // restarts it and announces "receiving". Publishing the momentary
        // "idle" in between would have a client tracking availability flap
        // once per transmission.
        if !self.rx_enabled {
            self.publish(events);
        }
        Ok(request.air_time_us())
    }

    /// One transfer's worth of receiving.
    fn pump<T: Transceiver>(
        &mut self,
        device: &mut T,
        detector: &mut ook::Detector,
        buffer: &mut Vec<u8>,
        events: &broadcast::Sender<wire::Message>,
    ) {
        if !self.rx_running {
            if let Err(error) = device.start_rx(self.rx_frequency) {
                self.fault(&error, events);
                return;
            }
            self.rx_running = true;
            self.publish(events);
        }

        if let Err(error) = device.read(buffer) {
            self.fault(&error, events);
            return;
        }

        for burst in detector.push(buffer) {
            self.counters.rx_frames += 1;
            // Debug rather than info: on 315 MHz a house hears car remotes,
            // weather stations and doorbells, and none of that is worth a line
            // in the journal by default.
            log::debug!(
                "burst: {} edges, {:.2} ms, peak {}/256, threshold {:?}",
                burst.timings.len(),
                ook::duration_us(&burst.timings) as f64 / 1000.0,
                burst.peak,
                detector.threshold()
            );
            let _ =
                events.send(wire::Message::event(wire::MessagePayload::RxFrame(wire::RxFrame {
                    frequency: self.rx_frequency,
                    timings: burst.timings,
                    rssi: burst.peak,
                    timestamp_ms: now_ms(),
                })));
        }
    }

    /// Try to bring a failed radio back, without spinning on it.
    fn retry<T: Transceiver>(&mut self, device: &mut T, events: &broadcast::Sender<wire::Message>) {
        if Instant::now() < self.retry_at {
            std::thread::sleep(FAULT_POLL);
            return;
        }
        let _ = device.stop();
        match device.describe() {
            Ok(description) => {
                log::info!("radio recovered: {description}");
                self.device = Some(description);
                self.faulted = false;
                self.publish(events);
            }
            Err(error) => {
                log::debug!("radio still unavailable: {error:#}");
                self.retry_at = Instant::now() + FAULT_BACKOFF;
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// What a fake device was asked to do, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Action {
        StartRx(u64),
        Read,
        Stop,
        Transmit { params: TxParams, samples: usize },
    }

    #[derive(Default)]
    struct Shared {
        actions: Vec<Action>,
        /// Fed to the arbiter one transfer at a time.
        rx_data: Vec<Vec<u8>>,
        fail_reads: usize,
        fail_start_rx: usize,
    }

    #[derive(Clone, Default)]
    struct FakeRadio {
        shared: Arc<Mutex<Shared>>,
    }

    impl FakeRadio {
        fn actions(&self) -> Vec<Action> {
            self.shared.lock().unwrap().actions.clone()
        }
    }

    impl Transceiver for FakeRadio {
        fn start_rx(&mut self, frequency_hz: u64) -> Result<()> {
            let mut shared = self.shared.lock().unwrap();
            shared.actions.push(Action::StartRx(frequency_hz));
            if shared.fail_start_rx > 0 {
                shared.fail_start_rx -= 1;
                return Err(anyhow::anyhow!("radio is not there"));
            }
            Ok(())
        }

        fn read(&mut self, out: &mut Vec<u8>) -> Result<()> {
            let mut shared = self.shared.lock().unwrap();
            shared.actions.push(Action::Read);
            if shared.fail_reads > 0 {
                shared.fail_reads -= 1;
                return Err(anyhow::anyhow!("USB went away"));
            }
            out.clear();
            if shared.rx_data.is_empty() {
                out.resize(4096, 0); // silence
            } else {
                out.extend_from_slice(&shared.rx_data.remove(0));
            }
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            self.shared.lock().unwrap().actions.push(Action::Stop);
            Ok(())
        }

        fn transmit(&mut self, params: &TxParams, samples: &[u8]) -> Result<()> {
            self.shared
                .lock()
                .unwrap()
                .actions
                .push(Action::Transmit { params: *params, samples: samples.len() });
            Ok(())
        }

        fn describe(&mut self) -> Result<String> {
            Ok("fake HackRF".into())
        }
    }

    struct Harness {
        radio: FakeRadio,
        commands: mpsc::Sender<Command>,
        events: broadcast::Receiver<wire::Message>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn start(config: Config, radio: FakeRadio) -> Self {
            let (tx, rx) = mpsc::channel(8);
            let (events, subscription) = broadcast::channel(64);
            let thread = {
                let radio = radio.clone();
                std::thread::spawn(move || run(radio, config, rx, events))
            };
            Self { radio, commands: tx, events: subscription, thread: Some(thread) }
        }

        fn send(&self, command: Command) {
            self.commands.blocking_send(command).expect("arbiter should be running");
        }

        fn transmit(&self, request: wire::Transmit) -> Result<i64> {
            let (reply, response) = oneshot::channel();
            self.send(Command::Transmit { request, reply });
            response.blocking_recv().expect("arbiter should reply")
        }

        fn configure_rx(&self, frequency: Option<u64>, enabled: bool) {
            let (reply, response) = oneshot::channel();
            self.send(Command::ConfigureRx { frequency, enabled, reply });
            response.blocking_recv().expect("arbiter should reply").expect("should succeed");
        }

        fn status(&self) -> wire::Status {
            let (reply, response) = oneshot::channel();
            self.send(Command::Status { reply });
            *response.blocking_recv().expect("arbiter should reply")
        }

        /// Wait for an event matching a predicate, or give up.
        fn wait_for<R>(&mut self, mut f: impl FnMut(&wire::Message) -> Option<R>) -> Option<R> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match self.events.try_recv() {
                    Ok(message) => {
                        if let Some(found) = f(&message) {
                            return Some(found);
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return None,
                }
            }
            None
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // Closing the command channel is what stops the arbiter.
            let (idle, _) = mpsc::channel(1);
            let commands = std::mem::replace(&mut self.commands, idle);
            drop(commands);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("arbiter should exit cleanly");
            }
        }
    }

    fn config() -> Config {
        let mut config = Config::new(2_000_000, 315_000_000);
        // A window short enough that tests do not have to synthesize a second
        // of signal, but still long enough to contain a whole test burst so
        // the threshold does not move underneath one.
        config.detector.window_samples = 20_000;
        config.detector.min_edges = 3;
        config
    }

    fn a_frame() -> wire::Transmit {
        wire::Transmit {
            frequency: 315_000_000,
            timings: vec![500, -500, 500],
            repeat: 1,
            gap_us: 4_150,
            txvga_db: None,
            amp: false,
        }
    }

    #[test]
    fn it_receives_by_default() {
        let harness = Harness::start(config(), FakeRadio::default());
        std::thread::sleep(Duration::from_millis(50));

        let actions = harness.radio.actions();
        assert_eq!(actions.first(), Some(&Action::StartRx(315_000_000)));
        assert!(actions.contains(&Action::Read), "it should be pumping the receiver");
    }

    #[test]
    fn a_transmission_preempts_the_receiver_and_hands_it_back() {
        let harness = Harness::start(config(), FakeRadio::default());
        std::thread::sleep(Duration::from_millis(50));

        let air_time = harness.transmit(a_frame()).expect("should transmit");
        assert_eq!(air_time, 1500 * 2 + 4_150);
        std::thread::sleep(Duration::from_millis(50));

        let actions = harness.radio.actions();
        let sent = actions.iter().position(|a| matches!(a, Action::Transmit { .. })).unwrap();
        assert_eq!(
            actions[sent - 1],
            Action::Stop,
            "the receiver must be stopped before transmitting"
        );
        assert!(
            actions[sent..].contains(&Action::StartRx(315_000_000)),
            "the receiver must come back afterwards: {actions:?}"
        );
    }

    #[test]
    fn transmit_parameters_come_from_the_request_then_the_config() {
        let mut config = config();
        config.txvga_db = 22;
        let harness = Harness::start(config, FakeRadio::default());

        harness.transmit(a_frame()).unwrap();
        harness.transmit(wire::Transmit { txvga_db: Some(40), amp: true, ..a_frame() }).unwrap();

        let sent: Vec<_> = harness
            .radio
            .actions()
            .into_iter()
            .filter_map(|a| match a {
                Action::Transmit { params, .. } => Some(params),
                _ => None,
            })
            .collect();
        assert_eq!(sent[0].txvga_db, 22, "unset gain falls back to the daemon's");
        assert!(!sent[0].amp_enable, "the amplifier stays off unless asked for");
        assert_eq!(sent[1].txvga_db, 40);
        assert!(sent[1].amp_enable);
    }

    #[test]
    fn disabling_the_receiver_stops_it_and_parks_the_thread() {
        let harness = Harness::start(config(), FakeRadio::default());
        std::thread::sleep(Duration::from_millis(50));

        harness.configure_rx(None, false);
        let quiesced = harness.radio.actions().len();
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(
            harness.radio.actions().len(),
            quiesced,
            "a disabled receiver must not keep touching the radio"
        );
        assert!(!harness.status().rx.enabled);

        // And it must still answer, rather than being wedged on a blocking read.
        harness.transmit(a_frame()).expect("a parked arbiter still transmits");
    }

    #[test]
    fn retuning_restarts_the_receiver_on_the_new_frequency() {
        let harness = Harness::start(config(), FakeRadio::default());
        std::thread::sleep(Duration::from_millis(50));

        harness.configure_rx(Some(433_920_000), true);

        // Already receiving by the time the reply lands, rather than a moment
        // later: a client that enables the receiver and immediately asks for
        // status should not be told "idle".
        assert!(harness.radio.actions().contains(&Action::StartRx(433_920_000)));
        let status = harness.status();
        assert_eq!(status.rx.frequency, 433_920_000);
        assert_eq!(status.state, wire::DeviceState::Receiving);
    }

    #[test]
    fn enabling_a_receiver_that_cannot_start_reports_the_failure() {
        let radio = FakeRadio::default();
        radio.shared.lock().unwrap().fail_start_rx = 1;
        let harness = Harness::start(config(), radio);

        let (reply, response) = oneshot::channel();
        harness.send(Command::ConfigureRx { frequency: None, enabled: true, reply });

        assert!(
            response.blocking_recv().unwrap().is_err(),
            "the reply must carry the failure, not leave the client to infer it"
        );
    }

    #[test]
    fn a_received_burst_is_published_as_an_event() {
        let radio = FakeRadio::default();
        {
            let mut shared = radio.shared.lock().unwrap();
            // One window to learn the noise floor, then a burst, then silence
            // long enough to close it.
            shared.rx_data.push(vec![0u8; 20_000 * ook::BYTES_PER_SAMPLE]);
            shared.rx_data.push(ook::synthesize(&[500, -500, 500, -500, 500], 2_000_000, 100));
            shared.rx_data.push(ook::synthesize(&[-20_000], 2_000_000, 100));
        }
        let mut harness = Harness::start(config(), radio);

        let frame = harness
            .wait_for(|message| match &message.payload {
                wire::MessagePayload::RxFrame(frame) => Some(frame.clone()),
                _ => None,
            })
            .expect("the burst should be published");

        assert_eq!(frame.timings, vec![500, -500, 500, -500, 500]);
        assert_eq!(frame.frequency, 315_000_000);
        assert_eq!(frame.rssi, 100);
        assert_eq!(harness.status().counters.rx_frames, 1);
    }

    #[test]
    fn state_changes_are_announced_but_not_repeated() {
        let mut harness = Harness::start(config(), FakeRadio::default());

        assert_eq!(
            harness.wait_for(|m| match &m.payload {
                wire::MessagePayload::DeviceState { state } => Some(*state),
                _ => None,
            }),
            Some(wire::DeviceState::Receiving)
        );

        std::thread::sleep(Duration::from_millis(50));
        let repeats = std::iter::from_fn(|| harness.events.try_recv().ok())
            .filter(|m| matches!(m.payload, wire::MessagePayload::DeviceState { .. }))
            .count();
        assert_eq!(repeats, 0, "steady state must not be re-announced every transfer");
    }

    #[test]
    fn a_transmission_does_not_flap_availability_through_idle() {
        // A client tracking availability off device_state should see the radio
        // go busy and come back, not blink through "idle" every time.
        let mut harness = Harness::start(config(), FakeRadio::default());
        assert_eq!(
            harness.wait_for(|m| match &m.payload {
                wire::MessagePayload::DeviceState { state } => Some(*state),
                _ => None,
            }),
            Some(wire::DeviceState::Receiving)
        );

        harness.transmit(a_frame()).unwrap();

        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !seen.ends_with(&[wire::DeviceState::Receiving]) {
            if let Ok(message) = harness.events.try_recv() {
                if let wire::MessagePayload::DeviceState { state } = message.payload {
                    seen.push(state);
                }
            }
        }

        assert_eq!(
            seen,
            vec![wire::DeviceState::Transmitting, wire::DeviceState::Receiving],
            "expected busy then back, with no idle in between"
        );
    }

    #[test]
    fn a_failing_radio_faults_and_recovers_rather_than_killing_the_daemon() {
        let radio = FakeRadio::default();
        radio.shared.lock().unwrap().fail_reads = 1;
        let mut harness = Harness::start(config(), radio);

        assert_eq!(
            harness.wait_for(|m| match &m.payload {
                wire::MessagePayload::DeviceState { state: wire::DeviceState::Faulted } => Some(()),
                _ => None,
            }),
            Some(()),
            "a read failure should be announced, not fatal"
        );
        assert_eq!(harness.status().counters.device_faults, 1);

        // The backoff is two seconds; the daemon must still be answering.
        assert!(harness.status().counters.rx_frames == 0);
        assert_eq!(
            harness.wait_for(|m| match &m.payload {
                wire::MessagePayload::DeviceState { state: wire::DeviceState::Receiving } =>
                    Some(()),
                _ => None,
            }),
            Some(()),
            "it should come back on its own"
        );
    }
}
