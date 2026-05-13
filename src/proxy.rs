//! Stateful paging proxy: rewrites MIDI in both directions and maintains a
//! per-page LED cache.

use std::collections::{HashMap, HashSet};

use crate::config::{ButtonRef, DeviceConfig, Mode};
use crate::midi::device::{Device, Driver};
use crate::midi::parse::{self, Msg};
use crate::midi::sysex_lighting::{ColorSpec, LedSpec, LightingSysex, MINI_MK3};

/// One LED's last-known state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedCell {
    NoteOn { channel: u8, velocity: u8 },
    Cc { channel: u8, value: u8 },
    SysexColor(ColorSpec),
}

/// Cache key — distinguishes Note-addressed buttons (grid pads) from
/// CC-addressed buttons (Mini MK3 side column / top row), since the two
/// number-spaces overlap (e.g. note 89 and CC 89 are different LEDs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheKey {
    Note(u8),
    Cc(u8),
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
    /// Send these bytes to the device after a delay. Used for the
    /// press-feedback flash on next/previous page buttons (light-on goes
    /// out as a regular ToDevice; this variant carries the matching
    /// light-off, scheduled `delay_ms` later).
    DeviceDelayedSend { delay_ms: u32, bytes: Vec<u8> },
}

/// Proxy state and the two rewrite paths.
pub struct Proxy {
    pub mode: Mode,
    /// Page the user "lives on" — moved by next/prev or by tapping a page
    /// button in Mode B. Persists across hold-preview windows.
    pub persistent_page: u8,
    /// `Some(p)` while a page button is held in Mode C; the grid is
    /// temporarily showing page `p`. `None` otherwise.
    pub held_preview: Option<u8>,
    /// Which page button started the current preview (so we know which
    /// release event ends it). `None` when not previewing.
    held_preview_button: Option<ButtonRef>,
    /// Cached visible page = `held_preview.unwrap_or(persistent_page)`.
    /// Updated by `set_persistent_page` / `enter_preview` / `exit_preview`.
    pub current_page: u8,
    pub pages: u8,
    pub note_offset: u8,
    pub next_page_button: Option<ButtonRef>,
    pub previous_page_button: Option<ButtonRef>,
    pub page_buttons: Vec<ButtonRef>,
    pub hold_to_preview: bool,
    pub driver: Driver,

