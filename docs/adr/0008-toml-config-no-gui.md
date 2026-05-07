# 0008 — TOML config file, no GUI in v1

- **Status:** accepted
- **Date:** 2026-05-07

## Context

The proxy is a long-running background process. It needs configuration (port names, page count, button assignments) but the configuration changes rarely.

## Decision

A single `config.toml` file is the source of truth. No GUI in v1. CLI flags cover only `--config`, `--list-ports`, and log level.

## Consequences

- Faster to ship.
- Easy to version-control alongside the rest of the user's setup.
- A future v2 can wrap this binary in a tray app or embed the existing `daslight-midi-pad-editor` (Lit/JS) UI via [`tauri`](https://tauri.app/) without changing the core.

## Alternatives considered

- **Tauri tray app from day one.** Rejected for v1 — doubles the build matrix and slows iteration on the MIDI logic.
- **CLI-only (no config file).** Rejected: too many parameters per device.
