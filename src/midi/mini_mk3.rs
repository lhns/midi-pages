//! Launchpad Mini MK3 driver.
//!
//! Grid pads in programmer mode are notes 11..=88 (8x8 layout, row stride 10).
//! Top row buttons are CCs 91..=98. Side column buttons are CCs 19/29/.../89.

use crate::config::ButtonRef;
use crate::midi::device::Device;
use crate::midi::parse;
use crate::midi::sysex_lighting::{ColorSpec, LedSpec, LightingSysex, MINI_MK3};

pub struct MiniMk3;

impl Device for MiniMk3 {
    fn name(&self) -> &str {
        MINI_MK3.name
    }

    fn is_grid_note(&self, note: u8) -> bool {
        // Programmer-mode grid is 11..=88 with column 1..=8 and row 1..=8.
        let row = note / 10;
        let col = note % 10;
        (1..=8).contains(&row) && (1..=8).contains(&col)
    }

    fn is_grid_cc(&self, _controller: u8) -> bool {
        // No grid CCs on Mini MK3 — top row & side strip CCs are never paged.
        false
    }

    fn boot(&self) -> Vec<Vec<u8>> {
        // Switch to Programmer mode so SysEx LED control is live.
        vec![vec![0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x0E, 0x01, 0xF7]]
    }

    fn clear_all(&self) -> Vec<Vec<u8>> {
        // One Lighting SysEx that turns every grid LED off.
        let leds: Vec<_> = (1u8..=8)
            .flat_map(|row| (1u8..=8).map(move |col| row * 10 + col))
            .map(|n| LedSpec {
                led_index: n,
                color: ColorSpec::Static(0),
            })
            .collect();
        vec![
            LightingSysex {
                model: MINI_MK3,
                leds,
            }
            .emit(),
        ]
    }

    fn paint_indicators(&self, page: u8, indicators: &[ButtonRef]) -> Vec<Vec<u8>> {
        // Light the indicator at slot `page` green, others dim white.
        const COLOR_ACTIVE: u8 = 21; // green
        const COLOR_INACTIVE: u8 = 1; // dim white
        indicators
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let v = if i as u8 == page {
                    COLOR_ACTIVE
                } else {
                    COLOR_INACTIVE
                };
                match b {
                    ButtonRef::Cc { number } => parse::cc(0, *number, v).to_vec(),
                    ButtonRef::Note { number } => parse::note_on(0, *number, v).to_vec(),
                }
            })
            .collect()
    }
}
