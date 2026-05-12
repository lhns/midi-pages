# 0011 — Per-port paging mode (default)

- **Status:** accepted; supersedes ADR 0003 as the default mode.
- **Date:** 2026-05-08

## Context

ADR 0003 chose to encode the page index as an offset added to the MIDI note number. In practice this hits MIDI's 7-bit ceiling: with `note_offset = 64` only 2 pages fit into the 0–127 note range, and other choices are similarly constrained. It also forces ADR 0005 (SysEx Lighting rewriting) to do nontrivial parsing/partitioning of LED messages.

After running the design with the user we realised a much simpler model works: present **each page as its own virtual MIDI controller**. The host sees N independent Launchpad-shaped controllers; the proxy bridges exactly one to the physical hardware at any time and caches LED state for the others.

## Decision

Add a new `Mode::PerPort` to `DeviceConfig`, default to it, and keep `Mode::NoteOffset` as a supported alternative. In per-port mode:

1. The proxy creates `pages` virtual host port pairs, named `<prefix>-page<N>-{in,out}` where `<prefix>` is auto-derived from `device.name`.
2. Device → host: the message is forwarded **unchanged** to the *current* page's host port. No note offset, no math.
3. Host → device: the proxy already knows which page the message was sent to from which virtual port received it. If that page is active, the message is forwarded byte-for-byte to the device. Otherwise it is cached.
4. Page change: synthesize Note Off on the **old** page's host port for held pads, then run the same clear-and-replay cycle as before.

## Consequences

- **No 7-bit ceiling.** Any sensible `pages` count works (the proxy doesn't artificially cap; WMS / OS limits dominate around 100s of endpoints).
- **No SysEx rewriting.** ADR 0005's parser is now used only by `note_offset` mode. The lighting SysEx parser stays in the codebase because `note_offset` users still rely on it.
- **DasLight mapping is symmetric across pages.** Each virtual controller exposes a fresh 0–63 (or 11–88) note layout; mappings can be copy-pasted between pages.
- More virtual endpoints to create — handled automatically (see ADR 0012).
- Per-port mode emits a new `Out::ToHostPage { page, bytes }` variant. The dispatcher in `main.rs` routes it to the right output connection.

## Alternatives considered

- **Channel offset.** Encode the page in the MIDI channel (0–15). Up to 16 pages, no port plumbing, but DasLight maps notes per channel and would require duplicated mappings — ergonomically worse than per-port.
- **Keep note offset only.** Limits us to ~2 pages of 64 pads; rejected as insufficient.
