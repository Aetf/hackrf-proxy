# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/Aetf/hackrf-proxy/compare/v0.1.1...v0.1.2) - 2026-08-22

### Other

- update Cargo.lock dependencies

## [0.1.1](https://github.com/Aetf/hackrf-proxy/compare/v0.1.0...v0.1.1) - 2026-08-20

### Other

- keep nusb on a single version shared with the driver ([#9](https://github.com/Aetf/hackrf-proxy/pull/9))

## [0.1.0] - 2026-08-20

The first public release.

### Added

- `hackrf-proxyd` (binary `hrf`): a network-attached OOK transceiver daemon
  for the HackRF One — a WebSocket API moving raw signed-microsecond timings,
  a half-duplex arbiter that receives by default and lets transmissions
  preempt, continuous-receive burst detection with a noise-adaptive
  threshold, and fault recovery that only believes a radio once a transfer
  has actually arrived.
- Bench tools for solving OOK protocols: `scan`, `capture`, `demod`
  (offline and through the live detector's code path) and `transmit`.
- `hackrf-proxy-client` on PyPI: an async Python client released in lockstep
  with the daemon; compatibility contract is same-semver-major, checked at
  connect.
- Container images at `ghcr.io/aetf/hackrf-proxyd` and static musl binaries
  on every release.
