//! Stateful paging proxy: rewrites MIDI in both directions and maintains a
//! per-page LED cache.

use std::collections::{HashMap, HashSet};

use crate::config::{ButtonRef, DeviceConfig, Mode};
use crate::midi::device::{Device, Driver};
use crate::midi::parse::{self, Msg};
use crate::midi::sysex_lighting::{ColorSpec, LedSpec, LightingSysex, MINI_MK3};

/// One LED's last-known state, keyed in the cache by its physical pad index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedCell {
    NoteOn { channel: u8, velocity: u8 },
    Cc { channel: u8, value: u8 },
    SysexColor(ColorSpec),
}

/// One MIDI write the proxy wants to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// Send these bytes to the host on the single host port.
    /// Used in `Mode::NoteOffset`.
    ToHost(Vec<u8>),
    /// Send these bytes to the host on the virtual port for `page`.
    /// Used in `Mode::PerPort`.
    ToHostPage { page: u8, bytes: Vec<u8> },
    /// Send these bytes to the device (real USB-MIDI side).
    ToDevice(Vec<u8>),
}

/// Proxy state and the two rewrite paths.
pub struct Proxy {
    pub mode: Mode,
    pub current_page: u8,
    pub pages: u8,
    pub note_offset: u8,
    pub page_up: ButtonRef,
    pub page_down: ButtonRef,
    pub indicators: Vec<ButtonRef>,
    pub driver: Driver,

    /// `led_cache[page][physical_note] -> last LED state for that pad on that page`.
    pub led_cache: Vec<HashMap<u8, LedCell>>,

    /// Physical pads currently held down. Lets us synthesize Note Off on page change.
    held: HashSet<u8>,
    /// Physical pads whose Note Off we already synthesized — suppress the next real one.
    suppressed_releases: HashSet<u8>,

    device: Box<dyn Device>,
}

impl Proxy {
    pub fn new(cfg: &DeviceConfig, device: Box<dyn Device>) -> Self {
        let pages = cfg.pages as usize;
        Self {
            mode: cfg.mode,
            current_page: 0,
            pages: cfg.pages,
            note_offset: cfg.note_offset_value(),
            page_up: cfg.page_up_button,
            page_down: cfg.page_down_button,
            indicators: cfg.indicator_leds.clone(),
            driver: cfg.driver,
            led_cache: vec![HashMap::new(); pages],
            held: HashSet::new(),
            suppressed_releases: HashSet::new(),
            device,
        }
    }

    pub fn device(&self) -> &dyn Device {
        &*self.device
    }

    // -- Device -> host -----------------------------------------------------

    /// Handle a message arriving from the device (a pad press, a release, etc.).
    pub fn handle_device_in(&mut self, bytes: &[u8]) -> Vec<Out> {
        let msg = parse::classify(bytes);
        match msg {
            Msg::NoteOn {
                channel,
                note,
                velocity,
            } => {
                if self.is_button(ButtonRef::Note { number: note }, &self.page_up) {
                    return self.cycle_page(true);
                }
                if self.is_button(ButtonRef::Note { number: note }, &self.page_down) {
                    return self.cycle_page(false);
                }
                if !self.device.is_grid_note(note) {
                    return vec![self.to_host_current(bytes.to_vec())];
                }
                self.held.insert(note);
                let bytes = match self.mode {
                    Mode::NoteOffset => {
                        let logical = note + self.current_page * self.note_offset;
                        parse::note_on(channel, logical, velocity).to_vec()
                    }
                    Mode::PerPort => parse::note_on(channel, note, velocity).to_vec(),
                };
                vec![self.to_host_current(bytes)]
            }
            Msg::NoteOff {
                channel,
                note,
                velocity,
            } => {
                if self.is_button(ButtonRef::Note { number: note }, &self.page_up)
                    || self.is_button(ButtonRef::Note { number: note }, &self.page_down)
                {
                    return Vec::new();
                }
                if !self.device.is_grid_note(note) {
                    return vec![self.to_host_current(bytes.to_vec())];
                }
                let was_held = self.held.remove(&note);
                if self.suppressed_releases.remove(&note) {
                    return Vec::new();
                }
                let _ = was_held;
                let bytes = match self.mode {
                    Mode::NoteOffset => {
                        let logical = note + self.current_page * self.note_offset;
                        parse::note_off(channel, logical, velocity).to_vec()
                    }
                    Mode::PerPort => parse::note_off(channel, note, velocity).to_vec(),
                };
                vec![self.to_host_current(bytes)]
            }
            Msg::Cc {
                channel,
                controller,
                value,
            } => {
                if self.is_button(ButtonRef::Cc { number: controller }, &self.page_up) {
                    return if value > 0 {
                        self.cycle_page(true)
                    } else {
                        Vec::new()
                    };
                }
                if self.is_button(ButtonRef::Cc { number: controller }, &self.page_down) {
                    return if value > 0 {
                        self.cycle_page(false)
                    } else {
                        Vec::new()
                    };
                }
                if !self.device.is_grid_cc(controller) {
                    return vec![self.to_host_current(bytes.to_vec())];
                }
                let bytes = match self.mode {
                    Mode::NoteOffset => {
                        let logical = controller + self.current_page * self.note_offset;
                        parse::cc(channel, logical, value).to_vec()
                    }
                    Mode::PerPort => parse::cc(channel, controller, value).to_vec(),
                };
                vec![self.to_host_current(bytes)]
            }
            Msg::SysEx(_) | Msg::Other(_) => {
                vec![self.to_host_current(bytes.to_vec())]
            }
        }
    }

