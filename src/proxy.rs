//! Stateful paging proxy: rewrites MIDI in both directions and maintains a
//! per-page LED cache.

use std::collections::{HashMap, HashSet};

use crate::config::{ButtonRef, DeviceConfig};
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
    /// Send these bytes to the host (loopMIDI side).
    ToHost(Vec<u8>),
    /// Send these bytes to the device (real USB-MIDI side).
    ToDevice(Vec<u8>),
}

/// Proxy state and the two rewrite paths.
pub struct Proxy {
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
            current_page: 0,
            pages: cfg.pages,
            note_offset: cfg.note_offset,
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
                    return vec![Out::ToHost(bytes.to_vec())];
                }
                self.held.insert(note);
                let logical = note + self.current_page * self.note_offset;
                vec![Out::ToHost(
                    parse::note_on(channel, logical, velocity).to_vec(),
                )]
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
                    return vec![Out::ToHost(bytes.to_vec())];
                }
                let was_held = self.held.remove(&note);
                if self.suppressed_releases.remove(&note) {
                    // We already sent the matching Note Off on a previous page.
                    return Vec::new();
                }
                if !was_held {
                    // Stray Note Off (we never saw the press); pass through best-effort.
                }
                let logical = note + self.current_page * self.note_offset;
                vec![Out::ToHost(
                    parse::note_off(channel, logical, velocity).to_vec(),
                )]
            }
            Msg::Cc {
                channel,
                controller,
                value,
            } => {
                // Page buttons configured as CC (Mini MK3 arrows are CC 91/92).
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
                    return vec![Out::ToHost(bytes.to_vec())];
                }
                let logical = controller + self.current_page * self.note_offset;
                vec![Out::ToHost(parse::cc(channel, logical, value).to_vec())]
            }
            Msg::SysEx(_) | Msg::Other(_) => {
                vec![Out::ToHost(bytes.to_vec())]
            }
        }
    }

    // -- Host -> device -----------------------------------------------------

    /// Handle a message arriving from the host (DasLight LED update etc.).
    pub fn handle_host_in(&mut self, bytes: &[u8]) -> Vec<Out> {
        // Lighting SysEx gets its own path (per ADR 0005).
        if LightingSysex::looks_like(bytes, MINI_MK3) {
            return self.handle_host_lighting_sysex(bytes);
        }

        let msg = parse::classify(bytes);
        match msg {
            Msg::NoteOn {
                channel,
                note,
                velocity,
            } => self.handle_host_note(channel, note, velocity, true),
            Msg::NoteOff {
                channel,
                note,
                velocity,
            } => self.handle_host_note(channel, note, velocity, false),
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
            Msg::SysEx(_) | Msg::Other(_) => {
                // Unrelated SysEx (mode select, device inquiry, etc.): pass through.
                vec![Out::ToDevice(bytes.to_vec())]
            }
        }
    }

    fn handle_host_note(&mut self, channel: u8, note: u8, velocity: u8, is_on: bool) -> Vec<Out> {
        // Notes addressed below the page-1 grid: these target side/top LEDs that
        // aren't paged. Pass through unchanged.
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
        // "Clear all" detection: host writing 0-velocity Note Offs across the
        // whole grid is its way of wiping LEDs. A single zero-velocity note isn't
        // enough on its own — but each one should still wipe its own cache slot,
        // so just record the off. This matches real DasLight behaviour.
        let cell = LedCell::NoteOn {
            channel,
            velocity: if is_on { velocity } else { 0 },
        };
        self.led_cache[target_page as usize].insert(physical, cell);

        if target_page == self.current_page {
            let bytes = bytes_for_note(channel, physical, velocity, is_on);
            vec![Out::ToDevice(bytes)]
        } else {
            Vec::new()
        }
    }

    fn handle_host_lighting_sysex(&mut self, bytes: &[u8]) -> Vec<Out> {
        let parsed = match LightingSysex::parse(bytes, MINI_MK3) {
            Ok(p) => p,
            // If parsing fails, forward unchanged — we'd rather pass a malformed
            // SysEx and let the device complain than silently drop it.
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
            // No-op: at boundary. Don't wipe LEDs unnecessarily.
            return Vec::new();
        }
        self.change_page_to(new_page)
    }

    pub fn change_page_to(&mut self, new_page: u8) -> Vec<Out> {
        if new_page >= self.pages {
            return Vec::new();
        }
        let mut out = Vec::new();

        // 1. Note Off for held pads on the *old* page so the host doesn't see stuck notes.
        let held: Vec<u8> = self.held.iter().copied().collect();
        for n in held {
            let logical = n + self.current_page * self.note_offset;
            out.push(Out::ToHost(parse::note_off(0, logical, 0).to_vec()));
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
            // Sort for deterministic output (also nicer in tests).
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

    fn is_button(&self, b: ButtonRef, target: &ButtonRef) -> bool {
        b == *target
    }

    fn is_paged_logical_note(&self, note: u8) -> bool {
        // Within the "paged window": note maps to a grid pad on some page.
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

    fn cfg_mini(pages: u8) -> DeviceConfig {
        DeviceConfig {
            name: "Mini".into(),
            port_match: "Launchpad Mini".into(),
            host_port_in: "in".into(),
            host_port_out: "out".into(),
            driver: Driver::MiniMk3,
            pages,
            note_offset: 64,
            boot_sysex: None,
            page_up_button: ButtonRef::Cc { number: 91 },
            page_down_button: ButtonRef::Cc { number: 92 },
            indicator_leds: vec![],
        }
    }

    fn cfg_apc(pages: u8) -> DeviceConfig {
        DeviceConfig {
            name: "APC".into(),
            port_match: "APC MINI".into(),
            host_port_in: "in".into(),
            host_port_out: "out".into(),
            driver: Driver::ApcMini,
            pages,
            note_offset: 64,
            boot_sysex: None,
            page_up_button: ButtonRef::Note { number: 98 },
            page_down_button: ButtonRef::Note { number: 99 },
            indicator_leds: vec![],
        }
    }

    fn proxy_mini(pages: u8) -> Proxy {
        Proxy::new(&cfg_mini(pages), Box::new(MiniMk3))
    }

    fn proxy_apc(pages: u8) -> Proxy {
        Proxy::new(&cfg_apc(pages), Box::new(ApcMini))
    }

    fn host_bytes(out: &[Out]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|o| match o {
                Out::ToHost(b) => Some(b.clone()),
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

    // -- Device -> host ----------------------------------------------------

    #[test]
    fn pad_press_page0_passes_note_through() {
        // APC mini grid note 5 -> host sees note 5 on page 0.
        let mut p = proxy_apc(2);
        let out = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert_eq!(host_bytes(&out), vec![parse::note_on(0, 5, 100).to_vec()]);
    }

    #[test]
    fn pad_press_page1_offsets_by_offset() {
        // 2 pages * 64 = 128 fits MIDI's 7-bit note range exactly.
        let mut p = proxy_apc(2);
        p.change_page_to(1);
        let out = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert_eq!(
            host_bytes(&out),
            vec![parse::note_on(0, 5 + 64, 100).to_vec()]
        );
    }

    #[test]
    fn page_up_cc_press_mutates_state_and_does_not_forward() {
        let mut p = proxy_mini(3);
        let out = p.handle_device_in(&parse::cc(0, 91, 127));
        assert_eq!(p.current_page, 1);
        assert!(
            host_bytes(&out).is_empty(),
            "page button must not reach host"
        );
    }

    #[test]
    fn page_up_at_max_is_noop() {
        let mut p = proxy_mini(2);
        p.change_page_to(1);
        let before = p.current_page;
        let out = p.handle_device_in(&parse::cc(0, 91, 127));
        assert_eq!(p.current_page, before);
        // No-op: nothing emitted.
        assert!(out.is_empty());
    }

    #[test]
    fn non_grid_top_row_cc_passes_through_unchanged() {
        // Mini MK3: CC 95 is the top-row 'session' button — we have not configured
        // it as a page button, so it must reach the host as-is.
        let mut p = proxy_mini(2);
        let out = p.handle_device_in(&parse::cc(0, 95, 127));
        assert_eq!(host_bytes(&out), vec![parse::cc(0, 95, 127).to_vec()]);
    }

    // -- Host -> device ----------------------------------------------------

    #[test]
    fn host_note_on_for_current_page_reaches_device() {
        let mut p = proxy_apc(2);
        let out = p.handle_host_in(&parse::note_on(0, 5, 1));
        assert_eq!(device_bytes(&out), vec![parse::note_on(0, 5, 1).to_vec()]);
    }

    #[test]
    fn host_note_on_for_other_page_caches_only() {
        let mut p = proxy_apc(2);
        // current_page = 0; address logical note 69 = page 1, physical 5.
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
    fn host_lighting_sysex_partitions_by_page() {
        // 2 pages, offset 64 -> SysEx LED indices 0..127 cover both pages.
        let mut p = proxy_mini(2);
        p.change_page_to(1);
        // LEDs at 11 (page 0) and 75 (page 1) — note SysEx LED indices and Note
        // numbers share the same 0..127 space.
        let leds = vec![
            LedSpec {
                led_index: 11,
                color: ColorSpec::Static(5),
            },
            LedSpec {
                led_index: 75,
                color: ColorSpec::Static(7),
            },
        ];
        let bytes = LightingSysex {
            model: MINI_MK3,
            leds,
        }
        .emit();
        let out = p.handle_host_in(&bytes);

        // Should emit one SysEx with the page-1 entry rewritten to physical 11.
        let dev = device_bytes(&out);
        assert_eq!(dev.len(), 1);
        let parsed = LightingSysex::parse(&dev[0], MINI_MK3).unwrap();
        assert_eq!(parsed.leds.len(), 1);
        assert_eq!(parsed.leds[0].led_index, 11);
        assert_eq!(parsed.leds[0].color, ColorSpec::Static(7));

        // Both caches updated.
        assert!(p.led_cache[0].contains_key(&11));
        assert!(p.led_cache[1].contains_key(&11));
    }

    #[test]
    fn host_lighting_sysex_with_zero_on_page_emits_nothing() {
        let mut p = proxy_mini(2);
        // current_page = 0; only off-page LEDs (page 1, indices 65..).
        let bytes = LightingSysex {
            model: MINI_MK3,
            leds: vec![
                LedSpec {
                    led_index: 75,
                    color: ColorSpec::Static(1),
                },
                LedSpec {
                    led_index: 80,
                    color: ColorSpec::Static(2),
                },
            ],
        }
        .emit();
        let out = p.handle_host_in(&bytes);
        assert!(device_bytes(&out).is_empty());
        assert!(p.led_cache[1].contains_key(&11));
        assert!(p.led_cache[1].contains_key(&16));
    }

    // -- Page change -------------------------------------------------------

    #[test]
    fn page_change_with_held_pad_emits_old_page_note_off() {
        let mut p = proxy_apc(2);
        let _ = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert!(p.held.contains(&5));
        let out = p.handle_device_in(&parse::note_on(0, 98, 127)); // page up
        let host = host_bytes(&out);
        // Synthesized old-page Note Off for note 5 on page 0.
        assert!(host.iter().any(|m| m == &parse::note_off(0, 5, 0).to_vec()));
        assert_eq!(p.current_page, 1);
    }

    #[test]
    fn release_after_page_change_is_suppressed() {
        let mut p = proxy_apc(2);
        let _ = p.handle_device_in(&parse::note_on(0, 5, 100));
        let _ = p.handle_device_in(&parse::note_on(0, 98, 127)); // page up
        // Now physically release pad 5.
        let out = p.handle_device_in(&parse::note_off(0, 5, 0));
        assert!(host_bytes(&out).is_empty(), "release must not double-emit");
    }

    #[test]
    fn page_change_replays_cache_to_device() {
        let mut p = proxy_apc(2);
        // Cache page 1 with one LED.
        let _ = p.handle_host_in(&parse::note_on(0, 64 + 5, 3));
        let out = p.change_page_to(1);
        let dev = device_bytes(&out);
        // 64 clear_all messages, then a NoteOn(0, 5, 3).
        assert_eq!(dev.iter().filter(|b| b[2] != 0).count(), 1);
        assert!(dev.iter().any(|b| b == &parse::note_on(0, 5, 3).to_vec()));
    }

    #[test]
    fn page_change_paints_indicators() {
        let mut cfg = cfg_mini(2);
        cfg.indicator_leds = vec![ButtonRef::Cc { number: 89 }, ButtonRef::Cc { number: 79 }];
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        let out = p.change_page_to(1);
        let dev = device_bytes(&out);
        assert!(dev.iter().any(|b| b == &parse::cc(0, 79, 21).to_vec()));
        assert!(dev.iter().any(|b| b == &parse::cc(0, 89, 1).to_vec()));
    }
}
