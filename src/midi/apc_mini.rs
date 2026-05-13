//! Akai APC mini driver.
//!
//! Grid pads are notes 0..=63 (8x8). LED color is set by the velocity of a
//! Note On message:
//!
//! | velocity | color           |
//! |---------:|-----------------|
//! |        0 | off             |
//! |        1 | green           |
//! |        2 | green blink     |
//! |        3 | red             |
//! |        4 | red blink       |
//! |        5 | yellow          |
//! |        6 | yellow blink    |

use crate::config::ButtonRef;
use crate::midi::device::Device;
use crate::midi::parse;

pub struct ApcMini;

/// Color codes documented for the APC mini.
pub mod color {
    pub const OFF: u8 = 0;
    pub const GREEN: u8 = 1;
    pub const GREEN_BLINK: u8 = 2;
    pub const RED: u8 = 3;
    pub const RED_BLINK: u8 = 4;
    pub const YELLOW: u8 = 5;
    pub const YELLOW_BLINK: u8 = 6;
}

impl Device for ApcMini {
    fn name(&self) -> &str {
        "APC mini"
    }

    fn is_grid_note(&self, note: u8) -> bool {
        note < 64
    }

    fn is_grid_cc(&self, _controller: u8) -> bool {
        false
    }

    fn boot(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn clear_all(&self) -> Vec<Vec<u8>> {
        (0u8..64)
            .map(|n| parse::note_on(0, n, color::OFF).to_vec())
            .collect()
    }

    fn paint_button(&self, btn: ButtonRef, color: u8) -> Vec<u8> {
        match btn {
            ButtonRef::Cc { number } => parse::cc(0, number, color).to_vec(),
            ButtonRef::Note { number } => parse::note_on(0, number, color).to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_table_exhaustive() {
        // pin all 7 documented codes
        assert_eq!(color::OFF, 0);
        assert_eq!(color::GREEN, 1);
        assert_eq!(color::GREEN_BLINK, 2);
        assert_eq!(color::RED, 3);
        assert_eq!(color::RED_BLINK, 4);
        assert_eq!(color::YELLOW, 5);
        assert_eq!(color::YELLOW_BLINK, 6);
    }

    #[test]
    fn clear_all_sends_64_note_offs() {
        let out = ApcMini.clear_all();
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|m| m[2] == 0));
        for (i, m) in out.iter().enumerate() {
            assert_eq!(m[1], i as u8);
        }
    }

    #[test]
    fn grid_extends_to_63() {
        assert!(ApcMini.is_grid_note(0));
        assert!(ApcMini.is_grid_note(63));
        assert!(!ApcMini.is_grid_note(64));
    }
}
