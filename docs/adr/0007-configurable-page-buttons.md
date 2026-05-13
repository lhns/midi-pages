# 0007 — Configurable page-cycle buttons

- **Status:** accepted (revised 2026-05-13: shared-page semantics + per-button-kind colour overrides)
- **Date:** 2026-05-07, last revised 2026-05-13

## Context

The original (v1) model reserved exactly two physical buttons per device — `page_up_button` and `page_down_button` — and exposed an optional `indicator_leds` list of read-only display widgets that lit up to show which page was active. Two problems surfaced in real use:

- **Orientation-coded names**: "up/down" assumes a particular physical layout. On the Mini MK3 the top-row arrows go right-to-left visually, which made `page_up` ↔ `page_down` confusingly counter-intuitive.
- **Indicators were dead weight**: the side-strip buttons that displayed page state could just as well *be* the navigation — tap to jump directly to that page. The v1 model wasted them.
- **Only one interaction style**: walking through pages sequentially with two arrows is fine for some workflows, but for a "macro keyboard" use case where you want to hold a button to temporarily access another page (chord-of-thumb-and-finger style), there was no path.

## Decision

Four optional config keys per device, all serde-defaulting to absent:

```toml
next_page_button     = { kind = "cc", number = 91 }   # optional
previous_page_button = { kind = "cc", number = 92 }   # optional
page_buttons = [                                      # optional
  { kind = "cc", number = 89 },                       # auto-assigned to first free page
  { kind = "cc", number = 79, page = 3 },             # pinned to page 3
  { kind = "cc", number = 69, hold_to_preview = true }, # hold-mode for this button only
  { kind = "cc", number = 59 },
]
page_buttons_hold_to_preview = false                  # optional, default false (applies to entries that don't set their own)
```

Each `page_buttons` entry is a TOML table with required `kind` + `number` and two optional fields:

- `page = N`: pin the entry to page index `N`. Otherwise the entry auto-assigns to the lowest free page index in declaration order.
- `hold_to_preview = true|false`: override the device-level default just for this button. Lets you mix Mode B (tap) and Mode C (hold) buttons on the same device.

**Pages are zero-indexed throughout the schema.** A device with `pages = 4` has valid page indices `0`, `1`, `2`, `3`; persistent / preview page state, auto-assignment, and explicit `page = N` overrides all use this numbering. Internal state (`Proxy.persistent_page`, `held_preview`), log lines, and tests all match.

**Auto-assignment walks strict declaration order.** Each unconstrained entry claims the lowest page index that no earlier entry (auto-assigned or explicit) has already claimed. An explicit `page = N` later in the list is taken as written and may share a page with an earlier auto-assigned entry. Two buttons that resolve to the same page is allowed and intentional: they both light up active when that page is active, and they both jump or preview to it. (Two *explicit* `page = N` values matching each other is still rejected as a clear typo.)

Validation: if `pages > 1`, *some* navigation must be configured (at least one of next/prev or a non-empty `page_buttons`). `page_buttons_hold_to_preview = true` requires `page_buttons` non-empty. `page_buttons.len() <= pages`. `page_buttons` entries' buttons must be unique among themselves and not collide with next/prev. Explicit `page` values must be `< pages` and pair-wise distinct.

### The three resulting modes

**Mode A — sequential.** Configure only `next_page_button` and/or `previous_page_button`. Walk pages one at a time. This is what v1 did.

**Mode B — direct jump.** Configure `page_buttons`. Tapping any page button jumps persistently to that page. Next/prev still work if also configured. Indicator LEDs (driven by the same `page_buttons` list) light to show the active page — same visual as v1's `indicator_leds`, just no longer read-only.

**Mode C — hold-to-preview.** Configure `page_buttons` and set `page_buttons_hold_to_preview = true`. Holding a page button **interactively swaps** the grid to that page — pads pressed on the grid while holding fire on the previewed page (host receives them on that page's endpoint, that page's LED state is shown). Releasing reverts to the persistent page. Tapping a page button has no persistent effect; the persistent page is moved only via next/prev.

The three modes compose: A + B coexists naturally (next/prev plus direct-jump); A + C lets you walk pages persistently AND hold-preview a different page momentarily.

### State model

`Proxy` tracks two pages:

- `persistent_page: u8` — the page set by next/prev or by a Mode-B tap. Persists across hold-preview windows.
- `held_preview: Option<u8>` — `Some(p)` while a Mode-C page button is held; `None` otherwise.

The visible page is the derived value `held_preview.unwrap_or(persistent_page)`, cached as `current_page` and refreshed by `set_persistent_page` / `enter_preview` / `exit_preview`. All grid I/O routes through `current_page`, which makes Mode-C preview interactive without any special-case code in the input/output paths.

Held-pad note-off synthesis (the existing "no stuck notes on page change" mechanism) runs on every page swap — including enter/exit preview — so a pad held during a hold-preview window gets its note-off on the previewed page when the preview ends.

### Press feedback (lit while held)

On press of `next_page_button` / `previous_page_button`, the proxy paints the button with `active_color`. On release it paints `inactive_color`. The LED is lit for as long as the physical button is held, no timer involved. Same `Device::paint_button` primitive the page-button indicators use.

This replaced an earlier 200 ms `Out::DeviceDelayedSend`-based flash. The new behaviour gives clearer tactile-visual feedback (the LED tracks the finger exactly) and removes the wall-clock dispatcher path entirely. `proxy.rs` stays purely event-driven.

