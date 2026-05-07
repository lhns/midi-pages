//! TOML configuration schema and validation.

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

use crate::midi::device::Driver;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(rename = "device")]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    pub name: String,
    pub port_match: String,
    pub host_port_in: String,
    pub host_port_out: String,
    pub driver: Driver,
    pub pages: u8,
    #[serde(default = "default_offset")]
    pub note_offset: u8,
    #[serde(default, deserialize_with = "deserialize_optional_sysex")]
    pub boot_sysex: Option<Vec<u8>>,
    pub page_up_button: ButtonRef,
    pub page_down_button: ButtonRef,
    #[serde(default)]
    pub indicator_leds: Vec<ButtonRef>,
}

fn default_offset() -> u8 {
    64
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ButtonRef {
    Note { number: u8 },
    Cc { number: u8 },
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.devices.is_empty() {
            return Err(ConfigError::Invalid(
                "no [[device]] sections defined".into(),
            ));
        }
        for d in &self.devices {
            if d.pages == 0 {
                return Err(ConfigError::Invalid(format!(
                    "{}: pages must be >= 1",
                    d.name
                )));
            }
            if d.note_offset == 0 {
                return Err(ConfigError::Invalid(format!(
                    "{}: note_offset must be >= 1",
                    d.name
                )));
            }
            if d.page_up_button == d.page_down_button {
                return Err(ConfigError::Invalid(format!(
                    "{}: page_up_button and page_down_button must differ",
                    d.name
                )));
            }
            if d.indicator_leds.contains(&d.page_up_button)
                || d.indicator_leds.contains(&d.page_down_button)
            {
                return Err(ConfigError::Invalid(format!(
                    "{}: indicator_leds collides with a page button",
                    d.name
                )));
            }
            // MIDI notes / CCs / SysEx LED indices are 7-bit (0..=127). The
            // highest logical address we ever produce is `(pages-1)*note_offset
            // + max_grid_index`. We don't know `max_grid_index` here without
            // device knowledge, so we require the conservative bound that
            // `pages * note_offset <= 128`. Per ADR 0003.
            let span = (d.pages as u16) * (d.note_offset as u16);
            if span > 128 {
                return Err(ConfigError::Invalid(format!(
                    "{}: pages*note_offset = {} exceeds the 128-value MIDI range. \
                     Reduce pages or note_offset. See docs/adr/0003-note-offset-paging.md.",
                    d.name, span
                )));
            }
        }
        // Detect overlapping port_match substrings.
        for (i, a) in self.devices.iter().enumerate() {
            for b in &self.devices[i + 1..] {
                if a.port_match == b.port_match {
                    return Err(ConfigError::Invalid(format!(
                        "duplicate port_match `{}` between `{}` and `{}`",
                        a.port_match, a.name, b.name
                    )));
                }
            }
        }
        Ok(())
    }
}

fn deserialize_optional_sysex<'de, D>(de: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(de)?;
    s.map(|s| parse_hex(&s).map_err(serde::de::Error::custom))
        .transpose()
}

fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    s.split_whitespace()
        .map(|t| u8::from_str_radix(t.trim_start_matches("0x"), 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(text: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f
    }

    const VALID: &str = r#"
[[device]]
name             = "Mini"
port_match       = "Launchpad Mini"
host_port_in     = "host-in"
host_port_out    = "host-out"
driver           = "mini_mk3"
pages            = 2
boot_sysex       = "F0 00 20 29 02 0D 0E 01 F7"
page_up_button   = { kind = "cc", number = 91 }
page_down_button = { kind = "cc", number = 92 }
"#;

    #[test]
    fn parses_valid_minimum() {
        let cfg: Config = toml::from_str(VALID).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices.len(), 1);
        assert_eq!(cfg.devices[0].pages, 2);
        assert_eq!(cfg.devices[0].note_offset, 64);
        assert_eq!(
            cfg.devices[0].boot_sysex.as_deref().map(|s| s.len()),
            Some(9)
        );
    }

    #[test]
    fn rejects_zero_pages() {
        let bad = VALID.replace("pages            = 2", "pages            = 0");
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_same_button_for_up_and_down() {
        let bad = VALID.replace("number = 92", "number = 91");
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_overlapping_port_match() {
        let two = format!("{0}\n{0}", VALID);
        let cfg: Config = toml::from_str(&two).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_excessive_pages_for_midi_range() {
        // 4 pages * 64 = 256 > 128: must be rejected.
        let bad = VALID.replace("pages            = 2", "pages            = 4");
        let cfg: Config = toml::from_str(&bad).unwrap();
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigError::Invalid(s) => assert!(s.contains("MIDI range")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_indicator_collision() {
        let bad = VALID.to_string() + "indicator_leds = [ { kind = \"cc\", number = 91 } ]\n";
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn loads_from_disk() {
        let f = write_tmp(VALID);
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.devices[0].name, "Mini");
    }

    #[test]
    fn parses_hex_sysex() {
        assert_eq!(
            parse_hex("F0 00 20 29 F7").unwrap(),
            vec![0xF0, 0x00, 0x20, 0x29, 0xF7]
        );
    }
}
