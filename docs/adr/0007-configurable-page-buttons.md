# 0007 — Configurable page-cycle buttons

- **Status:** accepted (revised 2026-05-13)
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
page_buttons = [                                       # optional
  { kind = "cc", number = 89 },   # slot 0 -> page 1
  { kind = "cc", number = 79 },   # slot 1 -> page 2
  { kind = "cc", number = 69 },
  { kind = "cc", number = 59 },
]
page_buttons_hold_to_preview = false                   # optional, default false
```

Validation: if `pages > 1`, *some* navigation must be configured (at least one of next/prev or a non-empty `page_buttons`). `page_buttons_hold_to_preview = true` requires `page_buttons` non-empty. `page_buttons.len() <= pages`. `page_buttons` entries must be unique among themselves and not collide with next/prev.

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

### Press-feedback flash

On press of `next_page_button` / `previous_page_button`, the proxy emits the button's "on" color immediately (via `Device::flash_color()`, e.g. green 21 on the Mini MK3) and a matching "off" message scheduled 200 ms later. The 200 ms ensures the flash is visible even on a quick tap.

The scheduling is implemented as a new `Out::DeviceDelayedSend { delay_ms, bytes }` variant. `proxy.rs` stays pure-functional and unit-testable — wall-clock timers are owned by the dispatcher (`main.rs`), which spawns a one-shot `std::thread::sleep` thread per flash. Volume is bounded by human button-tap rates (a few per second at most), so per-flash thread overhead is fine.

Page buttons (Mode B/C) don't need transient feedback — they already have stable color (active = green, inactive = dim) painted by `Device::paint_indicators`.

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
- The press-feedback flash makes button presses obvious on the Mini MK3 even when the grid is busy.

## Alternatives considered

- **Keep v1 names with new optional fields layered on top.** Confusing — `page_up_button` and `next_page_button` would coexist as synonyms.
- **Long-press detection on `page_buttons`.** A stateful timer state machine for marginal value over an explicit `_hold_to_preview` toggle.
- **Configurable home-page revert target for Mode C.** Always reverting to the last persistent page is what the user actually wanted; a `home_page` config would just be more typing.
- **Flash-while-held instead of fixed 200 ms.** Too short on tap-and-release; the 200 ms minimum was an explicit user choice.
- **Generic LED-effect framework.** YAGNI — only next/prev use the flash today.
