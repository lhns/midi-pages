# 0007 — Configurable page-cycle buttons

- **Status:** accepted
- **Date:** 2026-05-07

## Context

Different controllers have differently-located "natural" page buttons (top-row arrows on Mini MK3, no dedicated arrows on APC mini), and some users want to repurpose grid pads as page buttons.

## Decision

The two reserved page-up / page-down buttons are configured per device profile as `{ kind, number }` pairs, where `kind` is `note` or `cc`. Defaults are documented in `config.toml.example`.

## Consequences

- Trivial to support APC mini (which has no top-row CCs) alongside Mini MK3 (which has CC 91/92 arrows).
- Users can dedicate any pad as a page button at the cost of one of the 64 grid slots.
- Config schema validates that the two buttons differ and don't collide with `indicator_leds`.

## Alternatives considered

- **Hard-code top-row arrows.** Works for Mini MK3 only; breaks APC mini and reduces flexibility.
- **Auto-detect based on the device profile.** Implicit and surprising; explicit config is clearer.