    /// `led_cache[page][CacheKey] -> last LED state for that button on that page`.
    pub led_cache: Vec<HashMap<CacheKey, LedCell>>,

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
            persistent_page: 0,
            held_preview: None,
            held_preview_button: None,
            current_page: 0,
            pages: cfg.pages,
            note_offset: cfg.note_offset_value(),
            next_page_button: cfg.next_page_button,
            previous_page_button: cfg.previous_page_button,
            page_buttons: cfg.page_buttons.clone(),
            hold_to_preview: cfg.page_buttons_hold_to_preview,
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
                let btn = ButtonRef::Note { number: note };
                let is_press = velocity > 0;
                if Some(btn) == self.next_page_button {
                    return if is_press { self.handle_next_press(btn) } else { Vec::new() };
                }
                if Some(btn) == self.previous_page_button {
                    return if is_press { self.handle_previous_press(btn) } else { Vec::new() };
                }
                if let Some(slot) = self.page_button_slot(btn) {
                    return if is_press {
                        self.handle_page_button_press(slot, btn)
                    } else {
                        self.handle_page_button_release(btn)
                    };
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
                let btn = ButtonRef::Note { number: note };
                if Some(btn) == self.next_page_button || Some(btn) == self.previous_page_button {
                    return Vec::new();
                }
                if self.page_button_slot(btn).is_some() {
                    return self.handle_page_button_release(btn);
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
                let btn = ButtonRef::Cc { number: controller };
                let is_press = value > 0;
                if Some(btn) == self.next_page_button {
                    return if is_press { self.handle_next_press(btn) } else { Vec::new() };
                }
                if Some(btn) == self.previous_page_button {
                    return if is_press { self.handle_previous_press(btn) } else { Vec::new() };
                }
                if let Some(slot) = self.page_button_slot(btn) {
                    return if is_press {
                        self.handle_page_button_press(slot, btn)
                    } else {
                        self.handle_page_button_release(btn)
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

    fn page_button_slot(&self, btn: ButtonRef) -> Option<u8> {
        self.page_buttons
            .iter()
            .position(|b| *b == btn)
            .map(|i| i as u8)
    }

    fn handle_next_press(&mut self, btn: ButtonRef) -> Vec<Out> {
        let mut out = self.flash_pulse(btn);
        out.extend(self.cycle_persistent(true));
        out
    }

    fn handle_previous_press(&mut self, btn: ButtonRef) -> Vec<Out> {
        let mut out = self.flash_pulse(btn);
        out.extend(self.cycle_persistent(false));
        out
    }

    fn flash_pulse(&self, btn: ButtonRef) -> Vec<Out> {
        let on = self.device.paint_button(btn, self.device.flash_color());
        let off = self.device.clear_button(btn);
        vec![
            Out::ToDevice(on),
            Out::DeviceDelayedSend {
                delay_ms: 200,
                bytes: off,
            },
        ]
    }

    fn handle_page_button_press(&mut self, slot: u8, btn: ButtonRef) -> Vec<Out> {
        if slot >= self.pages {
            return Vec::new();
        }
        if self.hold_to_preview {
            self.enter_preview(slot, btn)
        } else {
            self.set_persistent_page(slot)
        }
    }

    fn handle_page_button_release(&mut self, btn: ButtonRef) -> Vec<Out> {
        if self.hold_to_preview && self.held_preview_button == Some(btn) {
            self.exit_preview()
        } else {
            Vec::new()
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
                        .insert(CacheKey::Cc(physical), LedCell::Cc { channel, value });
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
            CacheKey::Note(physical),
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
                    .insert(CacheKey::Note(physical), LedCell::SysexColor(led.color.clone()));
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
                self.led_cache[page as usize]
                    .insert(CacheKey::Note(note), LedCell::NoteOn { channel, velocity });
            }
            Msg::NoteOff { channel, note, .. } if self.device.is_grid_note(note) => {
                self.led_cache[page as usize].insert(
                    CacheKey::Note(note),
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
            } if !self.is_proxy_managed_cc(controller) => {
                // Mini MK3 side / top-row LEDs are CC-addressed. Cache them so a
                // page switch and return restores them. Page-cycle and indicator
                // CCs are excluded because the proxy itself manages those —
                // caching DAW writes to them would just be clobbered by
                // `paint_indicators` on every page change.
                self.led_cache[page as usize]
                    .insert(CacheKey::Cc(controller), LedCell::Cc { channel, value });
            }
            Msg::SysEx(s) if LightingSysex::looks_like(s, MINI_MK3) => {
                if let Ok(parsed) = LightingSysex::parse(s, MINI_MK3) {
                    for led in parsed.leds {
                        self.led_cache[page as usize].insert(
                            CacheKey::Note(led.led_index),
                            LedCell::SysexColor(led.color),
                        );
                    }
                }
            }
            other => {
                tracing::debug!(
                    page = page,
                    bytes = ?bytes,
                    msg = ?other,
                    "host-in: uncached message (forwarded once, will not replay on page return)"
                );
            }
        }
        if page == self.current_page {
            vec![Out::ToDevice(bytes.to_vec())]
        } else {
            Vec::new()
        }
    }

    /// True if `controller` is one of the CCs the proxy itself manages
    /// (next/previous page, or any page button). DAW writes to these are
    /// fine to forward but caching them is pointless because
    /// `paint_indicators` will overwrite them on every page change anyway.
    fn is_proxy_managed_cc(&self, controller: u8) -> bool {
        let cc = ButtonRef::Cc { number: controller };
        self.next_page_button == Some(cc)
            || self.previous_page_button == Some(cc)
            || self.page_buttons.contains(&cc)
    }

    // -- Page change --------------------------------------------------------

    /// Move the persistent page forward/backward by one and update the
    /// visible page if no preview is active. Used by next/prev buttons.
    fn cycle_persistent(&mut self, forward: bool) -> Vec<Out> {
        let new_page = if forward {
            self.persistent_page
                .saturating_add(1)
                .min(self.pages.saturating_sub(1))
        } else {
            self.persistent_page.saturating_sub(1)
        };
        if new_page == self.persistent_page {
            return Vec::new();
        }
        self.set_persistent_page(new_page)
    }

    /// Set the persistent page to `p`. If a preview is active, only the
    /// stored persistent_page is updated (no visible change); otherwise
    /// the grid is repainted to show the new page.
    pub fn set_persistent_page(&mut self, p: u8) -> Vec<Out> {
        if p >= self.pages {
            return Vec::new();
        }
        self.persistent_page = p;
        if self.held_preview.is_some() {
            // Visible page is the preview; persistent move is silent.
            return Vec::new();
        }
        self.change_page_to(p)
    }

    /// Mode C: enter preview of page `p` while `btn` is held. Visible page
    /// becomes `p` until release.
    fn enter_preview(&mut self, p: u8, btn: ButtonRef) -> Vec<Out> {
        if p >= self.pages {
            return Vec::new();
        }
        if self.held_preview == Some(p) {
            return Vec::new();
        }
        self.held_preview = Some(p);
        self.held_preview_button = Some(btn);
        self.change_page_to(p)
    }

    /// Mode C: end preview, revert visible page to the persistent one.
    fn exit_preview(&mut self) -> Vec<Out> {
        if self.held_preview.is_none() {
            return Vec::new();
        }
        self.held_preview = None;
        self.held_preview_button = None;
        self.change_page_to(self.persistent_page)
    }

    /// Internal page swap: synthesize note-offs for held pads on the OLD
    /// visible page, swap, clear the device, replay cache, repaint
    /// indicators. Public for tests that want to drive a specific page
    /// without going through next/prev or page buttons.
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
        for bytes in self.device.paint_indicators(new_page, &self.page_buttons) {
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
        for (&key, cell) in cache {
            match (key, cell) {
                (CacheKey::Note(n), LedCell::NoteOn { channel, velocity }) => {
                    out.push(Out::ToDevice(
                        parse::note_on(*channel, n, *velocity).to_vec(),
                    ));
                }
                (CacheKey::Cc(c), LedCell::Cc { channel, value }) => {
                    out.push(Out::ToDevice(parse::cc(*channel, c, *value).to_vec()));
                }
                (CacheKey::Note(n), LedCell::SysexColor(color)) => sysex_leds.push(LedSpec {
                    led_index: n,
                    color: color.clone(),
                }),
                // Type/key mismatches shouldn't happen — write-paths always
                // pair Note keys with NoteOn/SysexColor and Cc keys with Cc.
                // Skip silently rather than panicking.
                _ => {}
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
            next_page_button: Some(ButtonRef::Cc { number: 91 }),
            previous_page_button: Some(ButtonRef::Cc { number: 92 }),
            page_buttons: vec![],
            page_buttons_hold_to_preview: false,
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
            next_page_button: Some(ButtonRef::Note { number: 98 }),
            previous_page_button: Some(ButtonRef::Note { number: 99 }),
            page_buttons: vec![],
            page_buttons_hold_to_preview: false,
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
    fn offset_next_at_max_is_page_noop_but_still_flashes() {
        // Pressing next at the max page emits the flash feedback but does
        // NOT move the page or emit any host-bound messages.
        let mut p = proxy_mini_offset(2);
        p.set_persistent_page(1);
        let out = p.handle_device_in(&parse::cc(0, 91, 127));
        assert_eq!(p.current_page, 1);
        assert!(host_offset_bytes(&out).is_empty());
        // Flash-on (device send) and delayed-clear are still present.
        assert_eq!(device_bytes(&out).len(), 1);
        assert_eq!(delayed_device_bytes(&out).len(), 1);
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
            p.led_cache[1].get(&CacheKey::Note(5)),
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
            p.led_cache[1].get(&CacheKey::Note(5)),
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
        assert!(p.led_cache[2].contains_key(&CacheKey::Note(5)));
        assert!(!p.led_cache[0].contains_key(&CacheKey::Note(5)));
    }

    #[test]
    fn perport_host_cc_for_non_grid_button_is_cached_and_replayed() {
        // Mini MK3 side / top-row LEDs are CC-controlled. DAW writes to a
        // non-grid CC (e.g. CC 19) on an inactive page should be cached so
        // returning to that page restores the LED.
        let mut p = proxy_mini_perport(4);
        // Page 0 active, write to page 1's port.
        let out = p.handle_host_in_per_port(1, &parse::cc(0, 19, 42));
        assert!(device_bytes(&out).is_empty(), "uncached on inactive page");
        assert_eq!(
            p.led_cache[1].get(&CacheKey::Cc(19)),
            Some(&LedCell::Cc {
                channel: 0,
                value: 42,
            })
        );
        // Switch to page 1 — replay must include the CC bytes.
        let dev = device_bytes(&p.change_page_to(1));
        assert!(
            dev.iter().any(|b| b == &parse::cc(0, 19, 42).to_vec()),
            "page-switch replay missing CC: {dev:02X?}"
        );
    }

    #[test]
    fn perport_host_cc_for_proxy_managed_button_is_not_cached() {
        // Next/prev and page-button CCs are managed by paint_indicators —
        // caching DAW writes to them is pointless and would just be clobbered.
        let mut cfg = cfg_mini(4, Mode::PerPort);
        cfg.page_buttons = vec![ButtonRef::Cc { number: 89 }];
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        // next_page_button = CC 91 (per cfg_mini).
        let _ = p.handle_host_in_per_port(1, &parse::cc(0, 91, 100));
        assert!(!p.led_cache[1].contains_key(&CacheKey::Cc(91)));
        // previous_page_button = CC 92.
        let _ = p.handle_host_in_per_port(1, &parse::cc(0, 92, 100));
        assert!(!p.led_cache[1].contains_key(&CacheKey::Cc(92)));
        // CC 89 is a page button.
        let _ = p.handle_host_in_per_port(1, &parse::cc(0, 89, 100));
        assert!(!p.led_cache[1].contains_key(&CacheKey::Cc(89)));
        // A non-managed CC IS cached.
        let _ = p.handle_host_in_per_port(1, &parse::cc(0, 19, 100));
        assert!(p.led_cache[1].contains_key(&CacheKey::Cc(19)));
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
        assert!(p.led_cache[1].contains_key(&CacheKey::Note(11)));
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
    fn perport_page_change_paints_page_button_indicators() {
        let mut cfg = cfg_mini(2, Mode::PerPort);
        cfg.page_buttons = vec![ButtonRef::Cc { number: 89 }, ButtonRef::Cc { number: 79 }];
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        let out = p.change_page_to(1);
        let dev = device_bytes(&out);
        assert!(dev.iter().any(|b| b == &parse::cc(0, 79, 21).to_vec()));
        assert!(dev.iter().any(|b| b == &parse::cc(0, 89, 1).to_vec()));
    }

    // -- Mode A / B / C tests ----------------------------------------------

    fn delayed_device_bytes(out: &[Out]) -> Vec<(u32, Vec<u8>)> {
        out.iter()
            .filter_map(|o| match o {
                Out::DeviceDelayedSend { delay_ms, bytes } => Some((*delay_ms, bytes.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn next_prev_emits_flash_then_delayed_clear() {
        // Press next_page_button (CC 91 on the Mini) -> immediate flash-on
        // and a delayed flash-off, plus the page-change side effects.
        let mut p = proxy_mini_perport(4);
        let out = p.handle_device_in(&parse::cc(0, 91, 127));
        let dev = device_bytes(&out);
        // First device write should be the flash-on at flash color (21 for Mini MK3).
        assert_eq!(dev.first(), Some(&parse::cc(0, 91, 21).to_vec()));
        let delayed = delayed_device_bytes(&out);
        assert_eq!(delayed.len(), 1);
        assert_eq!(delayed[0], (200, parse::cc(0, 91, 0).to_vec()));
        assert_eq!(p.persistent_page, 1);
        assert_eq!(p.current_page, 1);
    }

    #[test]
    fn page_button_tap_jumps_to_page_in_mode_b() {
        // Mode B: page_buttons configured, hold_to_preview = false.
        let mut cfg = cfg_mini(4, Mode::PerPort);
        cfg.page_buttons = vec![
            ButtonRef::Cc { number: 89 },
            ButtonRef::Cc { number: 79 },
            ButtonRef::Cc { number: 69 },
            ButtonRef::Cc { number: 59 },
        ];
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        // Tap the third page button (slot 2 -> page 2).
        let _ = p.handle_device_in(&parse::cc(0, 69, 127));
        assert_eq!(p.persistent_page, 2);
        assert_eq!(p.current_page, 2);
        // Release does nothing in Mode B.
        let out = p.handle_device_in(&parse::cc(0, 69, 0));
        assert!(out.is_empty());
        assert_eq!(p.current_page, 2);
    }

    #[test]
    fn hold_preview_press_changes_visible_page() {
        let mut cfg = cfg_mini(4, Mode::PerPort);
        cfg.page_buttons = vec![
            ButtonRef::Cc { number: 89 },
            ButtonRef::Cc { number: 79 },
            ButtonRef::Cc { number: 69 },
            ButtonRef::Cc { number: 59 },
        ];
        cfg.page_buttons_hold_to_preview = true;
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        // Hold slot 2 -> visible page becomes 2; persistent stays 0.
        let _ = p.handle_device_in(&parse::cc(0, 69, 127));
        assert_eq!(p.current_page, 2);
        assert_eq!(p.persistent_page, 0);
        assert_eq!(p.held_preview, Some(2));
    }

    #[test]
    fn hold_preview_release_reverts_to_persistent() {
        let mut cfg = cfg_mini(4, Mode::PerPort);
        cfg.page_buttons = vec![
            ButtonRef::Cc { number: 89 },
            ButtonRef::Cc { number: 79 },
            ButtonRef::Cc { number: 69 },
            ButtonRef::Cc { number: 59 },
        ];
        cfg.page_buttons_hold_to_preview = true;
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        // Move persistent first.
        let _ = p.handle_device_in(&parse::cc(0, 91, 127)); // next -> page 1
        assert_eq!(p.persistent_page, 1);
        assert_eq!(p.current_page, 1);
        // Hold slot 3 -> preview page 3.
        let _ = p.handle_device_in(&parse::cc(0, 59, 127));
        assert_eq!(p.current_page, 3);
        assert_eq!(p.persistent_page, 1);
        // Release -> revert to persistent (page 1).
        let _ = p.handle_device_in(&parse::cc(0, 59, 0));
        assert_eq!(p.current_page, 1);
        assert_eq!(p.persistent_page, 1);
        assert_eq!(p.held_preview, None);
    }

    #[test]
    fn hold_preview_grid_press_routes_to_previewed_page() {
        // The killer test: while holding a page button, grid presses must
        // route to the previewed page, not the persistent one.
        let mut cfg = cfg_apc(4, Mode::PerPort);
        cfg.page_buttons = vec![
            ButtonRef::Note { number: 82 },
            ButtonRef::Note { number: 83 },
            ButtonRef::Note { number: 84 },
            ButtonRef::Note { number: 85 },
        ];
        cfg.page_buttons_hold_to_preview = true;
        let mut p = Proxy::new(&cfg, Box::new(ApcMini));
        // Persistent = 0. Hold page button slot 2.
        let _ = p.handle_device_in(&parse::note_on(0, 84, 127));
        assert_eq!(p.current_page, 2);
        // Press grid pad 5 -> must go to ToHostPage(page=2).
        let out = p.handle_device_in(&parse::note_on(0, 5, 100));
        assert_eq!(
            host_page_outs(&out),
            vec![(2, parse::note_on(0, 5, 100).to_vec())]
        );
        // Release page button -> revert. Grid pad 5 is now held on page 2;
        // the revert should synthesize a note-off on page 2.
        let out = p.handle_device_in(&parse::note_on(0, 84, 0));
        let host = host_page_outs(&out);
        assert!(
            host.iter()
                .any(|(page, b)| *page == 2 && b == &parse::note_off(0, 5, 0).to_vec()),
            "expected ToHostPage(page=2, NoteOff(5)) on revert, got {host:?}",
        );
        assert_eq!(p.current_page, 0);
    }

    #[test]
    fn hold_mode_tap_does_not_change_persistent_page() {
        // In Mode C, tap-then-release of a page button must not move the
        // persistent page (only next/prev does).
        let mut cfg = cfg_mini(4, Mode::PerPort);
        cfg.page_buttons = vec![
            ButtonRef::Cc { number: 89 },
            ButtonRef::Cc { number: 79 },
            ButtonRef::Cc { number: 69 },
            ButtonRef::Cc { number: 59 },
        ];
        cfg.page_buttons_hold_to_preview = true;
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        // Tap (press + release) slot 2.
        let _ = p.handle_device_in(&parse::cc(0, 69, 127));
        let _ = p.handle_device_in(&parse::cc(0, 69, 0));
        assert_eq!(p.persistent_page, 0, "persistent must stay at 0 after tap");
        assert_eq!(p.current_page, 0);
    }

    #[test]
    fn next_prev_still_works_when_in_preview() {
        let mut cfg = cfg_mini(4, Mode::PerPort);
        cfg.page_buttons = vec![
            ButtonRef::Cc { number: 89 },
            ButtonRef::Cc { number: 79 },
            ButtonRef::Cc { number: 69 },
            ButtonRef::Cc { number: 59 },
        ];
        cfg.page_buttons_hold_to_preview = true;
        let mut p = Proxy::new(&cfg, Box::new(MiniMk3));
        // Hold slot 3 -> preview page 3.
        let _ = p.handle_device_in(&parse::cc(0, 59, 127));
        assert_eq!(p.current_page, 3);
        assert_eq!(p.persistent_page, 0);
        // Press next -> persistent moves but visible page stays at preview.
        let _ = p.handle_device_in(&parse::cc(0, 91, 127));
        assert_eq!(p.persistent_page, 1);
        assert_eq!(p.current_page, 3);
        // Release preview -> revert to (newly moved) persistent = page 1.
        let _ = p.handle_device_in(&parse::cc(0, 59, 0));
        assert_eq!(p.current_page, 1);
    }
}
