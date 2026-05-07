# 0006 — Synthesize Note Off on the previous page when paging while held

- **Status:** accepted
- **Date:** 2026-05-07

## Context

If the user is holding a pad when they press page-up/down, the host has a Note On for `physical + old_page * 64` outstanding. After the page change, releasing that pad would naturally emit Note Off for `physical + new_page * 64`, which the host would ignore — leaving a stuck note.

## Decision

Track the set of currently-pressed physical pads. On page change, synthesize a Note Off for each held pad on the **old** page's logical number before mutating `current_page`, then continue with the page-change redraw.

## Consequences

- The host never sees an unmatched Note On.
- When the user releases the pad later, we suppress the corresponding Note Off so it isn't double-emitted.

### Negative

- A small extra piece of state (the held-pad set). Bounded at 64 entries.

## Alternatives considered

- **Ignore the problem.** Rejected: stuck notes show up immediately as scenes that won't release in DasLight.
- **Send Note Off on the new page.** Wrong — the host doesn't know that note was ever pressed.
