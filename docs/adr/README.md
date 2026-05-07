# Architecture Decision Records

These ADRs use a [MADR-lite](https://adr.github.io/madr/) shape: each file states the **Context**, the **Decision**, the **Consequences** (good and bad), and the **Alternatives** considered.

ADRs are write-once-then-superseded. If a decision changes, add a new ADR that links back to and supersedes the old one rather than editing history.

## Index

- [0001 — Rust + midir](0001-rust-and-midir.md)
- [0002 — loopMIDI on Windows](0002-loopmidi-on-windows.md)
- [0003 — Note-number offset for paging](0003-note-offset-paging.md) *(superseded by 0011 as default)*
- [0004 — Per-page LED state cache](0004-led-state-cache.md)
- [0005 — Rewrite SysEx Lighting messages](0005-sysex-lighting-rewrite.md) *(scoped to `note_offset` mode after 0011)*
- [0006 — Synthesize Note Off on page change](0006-held-pad-note-off-on-page-change.md)
- [0007 — Configurable page-cycle buttons](0007-configurable-page-buttons.md)
- [0008 — TOML config, no GUI](0008-toml-config-no-gui.md)
- [0009 — One process, N device profiles](0009-multi-device-profiles.md)
- [0010 — GitHub Actions CI / release](0010-ci-and-release-pipeline.md)
- [0011 — Per-port paging mode (default)](0011-per-port-paging.md)
- [0012 — Auto-create virtual host-side ports](0012-auto-create-virtual-ports.md)
