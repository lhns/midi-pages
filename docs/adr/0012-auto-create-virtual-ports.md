# 0012 — Auto-create virtual host-side ports

- **Status:** accepted
- **Date:** 2026-05-08

## Context

Per-port mode (ADR 0011) needs N virtual port pairs per controller. Asking users to create 8+ ports manually in loopMIDI before each session is friction we'd like to avoid.

Available primitives:

- **Linux & macOS:** `midir::MidiInput::create_virtual` and `MidiOutput::create_virtual` create OS-level virtual ports natively.
- **Windows:** no public API for creating virtual MIDI ports without a signed kernel driver. loopMIDI's GUI is the standard. **However**, loopMIDI exposes a CLI: `loopMIDI.exe -new "PortName"` adds a port (the GUI must be running for the port to persist).

## Decision

Best-effort auto-creation, transparent to the user:

- **Linux & macOS:** The proxy calls `create_virtual_*` for any host-side port that isn't already present. Zero setup.
- **Windows:** `ports::ensure_loopmidi_port(name)` checks whether the port exists; if not, it shells out to `loopMIDI.exe -new "<name>"` (looked up in PATH and at the two default install locations under `C:\Program Files`). After a brief pause it re-checks; if the port still isn't there, it returns a clear error explaining that the loopMIDI GUI must be running.
- The proxy never creates the *device* port — that's real hardware; missing-device is always a hard error.

## Consequences

- Linux & macOS: zero-config experience.
- Windows: one-time install of loopMIDI is still required (we don't redistribute it), but no manual port creation.
- We don't bundle Tobias Erichsen's `teVirtualMIDI` SDK (commercial licence + signed driver overhead, see ADR 0002), keeping the binary distribution unencumbered.

## Alternatives considered

- **Bundle teVirtualMIDI SDK.** Cleaner UX (proxy creates ports directly), but paid redistribution licence and driver-installer complexity. Rejected for v1.
- **Document manual loopMIDI port creation only.** Works, but creates real friction every time a user changes `pages` in their config.
