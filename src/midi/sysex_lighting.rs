//! Parser/emitter for the Novation Launchpad **Lighting SysEx** message.
//!
//! Format (Mini MK3 / Launchpad X):
//!
//! ```text
//! F0 00 20 29 02 <model> 03  ( <spec> <led_index> <color...> )*  F7
//! ```
//!
//! `<spec>` selects the color encoding for the next triplet:
//! - `0x00`: static palette (1 color byte)
//! - `0x01`: flashing palette (2 color bytes — color A, color B)
//! - `0x02`: pulsing palette (1 color byte)
//! - `0x03`: RGB (3 bytes, each 0..127)
//!
//! See: Launchpad Mini MK3 / Launchpad X Programmer's Reference Manuals.
//!
//! This parser is permissive: it tolerates trailing bytes the device might
//! ignore but rejects truncated payloads with a typed error. The proxy uses it
//! to walk and rewrite triplets, not to validate every nibble.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SysexError {
    #[error("not a sysex (does not start with F0)")]
    NotSysex,
    #[error("missing F7 terminator")]
    NoTerminator,
    #[error("not a Novation lighting message (manufacturer/model mismatch)")]
    NotLighting,
    #[error("payload truncated mid-triplet at byte {0}")]
    Truncated(usize),
    #[error("unknown color spec {0:#x}")]
    UnknownSpec(u8),
}

/// Manufacturer header bytes shared by all Novation SysEx.
pub const NOVATION_HEADER: [u8; 3] = [0x00, 0x20, 0x29];

/// Per-model SysEx header (after `F0 00 20 29 02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelHeader {
    pub model_byte: u8,
    pub name: &'static str,
}

