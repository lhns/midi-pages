//! Device-specific behaviour (Mini MK3 vs APC mini).

use crate::config::ButtonRef;

/// Per-device behaviour the proxy needs in order to remain device-agnostic.
pub trait Device: Send + Sync {
    fn name(&self) -> &str;

    /// True if this physical note is part of the 8x8 grid we want to page.
    /// (Top-row arrows, side buttons, and the shift/logo are *not* grid notes.)
    fn is_grid_note(&self, note: u8) -> bool;

    /// Same for CC controllers: side-strip CCs that should never be paged.
    fn is_grid_cc(&self, controller: u8) -> bool;

    /// Boot bytes (e.g. switch to programmer mode). Run once on startup.
    fn boot(&self) -> Vec<Vec<u8>>;

    /// Bytes that wipe all grid LEDs to off. Called before replaying a page.
    fn clear_all(&self) -> Vec<Vec<u8>>;

    /// Paint a single button (next/prev or a page button) at the given color.
    /// Color semantics are device-specific; `0` always means off. Returns one
    /// MIDI message (CC or Note On as per the ButtonRef variant).
    fn paint_button(&self, btn: ButtonRef, color: u8) -> Vec<u8>;

    /// Convenience: same as `paint_button(btn, 0)`.
    fn clear_button(&self, btn: ButtonRef) -> Vec<u8> {
        self.paint_button(btn, 0)
    }
}

/// Drivers known by name in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Driver {
    MiniMk3,
    ApcMini,
}
