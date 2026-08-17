//! Shared library behind the HackRF proxy tools.
//!
//! The `hrf` binary is a thin CLI over these modules, and the M2 daemon will
//! link this same library rather than growing out of the CLI. The layering is
//! strict: [`ook`] (signal processing) and [`proflame`] (protocol) are
//! hardware-free and carry the tests; only [`radio`] touches the device.

pub mod ook;
pub mod proflame;
pub mod radio;