pub const MINI_MK3: ModelHeader = ModelHeader {
    model_byte: 0x0D,
    name: "Launchpad Mini MK3",
};
pub const LAUNCHPAD_X: ModelHeader = ModelHeader {
    model_byte: 0x0C,
    name: "Launchpad X",
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedSpec {
    pub led_index: u8,
    pub color: ColorSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorSpec {
    Static(u8),
    Flashing { color_a: u8, color_b: u8 },
    Pulsing(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

impl ColorSpec {
    fn spec_byte(&self) -> u8 {
        match self {
            ColorSpec::Static(_) => 0x00,
            ColorSpec::Flashing { .. } => 0x01,
            ColorSpec::Pulsing(_) => 0x02,
            ColorSpec::Rgb { .. } => 0x03,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            ColorSpec::Static(c) => out.push(*c),
            ColorSpec::Flashing { color_a, color_b } => {
                out.extend_from_slice(&[*color_a, *color_b])
            }
            ColorSpec::Pulsing(c) => out.push(*c),
            ColorSpec::Rgb { r, g, b } => out.extend_from_slice(&[*r, *g, *b]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightingSysex {
    pub model: ModelHeader,
    pub leds: Vec<LedSpec>,
}

impl LightingSysex {
    /// Parse raw SysEx bytes (including F0/F7 framing).
    pub fn parse(bytes: &[u8], model: ModelHeader) -> Result<Self, SysexError> {
        if bytes.first() != Some(&0xF0) {
            return Err(SysexError::NotSysex);
        }
        if bytes.last() != Some(&0xF7) {
            return Err(SysexError::NoTerminator);
        }
        // Header: F0 00 20 29 02 <model> 03
        let prefix = [0x00, 0x20, 0x29, 0x02, model.model_byte, 0x03];
        if bytes.len() < 1 + prefix.len() + 1 || bytes[1..1 + prefix.len()] != prefix {
            return Err(SysexError::NotLighting);
        }
        let body = &bytes[1 + prefix.len()..bytes.len() - 1];
        let mut leds = Vec::new();
        let mut i = 0;
        while i < body.len() {
            let spec = body[i];
            i += 1;
            if i >= body.len() {
                return Err(SysexError::Truncated(i));
            }
            let led_index = body[i];
            i += 1;
            let color = match spec {
                0x00 => {
                    if i >= body.len() {
                        return Err(SysexError::Truncated(i));
                    }
                    let c = body[i];
                    i += 1;
                    ColorSpec::Static(c)
                }
                0x01 => {
                    if i + 1 >= body.len() {
                        return Err(SysexError::Truncated(i));
                    }
                    let a = body[i];
                    let b = body[i + 1];
                    i += 2;
                    ColorSpec::Flashing {
                        color_a: a,
                        color_b: b,
                    }
                }
                0x02 => {
                    if i >= body.len() {
                        return Err(SysexError::Truncated(i));
                    }
                    let c = body[i];
                    i += 1;
                    ColorSpec::Pulsing(c)
                }
                0x03 => {
                    if i + 2 >= body.len() {
                        return Err(SysexError::Truncated(i));
                    }
                    let r = body[i];
                    let g = body[i + 1];
                    let b = body[i + 2];
                    i += 3;
                    ColorSpec::Rgb { r, g, b }
                }
                other => return Err(SysexError::UnknownSpec(other)),
            };
            leds.push(LedSpec { led_index, color });
        }
        Ok(LightingSysex { model, leds })
    }

    /// Serialise back to raw SysEx bytes, including F0/F7 framing.
    pub fn emit(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.leds.len() * 5);
        out.push(0xF0);
        out.extend_from_slice(&NOVATION_HEADER);
        out.extend_from_slice(&[0x02, self.model.model_byte, 0x03]);
        for led in &self.leds {
            out.push(led.color.spec_byte());
            out.push(led.led_index);
            led.color.write(&mut out);
        }
        out.push(0xF7);
        out
    }

    /// Returns true if this looks like a Lighting SysEx for the given model
    /// (cheap check for the dispatch path; does not parse the body).
    pub fn looks_like(bytes: &[u8], model: ModelHeader) -> bool {
        bytes.len() >= 8
            && bytes[0] == 0xF0
            && bytes[1..4] == NOVATION_HEADER
            && bytes[4] == 0x02
            && bytes[5] == model.model_byte
            && bytes[6] == 0x03
            && *bytes.last().unwrap() == 0xF7
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lighting(leds: Vec<LedSpec>) -> Vec<u8> {
        LightingSysex {
            model: MINI_MK3,
            leds,
        }
        .emit()
    }

    #[test]
    fn parse_one_static_led() {
        let bytes = lighting(vec![LedSpec {
            led_index: 11,
            color: ColorSpec::Static(5),
        }]);
        let parsed = LightingSysex::parse(&bytes, MINI_MK3).unwrap();
        assert_eq!(parsed.leds.len(), 1);
        assert_eq!(parsed.leds[0].led_index, 11);
        assert_eq!(parsed.leds[0].color, ColorSpec::Static(5));
    }

    #[test]
    fn parse_64_rgb_leds_in_order() {
        let leds: Vec<_> = (0u8..64)
            .map(|i| LedSpec {
                led_index: i,
                color: ColorSpec::Rgb {
                    r: i,
                    g: 0,
                    b: 127 - i,
                },
            })
            .collect();
        let bytes = lighting(leds.clone());
        let parsed = LightingSysex::parse(&bytes, MINI_MK3).unwrap();
        assert_eq!(parsed.leds, leds);
    }

    #[test]
    fn round_trip_mixed_specs() {
        let leds = vec![
            LedSpec {
                led_index: 1,
                color: ColorSpec::Static(5),
            },
            LedSpec {
                led_index: 2,
                color: ColorSpec::Flashing {
                    color_a: 10,
                    color_b: 20,
                },
            },
            LedSpec {
                led_index: 3,
                color: ColorSpec::Pulsing(7),
            },
            LedSpec {
                led_index: 4,
                color: ColorSpec::Rgb { r: 1, g: 2, b: 3 },
            },
        ];
        let bytes = lighting(leds.clone());
        let parsed = LightingSysex::parse(&bytes, MINI_MK3).unwrap();
        assert_eq!(parsed.leds, leds);
        assert_eq!(parsed.emit(), bytes);
    }

    #[test]
    fn rejects_wrong_manufacturer() {
        let bytes = [0xF0, 0x7E, 0x00, 0x06, 0x01, 0xF7];
        assert_eq!(
            LightingSysex::parse(&bytes, MINI_MK3),
            Err(SysexError::NotLighting)
        );
    }

    #[test]
    fn rejects_missing_terminator() {
        let mut bytes = lighting(vec![LedSpec {
            led_index: 0,
            color: ColorSpec::Static(1),
        }]);
        bytes.pop();
        assert_eq!(
            LightingSysex::parse(&bytes, MINI_MK3),
            Err(SysexError::NoTerminator)
        );
    }

    #[test]
    fn rejects_unknown_spec() {
        // F0 00 20 29 02 0D 03 0F 00 F7  — spec 0x0F is undefined
        let bytes = [0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x03, 0x0F, 0x00, 0xF7];
        assert!(matches!(
            LightingSysex::parse(&bytes, MINI_MK3),
            Err(SysexError::UnknownSpec(0x0F))
        ));
    }

    #[test]
    fn rejects_truncated_triplet() {
        // valid header, then spec 0x03 (RGB) followed by only 2 bytes of color
        let bytes = [
            0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x03, 0x03, 11, 1, 2, 0xF7,
        ];
        assert!(matches!(
            LightingSysex::parse(&bytes, MINI_MK3),
            Err(SysexError::Truncated(_))
        ));
    }

    #[test]
    fn looks_like_is_cheap_and_correct() {
        let bytes = lighting(vec![LedSpec {
            led_index: 0,
            color: ColorSpec::Static(1),
        }]);
        assert!(LightingSysex::looks_like(&bytes, MINI_MK3));
        assert!(!LightingSysex::looks_like(&bytes, LAUNCHPAD_X));
        assert!(!LightingSysex::looks_like(&[0xF0, 0xF7], MINI_MK3));
    }

    #[test]
    fn programmer_mode_select_is_not_lighting() {
        // Programmer/Live mode select message: F0 00 20 29 02 0D 0E 01 F7
        let bytes = [0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x0E, 0x01, 0xF7];
        assert!(!LightingSysex::looks_like(&bytes, MINI_MK3));
        assert_eq!(
            LightingSysex::parse(&bytes, MINI_MK3),
            Err(SysexError::NotLighting)
        );
    }

    proptest::proptest! {
        #[test]
        fn parser_does_not_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(0u8..=0xFFu8, 0..256)) {
            let _ = LightingSysex::parse(&bytes, MINI_MK3);
        }
    }
}