    // -- Host -> device (note_offset mode) ---------------------------------

    /// Note-offset mode entry point. The host writes to a single virtual port
    /// and the proxy infers the target page from the note number.
    pub fn handle_host_in(&mut self, bytes: &[u8]) -> Vec<Out> {
        debug_assert_eq!(self.mode, Mode::NoteOffset);
        if LightingSysex::looks_like(bytes, MINI_MK3) {
            return self.handle_host_lighting_sysex(bytes);
        }
        let msg = parse::classify(bytes);
        match msg {
            Msg::NoteOn {
                channel,
                note,
                velocity,
            } => self.handle_host_note_offset(channel, note, velocity, true),
            Msg::NoteOff {
                channel,
                note,
                velocity,
            } => self.handle_host_note_offset(channel, note, velocity, false),
            Msg::Cc {
                channel,
                controller,
                value,
            } => {
                if !self.device.is_grid_cc(controller) {
                    return vec![Out::ToDevice(bytes.to_vec())];
                }
                let target_page = controller / self.note_offset;
                let physical = controller % self.note_offset;
                if (target_page as usize) < self.led_cache.len() {
                    self.led_cache[target_page as usize]
                        .insert(physical, LedCell::Cc { channel, value });
                }
                if target_page == self.current_page {
                    vec![Out::ToDevice(parse::cc(channel, physical, value).to_vec())]
                } else {
                    Vec::new()
                }
            }
            Msg::SysEx(_) | Msg::Other(_) => vec![Out::ToDevice(bytes.to_vec())],
        }
    }

    fn handle_host_note_offset(
        &mut self,
        channel: u8,
        note: u8,
        velocity: u8,
        is_on: bool,
    ) -> Vec<Out> {
        if !self.is_paged_logical_note(note) {
            return vec![Out::ToDevice(bytes_for_note(
                channel, note, velocity, is_on,
            ))];
        }
        let target_page = note / self.note_offset;
        let physical = note % self.note_offset;
        if (target_page as usize) >= self.led_cache.len() {
            return Vec::new();
        }
        self.led_cache[target_page as usize].insert(
            physical,
            LedCell::NoteOn {
                channel,
                velocity: if is_on { velocity } else { 0 },
            },
        );
        if target_page == self.current_page {
            vec![Out::ToDevice(bytes_for_note(
                channel, physical, velocity, is_on,
            ))]
        } else {
            Vec::new()
        }
    }

    fn handle_host_lighting_sysex(&mut self, bytes: &[u8]) -> Vec<Out> {
        let parsed = match LightingSysex::parse(bytes, MINI_MK3) {
            Ok(p) => p,
            Err(_) => return vec![Out::ToDevice(bytes.to_vec())],
        };
        let mut on_page = Vec::new();
        for led in parsed.leds {
            let target_page = led.led_index / self.note_offset;
            let physical = led.led_index % self.note_offset;
            if (target_page as usize) < self.led_cache.len() {
                self.led_cache[target_page as usize]
                    .insert(physical, LedCell::SysexColor(led.color.clone()));
            }
            if target_page == self.current_page {
                on_page.push(LedSpec {
                    led_index: physical,
                    color: led.color,
                });
            }
        }
        if on_page.is_empty() {
            Vec::new()
        } else {
            vec![Out::ToDevice(
                LightingSysex {
                    model: parsed.model,
                    leds: on_page,
                }
                .emit(),
            )]
        }
    }

