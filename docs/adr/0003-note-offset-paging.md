# 0003 — Note-number offset (not channel offset) for paging

- **Status:** accepted
- **Date:** 2026-05-07

## Context

The proxy multiplies the apparent button count by `pages`. There are two natural ways to encode the page in the MIDI stream the host sees: shift the note number, or shift the MIDI channel.

## Decision

Page `p` shifts the note number by `p * note_offset` (default `64`). The MIDI channel is preserved unchanged.

## Consequences

- DasLight (and most grid-aware host software) maps **notes** to scenes/effects, not channels. A note offset slots straight into existing UIs without rewiring channel assignments.
- 4 pages × 64 grid notes = 256 logical notes; well within the 0..127 MIDI range when `pages * note_offset <= 128`. The default 4 × 64 fits.
- `note_offset` is configurable so users with non-Launchpad layouts can pick a different stride.

### Negative

- We cannot exceed `(128 - max_grid_note) / note_offset` pages. For a Launchpad Mini MK3 (grid notes up to 88 in programmer mode) the practical ceiling is around 1-2 pages with the default offset. Users who need more should drop `note_offset` to 64 and accept that grid-only notes (0-63) get the full multiplier; configuration validation enforces this.

## Alternatives considered

- **Channel offset.** Cleaner conceptually but breaks the way DasLight ingests MIDI from a grid controller; users would have to duplicate channel mappings per page.
- **Per-page configurable mapping.** Rejected for v1 to keep config simple. Can be added later as an opt-in.
