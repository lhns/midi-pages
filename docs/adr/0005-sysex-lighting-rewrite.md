# 0005 — Rewrite SysEx Lighting messages on the fly

- **Status:** scoped to `note_offset` mode only after [ADR 0011](0011-per-port-paging.md). In `per_port` mode SysEx Lighting messages are forwarded byte-for-byte without rewriting.
- **Date:** 2026-05-07

## Context

In programmer mode the Launchpad Mini MK3 and Launchpad X accept a SysEx **Lighting** message that batches LED commands as repeated `(spec, led_index, color...)` triplets. DasLight uses this message heavily because it can update many LEDs in one MIDI write. A naive proxy that only rewrites Note Ons would silently drop these updates.

See: Launchpad Mini MK3 Programmer's Reference, [Launchpad X Programmer's Reference](https://fael-downloads-prod.focusrite.com/customer/prod/s3fs-public/downloads/Launchpad%20X%20-%20Programmers%20Reference%20Manual.pdf).

## Decision

Parse incoming Lighting SysEx, walk the triplets, partition them by `led_index / note_offset`, write each into the appropriate page's LED cache, and emit a re-serialised SysEx containing **only** the triplets that target the currently-active page (with `led_index` rewritten to its physical value). If the filtered SysEx would be empty, emit nothing.

## Consequences

- DasLight's batched LED updates work correctly across pages.
- We need a small, well-tested SysEx parser/emitter; that's worth its own module (`midi/sysex_lighting.rs`) and a fixture-driven test set.

### Negative

- Slightly higher per-message overhead than a memcpy passthrough (a few microseconds per triplet on commodity hardware — irrelevant in the audio path).
- Other Launchpad models (Pro MK3, MK2) use a different SysEx header byte; the parser is parameterised over the model header but each new model needs an explicit table entry.

## Alternatives considered

- **Only support Note-On-style LED control.** Rejected: DasLight uses programmer-mode SysEx by default.
- **Re-emit the entire SysEx unchanged and rely on the device to ignore off-page LEDs.** Impossible — the device has no concept of pages.
