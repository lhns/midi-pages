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

/// How to identify the physical device by its WinMM / CoreMIDI / ALSA port
/// name. Two shapes:
///
/// - `Simple(String)`: a substring that matches both the input and output
///   port name. Used when the device exposes both directions with names
///   that share a common substring.
/// - `Split { input, output }`: separate substrings for input and output
///   when the device names them completely differently.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PortMatch {
    Simple(String),
    Split {
        #[serde(rename = "in")]
        input: String,
        #[serde(rename = "out")]
        output: String,
    },
}

impl PortMatch {
    pub fn input(&self) -> &str {
        match self {
            PortMatch::Simple(s) => s,
            PortMatch::Split { input, .. } => input,
        }
    }

    pub fn output(&self) -> &str {
        match self {
            PortMatch::Simple(s) => s,
            PortMatch::Split { output, .. } => output,
        }
    }
}

impl std::fmt::Display for PortMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortMatch::Simple(s) => write!(f, "{s}"),
            PortMatch::Split { input, output } => {
                write!(f, "in={input:?}, out={output:?}")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    pub name: String,
    pub port_match: PortMatch,
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
    /// Defaults to a slug of `name`. The proxy creates ports named
    /// `<prefix>-page<N>` for N in 1..=pages.
    #[serde(default)]
    pub page_port_prefix: Option<String>,

    #[serde(default, deserialize_with = "deserialize_optional_sysex")]
    pub boot_sysex: Option<Vec<u8>>,

    /// Optional: physical button that advances to the next page when pressed.
    /// On press the LED briefly flashes (~200 ms) for visual feedback.
    #[serde(default)]
    pub next_page_button: Option<ButtonRef>,

    /// Optional: physical button that returns to the previous page.
    #[serde(default)]
    pub previous_page_button: Option<ButtonRef>,

    /// Optional: one button per page. Each entry can:
    /// - Specify the physical button via `kind` + `number` (required).
    /// - Optionally pin to a specific `page` index; entries without
    ///   `page` auto-assign in declaration order to the lowest free slot.
    /// - Optionally override `hold_to_preview` per-button; falls back to
    ///   `page_buttons_hold_to_preview` if unset.
    ///
    /// May be shorter than `pages`; extra entries (longer than `pages`)
    /// are rejected.
    #[serde(default)]
    pub page_buttons: Vec<PageButton>,

    /// Mode C: when true, page_buttons act as hold-to-preview switches —
    /// the persistent page is changed only via next/prev. When false (or
    /// page_buttons is empty), tapping a page button jumps persistently to
    /// that page (Mode B). Requires `page_buttons` to be non-empty.
    #[serde(default)]
    pub page_buttons_hold_to_preview: bool,

    /// Per-device page-button / next-prev LED colors. Every field is
    /// optional; missing values fall back to a generic active/inactive
    /// (lit/off) pair, with finer-grained fields cascading from those.
    #[serde(default)]
    pub colors: ColorConfig,

    /// Buttons pinned to page 0 regardless of the current page or any active
    /// preview. Pressing one always fires on page 0 (the "real" page from the
    /// host's perspective); LED writes the host sends to page 0 for these
    /// buttons always reach the device. Useful for a transport row, master
    /// mute, etc.
    #[serde(default)]
    pub global_buttons: Vec<ButtonRef>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ColorConfig {
    /// Tap page button on its persistent (active) page. Default fallback for
    /// `preview`, `active_cycle`, and `active_preview` when those are unset.
    #[serde(default)]
    pub active: Option<u8>,
    /// Tap page button on any other page. Default fallback for
    /// `inactive_cycle` and `inactive_preview`.
    #[serde(default)]
    pub inactive: Option<u8>,
    /// Page button currently held showing preview (Mode C). Defaults to `active`.
    #[serde(default)]
    pub preview: Option<u8>,
    /// next/previous_page_button while held. Defaults to `active`.
    #[serde(default)]
    pub active_cycle: Option<u8>,
    /// next/previous_page_button when idle. Defaults to `inactive`.
    #[serde(default)]
    pub inactive_cycle: Option<u8>,
    /// Hold-to-preview page button whose page is the persistent page (and the
    /// button is NOT currently held). Defaults to `active`.
    #[serde(default)]
    pub active_preview: Option<u8>,
    /// Hold-to-preview page button on any other page (not currently held).
    /// Defaults to `inactive`.
    #[serde(default)]
    pub inactive_preview: Option<u8>,
}

impl DeviceConfig {
    /// Effective per-page port prefix (auto-derived from `name` if not set).
    /// Kept short because Windows' WinMM MIDI device names are capped at
    /// 31 characters (`MIDIINCAPSW.szPname[32]`), so the full
    /// `<prefix>-page<N>` must fit. With the longest suffix `-page99` (7 chars)
    /// the prefix can be at most 24 chars.
    pub fn effective_prefix(&self) -> String {
        self.page_port_prefix
            .clone()
            .unwrap_or_else(|| slugify(&self.name))
    }

    /// Names of the N host-facing ports the proxy creates in `per_port` mode.
    /// Each name is one MIDI endpoint visible to the DAW.
    pub fn page_port_names(&self) -> Vec<String> {
        let prefix = self.effective_prefix();
        (1..=self.pages).map(|i| format!("{prefix}-page{i}")).collect()
    }

    pub fn note_offset_value(&self) -> u8 {
        self.note_offset.unwrap_or(64)
    }

    /// Resolve `page_buttons` to a list with definite page indices and
    /// hold flags. Walks the list in strict declaration order: each
    /// unconstrained entry claims the lowest free index at that point in
    /// the walk; entries with explicit `page` are taken as written. An
    /// explicit `page = N` that lands on a page an earlier auto-assigned
    /// entry already claimed is allowed; the two buttons then map to the
    /// same page (both light up, both jump or preview to that page).
    /// `hold` falls back to `page_buttons_hold_to_preview` when unset.
    ///
    /// Callers should run `Config::validate` first; this method assumes
    /// (and does not re-check) that explicit pages are `< pages` and
    /// pair-wise distinct, and that `page_buttons.len() <= pages`.
    pub fn resolved_page_buttons(&self) -> Vec<ResolvedPageButton> {
        let mut used: std::collections::HashSet<u8> = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(self.page_buttons.len());
        for pb in &self.page_buttons {
            let page = match pb.page {
                Some(p) => p,
                None => {
                    let mut candidate: u8 = 0;
                    while used.contains(&candidate) {
                        candidate = candidate.saturating_add(1);
                    }
                    candidate
                }
            };
            used.insert(page);
            let hold = pb
                .hold_to_preview
                .unwrap_or(self.page_buttons_hold_to_preview);
            out.push(ResolvedPageButton {
                button: pb.button,
                page,
                hold,
            });
        }
        out
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

impl ButtonRef {
    pub fn number(self) -> u8 {
        match self {
            ButtonRef::Note { number } | ButtonRef::Cc { number } => number,
        }
    }

}

/// A page-button entry from `[[device]].page_buttons`. Wraps a
/// `ButtonRef` with optional per-button page assignment and hold-mode
/// override. The `kind`/`number` fields are flattened so the TOML inline
/// table looks the same as before for the simple case.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct PageButton {
    #[serde(flatten)]
    pub button: ButtonRef,
    /// Pin this button to a specific page index. Auto-assigned to the
    /// lowest free index in declaration order when unset.
    #[serde(default)]
    pub page: Option<u8>,
    /// Override `page_buttons_hold_to_preview` for this button only.
    /// When unset, the device-level default applies.
    #[serde(default)]
    pub hold_to_preview: Option<bool>,
}

/// Page button after auto-assignment and `hold_to_preview` resolution.
/// Produced by `DeviceConfig::resolved_page_buttons` and stored on the
/// `Proxy` so the dispatch path doesn't re-resolve on every press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPageButton {
    pub button: ButtonRef,
    pub page: u8,
    pub hold: bool,
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
            // pages > 1 needs *some* navigation method.
            if d.pages > 1
                && d.next_page_button.is_none()
                && d.previous_page_button.is_none()
                && d.page_buttons.is_empty()
            {
                return Err(ConfigError::Invalid(format!(
                    "{}: pages > 1 requires at least one of next_page_button, \
                     previous_page_button, or page_buttons",
                    d.name
                )));
            }
            if let (Some(n), Some(p)) = (d.next_page_button, d.previous_page_button)
                && n == p
            {
                return Err(ConfigError::Invalid(format!(
                    "{}: next_page_button and previous_page_button must differ",
                    d.name
                )));
            }
            if d.page_buttons_hold_to_preview && d.page_buttons.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{}: page_buttons_hold_to_preview requires page_buttons to be non-empty",
                    d.name
                )));
            }
            if d.page_buttons.len() > d.pages as usize {
                return Err(ConfigError::Invalid(format!(
                    "{}: page_buttons has {} entries but pages = {}. Extras would be unreachable.",
                    d.name,
                    d.page_buttons.len(),
                    d.pages
                )));
            }
            // page_buttons button-refs must be unique among themselves and
            // not collide with next/prev.
            for (i, a) in d.page_buttons.iter().enumerate() {
                for b in &d.page_buttons[i + 1..] {
                    if a.button == b.button {
                        return Err(ConfigError::Invalid(format!(
                            "{}: page_buttons contains duplicate button entry",
                            d.name
                        )));
                    }
                }
                if Some(a.button) == d.next_page_button
                    || Some(a.button) == d.previous_page_button
                {
                    return Err(ConfigError::Invalid(format!(
                        "{}: page_buttons collides with next/previous_page_button",
                        d.name
                    )));
                }
            }
            // Explicit per-button `page` values must be < pages and
            // pair-wise distinct.
            let mut explicit_pages: std::collections::HashSet<u8> =
                std::collections::HashSet::new();
            for pb in &d.page_buttons {
                if let Some(p) = pb.page {
                    if p >= d.pages {
                        return Err(ConfigError::Invalid(format!(
                            "{}: page_buttons entry has page = {} but pages = {}.",
                            d.name, p, d.pages
                        )));
                    }
                    if !explicit_pages.insert(p) {
                        return Err(ConfigError::Invalid(format!(
                            "{}: two page_buttons entries pin to page = {}.",
                            d.name, p
                        )));
                    }
                }
            }
            // global_buttons: pair-wise unique and not colliding with any
            // page-nav button. No "addressable" check — for CCs especially,
            // non-grid CCs (top-row arrows, side strips) are legitimate
            // global targets and the proxy handles them as passthrough-to
            // -page-0.
            for (i, gb) in d.global_buttons.iter().enumerate() {
                for other in &d.global_buttons[i + 1..] {
                    if gb == other {
                        return Err(ConfigError::Invalid(format!(
                            "{}: duplicate global_buttons entry {gb:?}",
                            d.name
                        )));
                    }
                }
                if Some(*gb) == d.next_page_button || Some(*gb) == d.previous_page_button {
                    return Err(ConfigError::Invalid(format!(
                        "{}: global_buttons entry {gb:?} collides with next/previous_page_button",
                        d.name
                    )));
                }
                if d.page_buttons.iter().any(|pb| pb.button == *gb) {
                    return Err(ConfigError::Invalid(format!(
                        "{}: global_buttons entry {gb:?} collides with a page_buttons entry",
                        d.name
                    )));
                }
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
                    for gb in &d.global_buttons {
                        if gb.number() >= n {
                            return Err(ConfigError::Invalid(format!(
                                "{}: in mode=note_offset, global_buttons entry {gb:?} has \
                                 number {} >= note_offset {}; globals must live in page 0's range.",
                                d.name,
                                gb.number(),
                                n
                            )));
                        }
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
                            "{}: mode=per_port does not use host_port_in / host_port_out. \
                             Remove them or set mode=note_offset.",
                            d.name
                        )));
                    }
                    // WinMM caps MIDI device names at 31 chars (MIDIINCAPSW.szPname[32]).
                    // Anything longer gets truncated and becomes unfindable on Windows.
                    if let Some(longest) = d
                        .page_port_names()
                        .into_iter()
                        .max_by_key(|s| s.len())
                        && longest.len() > 31
                    {
                        return Err(ConfigError::Invalid(format!(
                            "{}: generated port name `{longest}` is {} chars; Windows truncates \
                             MIDI device names at 31 chars. Set a shorter `page_port_prefix` \
                             in this device section.",
                            d.name,
                            longest.len()
                        )));
                    }
                }
            }
        }
        for (i, a) in self.devices.iter().enumerate() {
            for b in &self.devices[i + 1..] {
                if a.port_match.input() == b.port_match.input() {
                    return Err(ConfigError::Invalid(format!(
                        "duplicate input port_match `{}` between `{}` and `{}`",
                        a.port_match.input(),
                        a.name,
                        b.name
                    )));
                }
                if a.port_match.output() == b.port_match.output() {
                    return Err(ConfigError::Invalid(format!(
                        "duplicate output port_match `{}` between `{}` and `{}`",
                        a.port_match.output(),
                        a.name,
                        b.name
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
name                 = "Mini"
port_match           = "Launchpad Mini"
mode                 = "note_offset"
host_port_in         = "host-in"
host_port_out        = "host-out"
driver               = "mini_mk3"
pages                = 2
note_offset          = 64
boot_sysex           = "F0 00 20 29 02 0D 0E 01 F7"
next_page_button     = { kind = "cc", number = 91 }
previous_page_button = { kind = "cc", number = 92 }
"#;

    const VALID_PER_PORT: &str = r#"
[[device]]
name                 = "Launchpad Mini MK3"
port_match           = "Launchpad Mini"
driver               = "mini_mk3"
pages                = 4
next_page_button     = { kind = "cc", number = 91 }
previous_page_button = { kind = "cc", number = 92 }
"#;

    #[test]
    fn parses_simple_port_match() {
        let cfg: Config = toml::from_str(VALID_PER_PORT).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.devices[0].port_match,
            PortMatch::Simple("Launchpad Mini".into())
        );
        assert_eq!(cfg.devices[0].port_match.input(), "Launchpad Mini");
        assert_eq!(cfg.devices[0].port_match.output(), "Launchpad Mini");
    }

    #[test]
    fn parses_split_port_match() {
        let src = r#"
[[device]]
name                 = "Weird"
port_match           = { in = "Weird IN", out = "Weird OUT" }
driver               = "mini_mk3"
pages                = 1
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].port_match.input(), "Weird IN");
        assert_eq!(cfg.devices[0].port_match.output(), "Weird OUT");
    }

    #[test]
    fn duplicate_port_match_split_vs_simple_rejected() {
        // Simple "X" and Split { in = "X", out = "Y" } collide on input.
        let src = r#"
[[device]]
name        = "A"
port_match  = "X"
driver      = "mini_mk3"
pages       = 1

[[device]]
name        = "B"
port_match  = { in = "X", out = "Y" }
driver      = "mini_mk3"
pages       = 1
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("duplicate input"), "{s}");
    }

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
        assert_eq!(names, vec![
            "launchpad-mini-mk3-page1",
            "launchpad-mini-mk3-page2",
            "launchpad-mini-mk3-page3",
            "launchpad-mini-mk3-page4",
        ]);
    }

    #[test]
    fn rejects_zero_pages() {
        let bad = VALID_NOTE_OFFSET.replace("pages                = 2", "pages                = 0");
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_same_button_for_next_and_previous() {
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
        let bad = VALID_NOTE_OFFSET.replace("pages                = 2", "pages                = 4");
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
        let many = VALID_PER_PORT.replace("pages                = 4", "pages                = 16");
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
            .replace("host_port_in         = \"host-in\"\n", "")
            .replace("host_port_out        = \"host-out\"\n", "");
        let cfg: Config = toml::from_str(&bad).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("host_port_in"), "{s}");
    }

    #[test]
    fn rejects_page_button_collision_with_next_prev() {
        let bad =
            VALID_NOTE_OFFSET.to_string() + "page_buttons = [ { kind = \"cc\", number = 91 } ]\n";
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_hold_to_preview_without_page_buttons() {
        let bad = VALID_PER_PORT.to_string() + "page_buttons_hold_to_preview = true\n";
        let cfg: Config = toml::from_str(&bad).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("page_buttons_hold_to_preview"), "{s}");
    }

    #[test]
    fn rejects_no_page_navigation_when_pages_gt_1() {
        let bad = r#"
[[device]]
name        = "Mini"
port_match  = "Launchpad Mini"
driver      = "mini_mk3"
pages       = 4
"#;
        let cfg: Config = toml::from_str(bad).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("pages > 1 requires"), "{s}");
    }

    #[test]
    fn allows_no_navigation_when_pages_eq_1() {
        let single = r#"
[[device]]
name        = "Mini"
port_match  = "Launchpad Mini"
driver      = "mini_mk3"
pages       = 1
"#;
        let cfg: Config = toml::from_str(single).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn parses_all_three_modes() {
        // Mode A: only next/prev.
        let a = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 4
next_page_button     = { kind = "cc", number = 91 }
previous_page_button = { kind = "cc", number = 92 }
"#;
        toml::from_str::<Config>(a).unwrap().validate().unwrap();

        // Mode B: page_buttons (no hold).
        let b = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 4
page_buttons = [
  { kind = "cc", number = 89 },
  { kind = "cc", number = 79 },
  { kind = "cc", number = 69 },
  { kind = "cc", number = 59 },
]
"#;
        toml::from_str::<Config>(b).unwrap().validate().unwrap();

        // Mode C: page_buttons + hold + optional next/prev.
        let c = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 4
next_page_button     = { kind = "cc", number = 91 }
previous_page_button = { kind = "cc", number = 92 }
page_buttons = [
  { kind = "cc", number = 89 },
  { kind = "cc", number = 79 },
  { kind = "cc", number = 69 },
  { kind = "cc", number = 59 },
]
page_buttons_hold_to_preview = true
"#;
        toml::from_str::<Config>(c).unwrap().validate().unwrap();
    }

    #[test]
    fn parses_optional_colors_block() {
        let with = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
colors = { active = 99, preview = 13 }
"#;
        let cfg: Config = toml::from_str(with).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].colors.active, Some(99));
        assert_eq!(cfg.devices[0].colors.inactive, None);
        assert_eq!(cfg.devices[0].colors.preview, Some(13));

        // Same config without the block — all None.
        let without = with.replace("colors = { active = 99, preview = 13 }\n", "");
        let cfg2: Config = toml::from_str(&without).unwrap();
        cfg2.validate().unwrap();
        assert_eq!(cfg2.devices[0].colors, ColorConfig::default());
    }

    #[test]
    fn parses_all_new_color_fields() {
        let src = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
[device.colors]
active = 21
inactive = 1
preview = 13
active_cycle = 30
inactive_cycle = 31
active_preview = 45
inactive_preview = 46
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let c = &cfg.devices[0].colors;
        assert_eq!(c.active, Some(21));
        assert_eq!(c.inactive, Some(1));
        assert_eq!(c.preview, Some(13));
        assert_eq!(c.active_cycle, Some(30));
        assert_eq!(c.inactive_cycle, Some(31));
        assert_eq!(c.active_preview, Some(45));
        assert_eq!(c.inactive_preview, Some(46));
    }

    #[test]
    fn parses_global_buttons() {
        let src = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
global_buttons = [
  { kind = "note", number = 11 },
  { kind = "note", number = 18 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].global_buttons.len(), 2);
    }

    #[test]
    fn accepts_non_grid_cc_as_global_button() {
        // Top-row arrows on Mini MK3 are CCs that `is_grid_cc` returns false
        // for; they're still legitimate global targets (passthrough to page 0).
        let src = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 2
next_page_button = { kind = "cc", number = 91 }
global_buttons = [ { kind = "cc", number = 19 }, { kind = "cc", number = 95 } ]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_duplicate_global_buttons() {
        let src = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 2
next_page_button = { kind = "cc", number = 91 }
global_buttons = [
  { kind = "note", number = 11 },
  { kind = "note", number = 11 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("duplicate global_buttons"), "{s}");
    }

    #[test]
    fn rejects_global_button_collides_with_page_button() {
        let src = r#"
[[device]]
name = "Mini"
port_match = "Launchpad Mini"
driver = "mini_mk3"
pages = 2
next_page_button = { kind = "cc", number = 91 }
page_buttons = [ { kind = "note", number = 11 } ]
global_buttons = [ { kind = "note", number = 11 } ]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("collides with a page_buttons"), "{s}");
    }

    #[test]
    fn rejects_global_button_outside_note_offset_range() {
        let src = r#"
[[device]]
name = "APC"
port_match = "APC MINI"
driver = "apc_mini"
mode = "note_offset"
host_port_in = "in"
host_port_out = "out"
pages = 2
note_offset = 32
next_page_button = { kind = "note", number = 98 }
global_buttons = [ { kind = "note", number = 40 } ]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("globals must live in page 0"), "{s}");
    }

    #[test]
    fn rejects_too_many_page_buttons() {
        let bad = VALID_PER_PORT.to_string()
            + "page_buttons = [
              { kind = \"cc\", number = 89 },
              { kind = \"cc\", number = 79 },
              { kind = \"cc\", number = 69 },
              { kind = \"cc\", number = 59 },
              { kind = \"cc\", number = 49 },
            ]\n";
        let cfg: Config = toml::from_str(&bad).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("entries but pages"), "{s}");
    }

    #[test]
    fn parses_page_buttons_with_explicit_page_field() {
        let src = r#"
[[device]]
name = "M"
port_match = "P"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
page_buttons = [
  { kind = "cc", number = 89, page = 3 },
  { kind = "cc", number = 79 },
  { kind = "cc", number = 69, hold_to_preview = true },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.devices[0].resolved_page_buttons();
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].page, 3);
        assert!(!resolved[0].hold);
        assert_eq!(resolved[1].page, 0);
        assert!(!resolved[1].hold);
        assert_eq!(resolved[2].page, 1);
        assert!(resolved[2].hold);
    }

    #[test]
    fn auto_assigns_lowest_free_page_in_declaration_order() {
        // Walk declaration order: entry 0 explicit 2; entry 1 unconstrained
        // claims 0 (lowest not in `used={2}`); entry 2 claims 1; entry 3
        // explicit 0 collides with entry 1's earlier claim. Both buttons
        // end up on page 0. This is allowed per the documented behaviour
        // (auto-then-explicit collisions are intentional).
        let src = r#"
[[device]]
name = "M"
port_match = "P"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
page_buttons = [
  { kind = "cc", number = 89, page = 2 },
  { kind = "cc", number = 79 },
  { kind = "cc", number = 69 },
  { kind = "cc", number = 59, page = 0 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.devices[0].resolved_page_buttons();
        assert_eq!(
            resolved.iter().map(|pb| pb.page).collect::<Vec<_>>(),
            vec![2, 0, 1, 0],
            "declaration-order walk; explicit page = 0 at the end shares page 0 with the first auto-assigned"
        );
    }

    #[test]
    fn auto_then_explicit_can_collide() {
        let src = r#"
[[device]]
name = "M"
port_match = "P"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
page_buttons = [
  { kind = "cc", number = 89 },
  { kind = "cc", number = 79, page = 0 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.devices[0].resolved_page_buttons();
        assert_eq!(
            resolved[0].page, 0,
            "unconstrained entry claims page 0 in declaration order"
        );
        assert_eq!(
            resolved[1].page, 0,
            "explicit page = 0 is taken as written; two buttons share page 0"
        );
    }

    #[test]
    fn rejects_duplicate_explicit_pages() {
        let src = r#"
[[device]]
name = "M"
port_match = "P"
driver = "mini_mk3"
pages = 4
next_page_button = { kind = "cc", number = 91 }
page_buttons = [
  { kind = "cc", number = 89, page = 2 },
  { kind = "cc", number = 79, page = 2 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("pin to page = 2"), "{s}");
    }

    #[test]
    fn rejects_page_out_of_range() {
        let src = r#"
[[device]]
name = "M"
port_match = "P"
driver = "mini_mk3"
pages = 2
next_page_button = { kind = "cc", number = 91 }
page_buttons = [
  { kind = "cc", number = 89, page = 5 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        let err = cfg.validate().unwrap_err();
        let ConfigError::Invalid(s) = err else { panic!("wrong err type") };
        assert!(s.contains("page = 5"), "{s}");
    }

    #[test]
    fn per_button_hold_to_preview_overrides_global() {
        let src = r#"
[[device]]
name = "M"
port_match = "P"
driver = "mini_mk3"
pages = 3
next_page_button = { kind = "cc", number = 91 }
page_buttons_hold_to_preview = true
page_buttons = [
  { kind = "cc", number = 89, hold_to_preview = false },
  { kind = "cc", number = 79 },
]
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.devices[0].resolved_page_buttons();
        assert!(!resolved[0].hold, "explicit per-button false overrides global true");
        assert!(resolved[1].hold, "missing per-button defaults to global true");
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
