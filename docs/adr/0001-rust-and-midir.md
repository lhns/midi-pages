# 0001 — Rust + midir

- **Status:** accepted
- **Date:** 2026-05-07

## Context

`midi-pages` sits in the live MIDI path between a host (DasLight) and a USB grid controller. It must add minimal latency, run as a single small process the user can drop on a Windows machine without installing a runtime, and be easy to test off-line.

## Decision

Implement in **Rust 2024**, using the [`midir`](https://crates.io/crates/midir) crate for cross-platform MIDI I/O.

## Consequences

- One self-contained `.exe` per platform; no Node / JVM / Python install needed on user machines.
- `midir` exposes WinMM, CoreMIDI and ALSA through one API, which keeps platform code minimal.
- Unit tests can mock the I/O surface behind a small `MidiSink` trait — no real hardware needed in CI.
- Latency overhead is bounded by `midir` callback dispatch (sub-millisecond on typical hardware).

### Negative

- `midir` cannot create virtual MIDI ports on Windows; we call the Windows MIDI Services WinRT API directly for that (see ADR 0012).
- Slower iteration than Node/Python during prototyping.

## Alternatives considered

- **Node.js + easymidi/JZZ.** Matches the existing `daslight-midi-pad-editor` toolchain, but ships with a runtime and has unpredictable GC pauses in the audio path.
- **C++/JUCE** (path taken by [fabianPas/Launchpad](https://github.com/fabianPas/Launchpad)). Battle-tested, but heavier build pipeline and less ergonomic for the SysEx parsing logic we need.
- **Python + mido/python-rtmidi.** Fastest to prototype but awkward to ship as a single-file binary on Windows.
