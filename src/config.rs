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

/// Two ways the proxy can present pages to the host.
///
/// - `NoteOffset`: one virtual port pair; pages encoded by adding `note_offset`
///   to the note number. Capped by MIDI's 7-bit range (`pages * note_offset <= 128`).
/// - `PerPort`: N virtual port pairs, one per page. Each page presents an identical
///   layout. No note math, no SysEx rewriting, no MIDI-range ceiling.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    PerPort,
    NoteOffset,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    pub name: String,
    pub port_match: String,
    pub driver: Driver,
    pub pages: u8,
    #[serde(default)]
    pub mode: Mode,

    /// Used in `note_offset` mode: the single host port the proxy reads from.
    #[serde(default)]
    pub host_port_in: Option<String>,
    /// Used in `note_offset` mode: the single host port the proxy writes to.
    #[serde(default)]
    pub host_port_out: Option<String>,
    /// Used in `note_offset` mode: per-page note shift.
    #[serde(default)]
    pub note_offset: Option<u8>,

    /// Used in `per_port` mode: prefix for auto-generated port names.
    /// Defaults to a slug of `name`. The proxy will look for ports named
    /// `<prefix>-page<N>-in` and `<prefix>-page<N>-out` for N in 1..=pages.
    #[serde(default)]
    pub page_port_prefix: Option<String>,

    #[serde(default, deserialize_with = "deserialize_optional_sysex")]
    pub boot_sysex: Option<Vec<u8>>,
    pub page_up_button: ButtonRef,
    pub page_down_button: ButtonRef,
    #[serde(default)]
    pub indicator_leds: Vec<ButtonRef>,
}

impl DeviceConfig {
    /// Effective per-page port prefix (auto-derived from `name` if not set).
    pub fn effective_prefix(&self) -> String {
        self.page_port_prefix
            .clone()
            .unwrap_or_else(|| format!("midi-pages-{}", slugify(&self.name)))
    }

    /// Names of the N host-side ports the proxy expects to find in `per_port` mode.
    /// Returns a `(in_name, out_name)` pair per page (loopMIDI side).
    pub fn page_port_names(&self) -> Vec<(String, String)> {
        let prefix = self.effective_prefix();
        (1..=self.pages)
            .map(|i| {
                (
                    format!("{prefix}-page{i}-in"),
                    format!("{prefix}-page{i}-out"),
                )
            })
            .collect()
    }

    pub fn note_offset_value(&self) -> u8 {
        self.note_offset.unwrap_or(64)
    }
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
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
            match d.mode {
                Mode::NoteOffset => {
                    if d.host_port_in.is_none() || d.host_port_out.is_none() {
                        return Err(ConfigError::Invalid(format!(
                            "{}: mode=note_offset requires host_port_in and host_port_out",
                            d.name
                        )));
                    }
                    let n = d.note_offset_value();
                    if n == 0 {
                        return Err(ConfigError::Invalid(format!(
                            "{}: note_offset must be >= 1",
                            d.name
                        )));
                    }
                    let span = (d.pages as u16) * (n as u16);
                    if span > 128 {
                        return Err(ConfigError::Invalid(format!(
                            "{}: pages*note_offset = {} exceeds the 128-value MIDI range. \
                             Reduce pages or note_offset, or switch to mode=per_port. \
                             See docs/adr/0003-note-offset-paging.md.",
                            d.name, span
                        )));
                    }
                }
                Mode::PerPort => {
                    if d.host_port_in.is_some() || d.host_port_out.is_some() {
                        return Err(ConfigError::Invalid(format!(
                            "{}: mode=per_port does not use host_port_in / host_port_out — \
                             remove them or set mode=note_offset",
                            d.name
                        )));
                    }
                }
            }
        }
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

    const VALID_NOTE_OFFSET: &str = r#"
[[device]]
name             = "Mini"
port_match       = "Launchpad Mini"
mode             = "note_offset"
host_port_in     = "host-in"
host_port_out    = "host-out"
driver           = "mini_mk3"
pages            = 2
note_offset      = 64
boot_sysex       = "F0 00 20 29 02 0D 0E 01 F7"
page_up_button   = { kind = "cc", number = 91 }
page_down_button = { kind = "cc", number = 92 }
"#;

    const VALID_PER_PORT: &str = r#"
[[device]]
name             = "Launchpad Mini MK3"
port_match       = "Launchpad Mini"
driver           = "mini_mk3"
pages            = 4
page_up_button   = { kind = "cc", number = 91 }
page_down_button = { kind = "cc", number = 92 }
"#;

    #[test]
    fn parses_note_offset() {
        let cfg: Config = toml::from_str(VALID_NOTE_OFFSET).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].mode, Mode::NoteOffset);
        assert_eq!(cfg.devices[0].note_offset_value(), 64);
    }

    #[test]
    fn parses_per_port_with_defaults() {
        let cfg: Config = toml::from_str(VALID_PER_PORT).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].mode, Mode::PerPort);
        assert_eq!(cfg.devices[0].pages, 4);

        let names = cfg.devices[0].page_port_names();
        assert_eq!(names.len(), 4);
        assert_eq!(
            names[0],
            (
                "midi-pages-launchpad-mini-mk3-page1-in".into(),
                "midi-pages-launchpad-mini-mk3-page1-out".into()
            )
        );
    }

    #[test]
    fn rejects_zero_pages() {
        let bad = VALID_NOTE_OFFSET.replace("pages            = 2", "pages            = 0");
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_same_button_for_up_and_down() {
        let bad = VALID_NOTE_OFFSET.replace("number = 92", "number = 91");
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_overlapping_port_match() {
        let two = format!("{0}\n{0}", VALID_PER_PORT);
        let cfg: Config = toml::from_str(&two).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_excessive_pages_for_midi_range() {
        // 4 pages * 64 = 256 > 128 in note_offset mode: must be rejected.
        let bad = VALID_NOTE_OFFSET.replace("pages            = 2", "pages            = 4");
        let cfg: Config = toml::from_str(&bad).unwrap();
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigError::Invalid(s) => assert!(s.contains("MIDI range")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn per_port_mode_allows_many_pages() {
        // What note_offset rejects, per_port allows.
        let many = VALID_PER_PORT.replace("pages            = 4", "pages            = 16");
        let cfg: Config = toml::from_str(&many).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].page_port_names().len(), 16);
    }

    #[test]
    fn rejects_per_port_with_host_port_set() {
        let bad = VALID_PER_PORT.to_string() + "host_port_in = \"x\"\nhost_port_out = \"y\"\n";
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_note_offset_without_host_ports() {
        let bad = VALID_NOTE_OFFSET
            .replace("host_port_in     = \"host-in\"\n", "")
            .replace("host_port_out    = \"host-out\"\n", "");
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_indicator_collision() {
        let bad =
            VALID_NOTE_OFFSET.to_string() + "indicator_leds = [ { kind = \"cc\", number = 91 } ]\n";
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn loads_from_disk() {
        let f = write_tmp(VALID_PER_PORT);
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.devices[0].mode, Mode::PerPort);
    }

    #[test]
    fn parses_hex_sysex() {
        assert_eq!(
            parse_hex("F0 00 20 29 F7").unwrap(),
            vec![0xF0, 0x00, 0x20, 0x29, 0xF7]
        );
    }

    #[test]
    fn slugify_handles_spaces_and_punctuation() {
        assert_eq!(slugify("Launchpad Mini MK3"), "launchpad-mini-mk3");
        assert_eq!(slugify("APC mini!"), "apc-mini");
        assert_eq!(slugify("multi   spaces"), "multi-spaces");
    }
}
