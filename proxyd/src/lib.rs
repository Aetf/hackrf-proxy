//! A network-attached HackRF: the daemon, and the signal work behind it.
//!
//! The `hrf` binary is a thin CLI over these modules. The layering is strict,
//! and it is what keeps the project testable without a radio on the bench:
//!
//! - [`ook`] — signal processing. IQ to timings and back, offline and
//!   streaming. No hardware.
//! - [`proflame`] — the fireplace protocol, both directions. No hardware, and
//!   nothing else depends on it: the daemon never links appliance semantics
//!   into the radio path.
//! - [`wire`] — the WebSocket protocol's types and validation. No hardware.
//! - [`engine`] — the half-duplex arbiter. Owns the radio through a trait, so
//!   the state machine is tested against a fake device.
//! - [`server`] — the WebSocket front end. Talks to [`engine`] by channel.
//! - [`radio`] — the only module that needs a HackRF plugged in.

pub mod engine;
pub mod ook;
pub mod proflame;
pub mod radio;
pub mod server;
pub mod wire;
