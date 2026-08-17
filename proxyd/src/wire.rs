//! The daemon's WebSocket protocol: JSON in, JSON out.
//!
//! Deliberately protocol-agnostic. Everything here moves raw OOK timings and
//! knows nothing about Proflame or fireplaces, which is what makes the daemon
//! a shared radio proxy rather than one appliance's bridge.
//!
//! Every message carries a version, from day one, so a client that predates a
//! change can be told so rather than silently misreading it. The shape of
//! [`RxFrame`] deliberately follows the receiver-event sketch in the Home
//! Assistant architecture discussion, so that if an upstream receiver platform
//! lands, consumers migrate with little churn.
//!
//! These are pure types: parsing and validation happen here and are tested
//! here; nothing in this module touches a radio.

use serde::{Deserialize, Serialize};

/// Bumped only for incompatible changes. Additive fields do not bump it.
pub const PROTOCOL_VERSION: u32 = 1;

fn default_version() -> u32 {
    PROTOCOL_VERSION
}

/// A request from a client, with its envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default = "default_version")]
    pub v: u32,
    /// Echoed back on the reply, so a client can match them up. Optional
    /// because `websocat` users should not have to invent one.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(flatten)]
    pub payload: RequestPayload,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestPayload {
    /// Send OOK timings. The reply comes after the transmission has finished,
    /// not when it is queued, so a client knows the air time is over.
    Transmit(Transmit),
    /// Retune or silence the receiver.
    ConfigureRx(ConfigureRx),
    /// Device, receiver and counter state.
    Status,
}

/// A transmit request: Flipper-RAW timings, positive for carrier on.
#[derive(Debug, Clone, Deserialize)]
pub struct Transmit {
    pub frequency: u64,
    pub timings: Vec<i64>,
    /// Extra repetitions; 0 sends the frame once.
    #[serde(default)]
    pub repeat: u32,
    /// Silence between repetitions.
    #[serde(default = "default_gap_us")]
    pub gap_us: u32,
    /// TX VGA gain in dB, 0..=47. Defaults to the daemon's configured gain.
    #[serde(default)]
    pub txvga_db: Option<u16>,
    /// The +14 dB power amplifier. Off unless asked for: at close range it
    /// buys nothing and splatters.
    #[serde(default)]
    pub amp: bool,
}

fn default_gap_us() -> u32 {
    10_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigureRx {
    /// Leave unset to keep the current frequency.
    #[serde(default)]
    pub frequency: Option<u64>,
    pub enabled: bool,
}

/// Anything the daemon sends: replies and unsolicited events alike.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub v: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub payload: MessagePayload,
}

impl Message {
    /// A reply to the request with this id.
    pub fn reply(id: Option<String>, payload: MessagePayload) -> Self {
        Self { v: PROTOCOL_VERSION, id, payload }
    }

    /// An unsolicited event, belonging to no request.
    pub fn event(payload: MessagePayload) -> Self {
        Self { v: PROTOCOL_VERSION, id: None, payload }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePayload {
    /// A transmission completed, with the air time it actually took.
    Transmitted {
        duration_us: i64,
    },
    Error {
        message: String,
    },
    Status(Box<Status>),
    RxFrame(RxFrame),
    DeviceState {
        state: DeviceState,
    },
}

/// A burst the receiver heard.
#[derive(Debug, Clone, Serialize)]
pub struct RxFrame {
    pub frequency: u64,
    /// Flipper-RAW timings, positive for carrier on.
    pub timings: Vec<i64>,
    /// Peak L1 magnitude during the burst, 0..=256. **Not dBm**: this is
    /// uncalibrated ADC amplitude and moves with the configured gains. It is
    /// useful for comparing bursts to each other, nothing more.
    pub rssi: u16,
    /// Milliseconds since the Unix epoch, taken when the burst completed.
    pub timestamp_ms: u64,
}

/// What the radio is doing. Pushed on every change, so a client never has to
/// poll to keep an availability indicator honest.
///
/// A bare string on the wire, so it reads the same as a `device_state` event's
/// field and as [`Status::state`] rather than nesting an object in one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// Listening, which is where the arbiter returns between transmissions.
    Receiving,
    /// Transmitting; the receiver is deaf until this ends.
    Transmitting,
    /// Present but neither receiving nor transmitting.
    Idle,
    /// The radio could not be driven. The daemon keeps running and keeps
    /// retrying, so this is a state rather than an exit.
    Faulted,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// The daemon's own version, for diagnostics.
    pub daemon_version: String,
    pub protocol_version: u32,
    pub state: DeviceState,
    /// Board id and firmware, once the radio has been opened successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub rx: RxStatus,
    pub counters: Counters,
}

#[derive(Debug, Clone, Serialize)]
pub struct RxStatus {
    pub enabled: bool,
    pub frequency: u64,
    pub sample_rate: u32,
    /// The detector's current slicing level, absent until the first window
    /// has been measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u16>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Counters {
    pub rx_frames: u64,
    pub transmissions: u64,
    /// Bursts dropped for having more edges than a frame can plausibly hold,
    /// which is the signature of a jammed or badly sliced band.
    pub burst_overflows: u64,
    /// Times the radio had to be reopened after a failure.
    pub device_faults: u64,
}

/// Why a transmit request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    Empty,
    ZeroTiming,
    TooManyTimings { count: usize },
    FrequencyOutOfRange { frequency: u64 },
    AirTimeTooLong { duration_us: i64 },
    GainTooHigh { txvga_db: u16 },
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::Empty => write!(f, "no timings to transmit"),
            Invalid::ZeroTiming => write!(f, "a timing of zero has no meaning"),
            Invalid::TooManyTimings { count } => {
                write!(f, "{count} timings exceeds the {MAX_TIMINGS} limit")
            }
            Invalid::FrequencyOutOfRange { frequency } => {
                write!(f, "{frequency} Hz is outside the radio's 1 MHz–6 GHz range")
            }
            Invalid::AirTimeTooLong { duration_us } => write!(
                f,
                "{:.1} s of air time exceeds the {:.1} s limit",
                *duration_us as f64 / 1e6,
                MAX_AIR_TIME_US as f64 / 1e6
            ),
            Invalid::GainTooHigh { txvga_db } => {
                write!(f, "TX VGA gain {txvga_db} dB exceeds the 47 dB maximum")
            }
        }
    }
}