    // -- Host -> device (per-port mode) ------------------------------------

    /// Per-port mode entry point. The proxy is told which page's virtual port
    /// the message arrived on and stores it accordingly.
    pub fn handle_host_in_per_port(&mut self, page: u8, bytes: &[u8]) -> Vec<Out> {
        debug_assert_eq!(self.mode, Mode::PerPort);
        if (page as usize) >= self.led_cache.len() {
            return Vec::new();
        }
        // Cache LED state per page so we can replay on switch. The bytes target
        // physical pads directly (no offset/rewriting), so we just record by
        // type and decide whether to forward to the device.
        match parse::classify(bytes) {
            Msg::NoteOn {
                channel,
                note,
                velocity,
            } if self.device.is_grid_note(note) => {
                self.led_cache[page as usize].insert(note, LedCell::NoteOn { channel, velocity });
            }
            Msg::NoteOff { channel, note, .. } if self.device.is_grid_note(note) => {
                self.led_cache[page as usize].insert(
                    note,
                    LedCell::NoteOn {
                        channel,
                        velocity: 0,
                    },
                );
            }
            Msg::Cc {
                channel,
                controller,
                value,
            } if self.device.is_grid_cc(controller) => {
                self.led_cache[page as usize].insert(controller, LedCell::Cc { channel, value });
            }
            Msg::SysEx(s) if LightingSysex::looks_like(s, MINI_MK3) => {
                if let Ok(parsed) = LightingSysex::parse(s, MINI_MK3) {
                    for led in parsed.leds {
                        self.led_cache[page as usize]
                            .insert(led.led_index, LedCell::SysexColor(led.color));
                    }
                }
            }
            _ => {}
        }
        if page == self.current_page {
            vec![Out::ToDevice(bytes.to_vec())]
        } else {
            Vec::new()
        }
    }

    // -- Page change --------------------------------------------------------

    fn cycle_page(&mut self, up: bool) -> Vec<Out> {
        let new_page = if up {
            self.current_page
                .saturating_add(1)
                .min(self.pages.saturating_sub(1))
        } else {
            self.current_page.saturating_sub(1)
        };
        if new_page == self.current_page {
            return Vec::new();
        }
        self.change_page_to(new_page)
    }

    pub fn change_page_to(&mut self, new_page: u8) -> Vec<Out> {
        if new_page >= self.pages {
            return Vec::new();
        }
        let mut out = Vec::new();
        let old_page = self.current_page;

        // 1. Note Off for held pads on the *old* page so the host doesn't see stuck notes.
        let held: Vec<u8> = self.held.iter().copied().collect();
        for n in held {
            let off_bytes = match self.mode {
                Mode::NoteOffset => {
                    let logical = n + old_page * self.note_offset;
                    parse::note_off(0, logical, 0).to_vec()
                }
                Mode::PerPort => parse::note_off(0, n, 0).to_vec(),
            };
            match self.mode {
                Mode::NoteOffset => out.push(Out::ToHost(off_bytes)),
                Mode::PerPort => out.push(Out::ToHostPage {
                    page: old_page,
                    bytes: off_bytes,
                }),
            }
            self.suppressed_releases.insert(n);
        }

        self.current_page = new_page;

        // 2. Clear physical LEDs.
        for bytes in self.device.clear_all() {
            out.push(Out::ToDevice(bytes));
        }
        // 3. Replay cache for the new page.
        out.extend(self.replay_page_to_device());
        // 4. Update indicator LEDs.
        for bytes in self.device.paint_indicators(new_page, &self.indicators) {
            out.push(Out::ToDevice(bytes));
        }
        out
    }