Page buttons (Mode B/C) don't need this on-press override; they already have stable color (active / inactive / preview) painted by `Proxy::paint_indicator_state` (see below).

### Shared-page semantics

Two `page_buttons` entries that resolve to the same page index (auto-then-explicit collision) is **allowed and intentional**. Both buttons:

- Light up with the active colour when their shared page is the persistent page.
- Light up with the preview colour when their shared page is currently being previewed.
- Tap-jump (Mode B) or hold-preview (Mode C) the same page, according to each button's own `hold_to_preview` resolution.

This lets you, e.g., assign the same logical page to a side-strip button (for indicator visibility) and a grid pad (for ergonomic tap-jump) without writing wrapper logic. Two *explicit* `page = N` values matching each other is still rejected as a likely typo.

### Indicator colors

Seven colours, all configurable per device via the optional `colors` config block. Three are tied to **page buttons**, two to **next/prev** buttons, two more to **hold-to-preview** page buttons when they're not currently held:

| field              | role                                                                       | default |
|--------------------|----------------------------------------------------------------------------|---------|
| `active`           | tap page button on its persistent (active) page                            | `1`     |
| `inactive`         | tap page button on any other page                                          | `0`     |
| `preview`          | page button currently held showing preview (Mode C)                        | `active`|
| `active_cycle`     | next/previous_page_button while held                                       | `active`|
| `inactive_cycle`   | next/previous_page_button when idle                                        | `inactive`|
| `active_preview`   | hold-to-preview page button on the persistent page (not currently held)    | `active`|
| `inactive_preview` | hold-to-preview page button on any other page (not currently held)         | `inactive`|

The `colors` block and every field inside it are optional. Missing fields cascade from the parents listed in the default column, with `active`/`inactive` falling back to **device-agnostic constants** (`1` = lit, `0` = off). These fallbacks are deliberately minimal so the proxy doesn't hardcode device-specific palettes; the example config shows recommended per-device overrides.

Driver-specific colour defaults were removed in 2026-05-13. `Device::default_colors()` and the `DefaultColors` struct are gone. The proxy uses two universal constants (`DEFAULT_ACTIVE = 1`, `DEFAULT_INACTIVE = 0`) and every other colour falls back to one of those two; users tune palettes via `[device.colors]`.

**Cascade rule**: when painting page-button indicators, the priority per slot is:

1. **preview** wins if the slot's page matches `held_preview` (Mode C),
2. else for the slot's page == `persistent_page`: **active** if the button is tap-mode (`hold = false`), **active_preview** if hold-mode,
3. else **inactive** for tap-mode slots, **inactive_preview** for hold-mode slots.

Next/prev paint with `inactive_cycle` when idle and `active_cycle` while held. Implemented in `Proxy::paint_indicator_state` (page buttons + idle next/prev) and `paint_nav_held` / `handle_nav_release` (held / released next/prev).

### Pages vs page_buttons mismatch

`page_buttons` and `pages` are independent counts:

- `page_buttons.len() > pages` is rejected by config validation. The extras would be unreachable.
- `page_buttons.len() < pages` is allowed. Pages with index `page_buttons.len() .. pages` are reachable only via `next_page_button` / `previous_page_button`. They have no indicator LED, no direct-jump tap, and no hold-preview gesture.

If `page_buttons.len() < pages` AND neither next nor prev is configured, the proxy emits a `WARN` log at startup naming the unreachable pages. The config still loads; the reachable pages work normally.

### Migration from v1

```toml
# Before (v1)
page_up_button   = { kind = "cc", number = 91 }
page_down_button = { kind = "cc", number = 92 }
indicator_leds   = [ ... ]

# After (v2)
next_page_button     = { kind = "cc", number = 91 }
previous_page_button = { kind = "cc", number = 92 }
page_buttons         = [ ... ]
```

Clean break: no serde aliases for the old keys. Single-user project; the user does the rename in the same commit that lands this change.

## Consequences

- Trivial to support APC mini (which has no top-row CCs) alongside Mini MK3 (which has CC 91/92 arrows) — both work in any of the three modes.
- Users can dedicate any pad as a page button at the cost of one of the 64 grid slots.
- Workflows that need "momentarily peek at another page" (Mode C) are now expressible without giving up persistent navigation.
- Next/prev light up while held (active color) so button presses are obvious on the Mini MK3 even when the grid is busy. Idle next/prev show inactive color so the buttons are visibly "navigation".

## Alternatives considered

- **Keep v1 names with new optional fields layered on top.** Confusing — `page_up_button` and `next_page_button` would coexist as synonyms.
- **Long-press detection on `page_buttons`.** A stateful timer state machine for marginal value over an explicit `_hold_to_preview` toggle.
- **Configurable home-page revert target for Mode C.** Always reverting to the last persistent page is what the user actually wanted; a `home_page` config would just be more typing.
- **Fixed-duration timer flash on press.** Tried first as a 200 ms `Out::DeviceDelayedSend` flash; rejected because it disconnects the LED from the physical button. Lit-while-held tracks the finger directly, no timer, less code.
- **Generic LED-effect framework.** YAGNI — only next/prev get an on-press paint, and it's a single line.
- **Per-page custom colors** (one color per page slot). Rejected as overkill for the macro-keyboard use case; active/inactive/preview is enough state to be readable at a glance.