/// A frame far longer than any remote's is more likely a client bug than a
/// real request.
const MAX_TIMINGS: usize = 8192;
/// Ceiling on how long one request may hold the transmitter, and with it the
/// band and the receiver. A Proflame keypress is under a second.
pub const MAX_AIR_TIME_US: i64 = 30_000_000;

impl Transmit {
    /// Total air time including repetitions and the gaps between them.
    pub fn air_time_us(&self) -> i64 {
        // Saturating throughout: a hostile or buggy client must get a refusal,
        // not an overflow that wraps into a small number and sails through.
        let frame =
            self.timings.iter().fold(0i64, |total, t| total.saturating_add(t.saturating_abs()));
        let gaps = i64::from(self.repeat) * i64::from(self.gap_us);
        frame.saturating_mul(i64::from(self.repeat) + 1).saturating_add(gaps)
    }

    /// Reject what the radio cannot do, and what it should not be asked to.
    ///
    /// The air-time ceiling is the one that matters operationally: the radio
    /// is half-duplex and single-tuner, so a long transmission is also a long
    /// deafness, and this is a shared proxy.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.timings.is_empty() {
            return Err(Invalid::Empty);
        }
        if self.timings.len() > MAX_TIMINGS {
            return Err(Invalid::TooManyTimings { count: self.timings.len() });
        }
        if self.timings.contains(&0) {
            return Err(Invalid::ZeroTiming);
        }
        if !(1_000_000..=6_000_000_000).contains(&self.frequency) {
            return Err(Invalid::FrequencyOutOfRange { frequency: self.frequency });
        }
        if let Some(gain) = self.txvga_db {
            if gain > 47 {
                return Err(Invalid::GainTooHigh { txvga_db: gain });
            }
        }
        let air_time = self.air_time_us();
        if air_time > MAX_AIR_TIME_US {
            return Err(Invalid::AirTimeTooLong { duration_us: air_time });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Request {
        serde_json::from_str(json).expect("should parse")
    }

    #[test]
    fn a_minimal_transmit_request_parses() {
        let request = parse(r#"{"type":"transmit","frequency":315000000,"timings":[500,-500]}"#);

        assert_eq!(request.v, PROTOCOL_VERSION, "the version defaults to current");
        assert_eq!(request.id, None);
        let RequestPayload::Transmit(transmit) = request.payload else {
            panic!("wrong payload");
        };
        assert_eq!(transmit.timings, vec![500, -500]);
        assert_eq!(transmit.repeat, 0);
        assert!(!transmit.amp, "the power amplifier stays off unless asked for");
    }

    #[test]
    fn the_envelope_round_trips_an_id_and_version() {
        let request = parse(
            r#"{"v":1,"id":"abc","type":"configure_rx","frequency":433920000,"enabled":true}"#,
        );

        assert_eq!(request.id.as_deref(), Some("abc"));
        let RequestPayload::ConfigureRx(rx) = request.payload else {
            panic!("wrong payload");
        };
        assert_eq!(rx.frequency, Some(433_920_000));
        assert!(rx.enabled);
    }

    #[test]
    fn status_needs_no_fields() {
        assert!(matches!(parse(r#"{"type":"status"}"#).payload, RequestPayload::Status));
    }

    #[test]
    fn an_unknown_request_type_is_rejected_rather_than_ignored() {
        let result: Result<Request, _> = serde_json::from_str(r#"{"type":"self_destruct"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn events_serialize_flat_with_a_type_tag() {
        let json = serde_json::to_string(&Message::event(MessagePayload::RxFrame(RxFrame {
            frequency: 315_000_000,
            timings: vec![500, -500],
            rssi: 72,
            timestamp_ms: 1_700_000_000_000,
        })))
        .unwrap();

        assert_eq!(
            json,
            r#"{"v":1,"type":"rx_frame","frequency":315000000,"timings":[500,-500],"rssi":72,"timestamp_ms":1700000000000}"#
        );
    }

    #[test]
    fn a_reply_carries_the_request_id_and_an_event_carries_none() {
        let reply = serde_json::to_string(&Message::reply(
            Some("abc".into()),
            MessagePayload::Transmitted { duration_us: 1000 },
        ))
        .unwrap();
        assert_eq!(reply, r#"{"v":1,"id":"abc","type":"transmitted","duration_us":1000}"#);

        let event = serde_json::to_string(&Message::event(MessagePayload::DeviceState {
            state: DeviceState::Receiving,
        }))
        .unwrap();
        assert_eq!(event, r#"{"v":1,"type":"device_state","state":"receiving"}"#);
    }

    /// The same state must read the same way whether it arrives as an event or
    /// inside a status reply, rather than being a bare string in one and a
    /// nested object in the other.
    #[test]
    fn device_state_is_a_bare_string_inside_status_too() {
        let status = Status {
            daemon_version: "0.0.1".into(),
            protocol_version: PROTOCOL_VERSION,
            state: DeviceState::Faulted,
            device: None,
            rx: RxStatus {
                enabled: true,
                frequency: 315_000_000,
                sample_rate: 2_000_000,
                threshold: None,
            },
            counters: Counters::default(),
        };

        let json: serde_json::Value = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "faulted");
    }

    fn transmit(timings: Vec<i64>) -> Transmit {
        Transmit {
            frequency: 315_000_000,
            timings,
            repeat: 0,
            gap_us: 10_000,
            txvga_db: None,
            amp: false,
        }
    }

    #[test]
    fn valid_requests_pass() {
        assert_eq!(transmit(vec![500, -500, 500]).validate(), Ok(()));
    }

    #[test]
    fn air_time_counts_repeats_and_gaps() {
        let request = Transmit { repeat: 9, gap_us: 4_150, ..transmit(vec![81_100]) };

        // Ten frames of 81.1 ms plus nine gaps of 4.15 ms.
        assert_eq!(request.air_time_us(), 811_000 + 9 * 4_150);
        assert_eq!(request.validate(), Ok(()));
    }

    #[test]
    fn a_request_that_would_hold_the_band_too_long_is_refused() {
        // The radio is half-duplex: a long transmission is a long deafness,
        // and this is a shared proxy.
        let hog = Transmit { repeat: 100_000, ..transmit(vec![500_000]) };

        assert!(matches!(hog.validate(), Err(Invalid::AirTimeTooLong { .. })));
    }

    #[test]
    fn nonsense_requests_are_refused() {
        assert_eq!(transmit(vec![]).validate(), Err(Invalid::Empty));
        assert_eq!(transmit(vec![500, 0, 500]).validate(), Err(Invalid::ZeroTiming));
        assert!(matches!(
            Transmit { frequency: 315, ..transmit(vec![500]) }.validate(),
            Err(Invalid::FrequencyOutOfRange { .. })
        ));
        assert!(matches!(
            Transmit { txvga_db: Some(60), ..transmit(vec![500]) }.validate(),
            Err(Invalid::GainTooHigh { .. })
        ));
    }

    #[test]
    fn air_time_of_an_absurd_request_saturates_instead_of_wrapping() {
        // A hostile or buggy client must get a refusal, not an overflow that
        // wraps into a small number and sails through validation.
        let absurd = Transmit { repeat: u32::MAX, ..transmit(vec![i64::MAX, i64::MAX]) };

        assert!(matches!(absurd.validate(), Err(Invalid::AirTimeTooLong { .. })));
    }
}