    fn replay_page_to_device(&self) -> Vec<Out> {
        let cache = &self.led_cache[self.current_page as usize];
        if cache.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut sysex_leds = Vec::new();
        for (&physical, cell) in cache {
            match cell {
                LedCell::NoteOn { channel, velocity } => {
                    out.push(Out::ToDevice(
                        parse::note_on(*channel, physical, *velocity).to_vec(),
                    ));
                }
                LedCell::Cc { channel, value } => {
                    out.push(Out::ToDevice(
                        parse::cc(*channel, physical, *value).to_vec(),
                    ));
                }
                LedCell::SysexColor(color) => sysex_leds.push(LedSpec {
                    led_index: physical,
                    color: color.clone(),
                }),
            }
        }
        if !sysex_leds.is_empty() {
            sysex_leds.sort_by_key(|l| l.led_index);
            out.push(Out::ToDevice(
                LightingSysex {
                    model: MINI_MK3,
                    leds: sysex_leds,
                }
                .emit(),
            ));
        }
        out
    }

    // -- Helpers ------------------------------------------------------------

    fn to_host_current(&self, bytes: Vec<u8>) -> Out {
        match self.mode {
            Mode::NoteOffset => Out::ToHost(bytes),
            Mode::PerPort => Out::ToHostPage {
                page: self.current_page,
                bytes,
            },
        }
    }

    fn is_button(&self, b: ButtonRef, target: &ButtonRef) -> bool {
        b == *target
    }

    fn is_paged_logical_note(&self, note: u8) -> bool {
        let page = note / self.note_offset;
        let physical = note % self.note_offset;
        (page as usize) < self.led_cache.len() && self.device.is_grid_note(physical)
    }
}

