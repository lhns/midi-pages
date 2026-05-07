# 0002 — Use loopMIDI for host-side virtual ports on Windows

- **Status:** accepted
- **Date:** 2026-05-07

## Context

The proxy needs to expose a pair of MIDI ports the host application (DasLight) can connect to in place of the real controller. On Linux and macOS this is built into the OS MIDI stack and `midir` exposes it. On Windows there is no first-class API for creating virtual MIDI ports without a signed kernel driver.

## Decision

On Windows, **require the user to install [loopMIDI](https://www.tobias-erichsen.de/software/loopmidi.html)** (free) and create two ports up-front. `midi-pages` opens those ports as if they were any other MIDI device.

## Consequences

- Zero kernel-driver work on our side; loopMIDI is widely deployed and trusted.
- Port creation/teardown is the user's responsibility — documented in `README.md`.

### Negative

- One extra install step on Windows (a few seconds, but a friction point for first-time users).
- We can't programmatically guarantee the loopMIDI ports exist; we surface a clear error if they're missing.

## Alternatives considered

- **Bundle the [virtualMIDI SDK](https://www.tobias-erichsen.de/software/virtualmidi.html).** Lets us create ports from inside the proxy. Rejected: the SDK requires a paid license for redistribution and ships its own signed driver, which adds installer complexity and trust burden.
- **Custom WDM driver.** Out of scope by orders of magnitude.
