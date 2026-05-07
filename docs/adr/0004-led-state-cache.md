# 0004 — Per-page LED state cache

- **Status:** accepted
- **Date:** 2026-05-07

## Context

When the user switches pages, the physical LEDs must instantly show what the host had set for that page — even though the host never saw the page change and won't re-send anything.

## Decision

Maintain an in-memory `led_cache: Vec<HashMap<u8, LedCell>>` indexed by page. Every host-to-device LED message updates the cache for its target page; the page-switch routine clears the device and replays `led_cache[new_page]`.

## Consequences

- Page switches are instant and idempotent.
- We can support hosts that send LED state once at scene-load time without re-sending afterward.
- Cache is bounded: `pages * 64` entries, fixed-size MIDI payloads — no unbounded memory growth.

### Negative

- Pulsing/flashing colors (Mini MK3 channel-2/3 LED messages) are stored as the last raw payload; they should restore correctly on page replay, but this needs hardware verification.
- "Clear all" from the host must be detected and broadcast across all caches, otherwise stale colors linger after the host meant to wipe.

## Alternatives considered

- **Ask the host to redraw.** Rejected: DasLight has no such hook, and most hosts don't either.
- **Stateless pass-through.** Rejected: every page switch would go dark until the host happened to send another LED update.
