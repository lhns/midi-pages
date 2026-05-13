# 0014 — Global buttons (page-independent buttons)

- **Status:** accepted
- **Date:** 2026-05-13

## Context

The paging model assumes each grid pad belongs to whichever page is currently visible. Pressing pad 11 on page 2 sends "pad 11 on page 2" to the host; the host's LED writes for pad 11 on page 0 are cached but don't show until page 0 is visible again.

For some workflows that's too rigid. The user's macro-pad case wants a small set of buttons (e.g. a transport row at the bottom of the grid, or a master mute) that **always** fire on page 0, regardless of which page is visible, and whose LED always reflects page 0's state. From the host's perspective those buttons live on a single page; from the user's perspective they always behave the same no matter where they currently are in the paging tree.

## Decision

A new optional config field `global_buttons: Vec<ButtonRef>` on each `[[device]]`. Each entry is a `ButtonRef` (`{ kind = "note" | "cc", number = ... }`) addressable on the device.

```toml
[[device]]
# ...
global_buttons = [
  { kind = "note", number = 11 },   # bottom-left pad as a transport button
  { kind = "note", number = 12 },
]
```

### Routing rules

**Device to host** (`Proxy::handle_device_in`). When a Note On / Note Off / CC event matches a global button:

- The proxy emits the message to **page 0** (`Out::ToHostPage { page: 0, ... }` in PerPort mode, raw bytes via `Out::ToHost` in NoteOffset mode where page 0 means zero offset).
- The proxy **does not** track the press in `self.held`. A global note is by definition page-independent, so a page change while it's held does not synthesize a phantom note-off (the `change_page_to` loop iterates `self.held`, which globals aren't in).

**Host to device** (`Proxy::handle_host_in_per_port` and `Proxy::handle_host_in`). When a host message addresses a global button:

- The LED is cached at `led_cache[0]`, regardless of which page port the message arrived on (PerPort) or what page number it would offset to (NoteOffset).
- The message is forwarded to the device immediately, regardless of `current_page`. The global LED always reflects page 0's latest write.

**Page change** (`Proxy::change_page_to`). After the normal "clear + replay current page's cache + paint indicators" sequence, when `current_page != 0` the proxy walks `global_buttons` and overlays each one's page-0 cache entry on top. Net effect: switching pages or entering a preview never blanks a global button's LED.

### Validation

- Entries must be pair-wise unique and must not collide with `next_page_button`, `previous_page_button`, or any `page_buttons` entry.
- In `mode = "note_offset"`, each entry's `number` must be `< note_offset` so it fits in page 0's range.

Note: no "is this button on the grid?" check. Non-grid CCs (e.g. Mini MK3 top-row arrows CC 91..98 or side-strip CCs) and non-grid notes are legitimate global targets. The proxy treats them as passthrough-to-page-0 just like grid buttons; the only difference is that LED cache state for non-grid buttons is, well, the same page-0 cache. Whether a button is "grid" is a driver-internal routing concept (see `Device::is_grid_note` / `is_grid_cc` in `src/midi/device.rs`) and not a configuration constraint.

### Implementation footprint

- `src/config.rs`: `DeviceConfig.global_buttons: Vec<ButtonRef>` and the validation rules above.
- `src/proxy.rs`: `Proxy.global_buttons: HashSet<ButtonRef>`. Branch in `handle_device_in` (Note/CC paths) that short-circuits to page 0. Branch in `handle_host_in_per_port` that detects global addressing and caches at page 0. Branches in `handle_host_in` / `handle_host_note_offset` for NoteOffset mode. New `replay_globals_from_page0` helper invoked from `change_page_to` after the regular replay.

## Consequences

- The host can map a transport row exactly once (on page 0) and never re-bind it per page.
- A global button can collide with the "physical pad" semantics on non-page-0 pages: pressing a global button while page 2 is visible will *not* show up as a page-2 press. That's the whole point; users who want both behaviours can keep the button non-global.
- Caching at page 0 is intentional even when the host wrote to a different page's port. A host that double-binds the same pad on multiple pages will see page-0 win for that pad. Treat that as the user's responsibility (they shouldn't double-bind a button they explicitly marked global).
- Held-pad synthesis on page change skips globals. That's correct (the button is still pressed and will release on the same page-0 binding) but slightly surprising if you imagine globals as "normal pads with a routing tweak". The mental model is closer to "this pad doesn't participate in paging at all".

## Alternatives considered

- **Per-page exception lists**: declare which buttons are page-independent per page. Way more config than the user needs; one global flag is enough.
- **A second route table that the user fills in manually** (e.g. "this physical button maps to logical port X note Y"). More flexible but unnecessary for the macro-pad use case; reuses the existing page-0 cache machinery.
- **Reuse `page_buttons` with a `page = 0` pin**: doesn't work, page buttons are nav-only and don't pass pad presses through to the host.
- **Apply held-pad note-off synthesis to globals anyway**: would emit phantom Note Offs on page change for a global pad the user is holding, which the host then has to ignore. Skipping is cleaner.