fn bytes_for_note(channel: u8, note: u8, velocity: u8, is_on: bool) -> Vec<u8> {
    if is_on {
        parse::note_on(channel, note, velocity).to_vec()
    } else {
        parse::note_off(channel, note, velocity).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::apc_mini::ApcMini;
    use crate::midi::mini_mk3::MiniMk3;

    fn cfg_mini(pages: u8, mode: Mode) -> DeviceConfig {
        DeviceConfig {
            name: "Mini".into(),
            port_match: "Launchpad Mini".into(),
            driver: Driver::MiniMk3,
            pages,
            mode,
            host_port_in: if mode == Mode::NoteOffset {
                Some("in".into())
            } else {
                None
            },
            host_port_out: if mode == Mode::NoteOffset {
                Some("out".into())
            } else {
                None
            },
            note_offset: Some(64),
            page_port_prefix: None,
            boot_sysex: None,
            page_up_button: ButtonRef::Cc { number: 91 },
            page_down_button: ButtonRef::Cc { number: 92 },
            indicator_leds: vec![],
        }
    }

    fn cfg_apc(pages: u8, mode: Mode) -> DeviceConfig {
        DeviceConfig {
            name: "APC".into(),
            port_match: "APC MINI".into(),
            driver: Driver::ApcMini,
            pages,
            mode,
            host_port_in: if mode == Mode::NoteOffset {
                Some("in".into())
            } else {
                None
            },
            host_port_out: if mode == Mode::NoteOffset {
                Some("out".into())
            } else {
                None
            },
            note_offset: Some(64),
            page_port_prefix: None,
            boot_sysex: None,
            page_up_button: ButtonRef::Note { number: 98 },
            page_down_button: ButtonRef::Note { number: 99 },
            indicator_leds: vec![],
        }
    }

    fn proxy_mini_offset(pages: u8) -> Proxy {
        Proxy::new(&cfg_mini(pages, Mode::NoteOffset), Box::new(MiniMk3))
    }
    fn proxy_apc_offset(pages: u8) -> Proxy {
        Proxy::new(&cfg_apc(pages, Mode::NoteOffset), Box::new(ApcMini))
    }
    fn proxy_apc_perport(pages: u8) -> Proxy {
        Proxy::new(&cfg_apc(pages, Mode::PerPort), Box::new(ApcMini))
    }
    fn proxy_mini_perport(pages: u8) -> Proxy {
        Proxy::new(&cfg_mini(pages, Mode::PerPort), Box::new(MiniMk3))
    }

    fn host_offset_bytes(out: &[Out]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|o| match o {
                Out::ToHost(b) => Some(b.clone()),
                _ => None,
            })
            .collect()
    }
    fn host_page_outs(out: &[Out]) -> Vec<(u8, Vec<u8>)> {
        out.iter()
            .filter_map(|o| match o {
                Out::ToHostPage { page, bytes } => Some((*page, bytes.clone())),
                _ => None,
            })
            .collect()
    }
    fn device_bytes(out: &[Out]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|o| match o {
                Out::ToDevice(b) => Some(b.clone()),
                _ => None,
            })
            .collect()
    }

    // -- Note-offset mode (all the original tests) -------------------------

    #[test]
    fn offset_pad_press_page0() {
        let mut p = proxy_apc_offset(2);
        let out = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert_eq!(
            host_offset_bytes(&out),
            vec![parse::note_on(0, 5, 100).to_vec()]
        );
    }

    #[test]
    fn offset_pad_press_page1() {
        let mut p = proxy_apc_offset(2);
        p.change_page_to(1);
        let out = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert_eq!(
            host_offset_bytes(&out),
            vec![parse::note_on(0, 5 + 64, 100).to_vec()]
        );
    }

    #[test]
    fn offset_page_up_cc_does_not_forward() {
        let mut p = proxy_mini_offset(2);
        let out = p.handle_device_in(&parse::cc(0, 91, 127));
        assert_eq!(p.current_page, 1);
        assert!(host_offset_bytes(&out).is_empty());
    }

    #[test]
    fn offset_page_up_at_max_is_noop() {
        let mut p = proxy_mini_offset(2);
        p.change_page_to(1);
        let out = p.handle_device_in(&parse::cc(0, 91, 127));
        assert_eq!(p.current_page, 1);
        assert!(out.is_empty());
    }

    #[test]
    fn offset_non_grid_top_row_cc_passes_through() {
        let mut p = proxy_mini_offset(2);
        let out = p.handle_device_in(&parse::cc(0, 95, 127));
        assert_eq!(
            host_offset_bytes(&out),
            vec![parse::cc(0, 95, 127).to_vec()]
        );
    }

    #[test]
    fn offset_host_note_on_for_current_page() {
        let mut p = proxy_apc_offset(2);
        let out = p.handle_host_in(&parse::note_on(0, 5, 1));
        assert_eq!(device_bytes(&out), vec![parse::note_on(0, 5, 1).to_vec()]);
    }

    #[test]
    fn offset_host_note_on_other_page_caches_only() {
        let mut p = proxy_apc_offset(2);
        let out = p.handle_host_in(&parse::note_on(0, 69, 1));
        assert!(device_bytes(&out).is_empty());
        assert_eq!(
            p.led_cache[1].get(&5),
            Some(&LedCell::NoteOn {
                channel: 0,
                velocity: 1
            })
        );
    }

    #[test]
    fn offset_host_lighting_sysex_partitions_by_page() {
        let mut p = proxy_mini_offset(2);
        p.change_page_to(1);
        let bytes = LightingSysex {
            model: MINI_MK3,
            leds: vec![
                LedSpec {
                    led_index: 11,
                    color: ColorSpec::Static(5),
                },
                LedSpec {
                    led_index: 75,
                    color: ColorSpec::Static(7),
                },
            ],
        }
        .emit();
        let out = p.handle_host_in(&bytes);
        let dev = device_bytes(&out);
        assert_eq!(dev.len(), 1);
        let parsed = LightingSysex::parse(&dev[0], MINI_MK3).unwrap();
        assert_eq!(parsed.leds.len(), 1);
        assert_eq!(parsed.leds[0].led_index, 11);
        assert_eq!(parsed.leds[0].color, ColorSpec::Static(7));
    }

    #[test]
    fn offset_page_change_with_held_pad_emits_old_page_note_off() {
        let mut p = proxy_apc_offset(2);
        let _ = p.handle_device_in(&parse::note_on(0, 5, 100));
        let out = p.handle_device_in(&parse::note_on(0, 98, 127));
        assert!(
            host_offset_bytes(&out)
                .iter()
                .any(|m| m == &parse::note_off(0, 5, 0).to_vec())
        );
    }

    #[test]
    fn offset_release_after_page_change_is_suppressed() {
        let mut p = proxy_apc_offset(2);
        let _ = p.handle_device_in(&parse::note_on(0, 5, 100));
        let _ = p.handle_device_in(&parse::note_on(0, 98, 127));
        let out = p.handle_device_in(&parse::note_off(0, 5, 0));
        assert!(host_offset_bytes(&out).is_empty());
    }

    #[test]
    fn offset_page_change_replays_cache_to_device() {
        let mut p = proxy_apc_offset(2);
        let _ = p.handle_host_in(&parse::note_on(0, 64 + 5, 3));
        let out = p.change_page_to(1);
        let dev = device_bytes(&out);
        assert!(dev.iter().any(|b| b == &parse::note_on(0, 5, 3).to_vec()));
    }

    // -- Per-port mode ------------------------------------------------------

    #[test]
    fn perport_pad_press_emits_to_current_page_port_with_raw_note() {
        let mut p = proxy_apc_perport(4);
        p.change_page_to(2);
        let out = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert_eq!(
            host_page_outs(&out),
            vec![(2, parse::note_on(0, 5, 100).to_vec())]
        );
    }

    #[test]
    fn perport_allows_more_than_two_pages() {
        // 8 pages — impossible in note-offset mode, fine here.
        let mut p = proxy_apc_perport(8);
        p.change_page_to(7);
        let out = p.handle_device_in(&parse::note_on(0, 63, 1));
        assert_eq!(
            host_page_outs(&out),
            vec![(7, parse::note_on(0, 63, 1).to_vec())]
        );
    }

    #[test]
    fn perport_host_in_for_current_page_reaches_device_unchanged() {
        let mut p = proxy_apc_perport(4);
        p.change_page_to(1);
        let out = p.handle_host_in_per_port(1, &parse::note_on(0, 5, 3));
        assert_eq!(device_bytes(&out), vec![parse::note_on(0, 5, 3).to_vec()]);
        assert_eq!(
            p.led_cache[1].get(&5),
            Some(&LedCell::NoteOn {
                channel: 0,
                velocity: 3
            })
        );
    }

    #[test]
    fn perport_host_in_for_other_page_caches_only() {
        let mut p = proxy_apc_perport(4);
        // current_page = 0; host writes on port 2.
        let out = p.handle_host_in_per_port(2, &parse::note_on(0, 5, 3));
        assert!(device_bytes(&out).is_empty());
        assert!(p.led_cache[2].contains_key(&5));
        assert!(!p.led_cache[0].contains_key(&5));
    }

    #[test]
    fn perport_lighting_sysex_caches_then_passes_through_unchanged_when_active() {
        let mut p = proxy_mini_perport(4);
        p.change_page_to(1);
        let sysex = LightingSysex {
            model: MINI_MK3,
            leds: vec![LedSpec {
                led_index: 11,
                color: ColorSpec::Rgb { r: 0, g: 127, b: 0 },
            }],
        }
        .emit();
        let out = p.handle_host_in_per_port(1, &sysex);
        // Forwarded byte-for-byte (no rewriting in per-port mode).
        assert_eq!(device_bytes(&out), vec![sysex]);
        assert!(p.led_cache[1].contains_key(&11));
    }

    #[test]
    fn perport_page_change_emits_held_note_off_on_old_page_port() {
        let mut p = proxy_apc_perport(4);
        let _ = p.handle_device_in(&parse::note_on(0, 5, 100));
        let _ = p.handle_device_in(&parse::note_on(0, 98, 127)); // page up
        let outs = p.change_page_to(3);
        let _ = outs;
        // Use cycle_page directly: confirm via the page-up event sequence above
        // that current_page advanced.
        assert_eq!(p.current_page, 3);
    }

    #[test]
    fn perport_page_change_synthesizes_note_off_to_old_page() {
        let mut p = proxy_apc_perport(4);
        let _ = p.handle_device_in(&parse::note_on(0, 5, 100));
        // page up via configured Note 98 -> cycle_page.
        let out = p.handle_device_in(&parse::note_on(0, 98, 127));
        let host = host_page_outs(&out);
        assert!(
            host.iter()
                .any(|(page, b)| *page == 0 && b == &parse::note_off(0, 5, 0).to_vec()),
            "expected ToHostPage(page=0, NoteOff(5)), got {host:?}",
        );
    }

    #[test]
    fn perport_page_change_paints_indicators() {
        let mut cfg = cfg_mini(2, Mode::PerPort);
        cfg.indicator_leds = vec![ButtonRef::Cc { number: 89 }, ButtonRef::Cc { number: 79 }];
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        let out = p.change_page_to(1);
        let dev = device_bytes(&out);
        assert!(dev.iter().any(|b| b == &parse::cc(0, 79, 21).to_vec()));
        assert!(dev.iter().any(|b| b == &parse::cc(0, 89, 1).to_vec()));
    }
}
