//! End-to-end integration: drive a Proxy with a sequence of MIDI events
//! and assert the per-page LED cache and the streams it emits look right.

use midi_pages::config::{ButtonRef, ColorConfig, DeviceConfig, Mode, PortMatch};
use midi_pages::midi::apc_mini::ApcMini;
use midi_pages::midi::device::Driver;
use midi_pages::midi::mini_mk3::MiniMk3;
use midi_pages::midi::parse;
use midi_pages::midi::sysex_lighting::{ColorSpec, LedSpec, LightingSysex, MINI_MK3};
use midi_pages::proxy::{CacheKey, LedCell, Out, Proxy};

fn cfg(driver: Driver, pages: u8, mode: Mode) -> DeviceConfig {
    DeviceConfig {
        name: "test".into(),
        port_match: PortMatch::Simple("x".into()),
        driver,
        pages,
        mode,
        host_port_in: if mode == Mode::NoteOffset {
            Some("x".into())
        } else {
            None
        },
        host_port_out: if mode == Mode::NoteOffset {
            Some("x".into())
        } else {
            None
        },
        note_offset: Some(64),
        page_port_prefix: None,
        boot_sysex: None,
        next_page_button: Some(match driver {
            Driver::MiniMk3 => ButtonRef::Cc { number: 91 },
            Driver::ApcMini => ButtonRef::Note { number: 98 },
        }),
        previous_page_button: Some(match driver {
            Driver::MiniMk3 => ButtonRef::Cc { number: 92 },
            Driver::ApcMini => ButtonRef::Note { number: 99 },
        }),
        page_buttons: vec![],
        page_buttons_hold_to_preview: false,
        colors: ColorConfig::default(),
        global_buttons: vec![],
    }
}

#[test]
fn end_to_end_paged_press_and_led_cache_offset_mode() {
    let cfg = cfg(Driver::MiniMk3, 2, Mode::NoteOffset);
    let mut p = Proxy::new(&cfg, Box::new(MiniMk3));

    // Host pre-paints page 1 with a Lighting SysEx.
    let bytes = LightingSysex {
        model: MINI_MK3,
        leds: vec![LedSpec {
            led_index: 64 + 11,
            color: ColorSpec::Rgb { r: 0, g: 127, b: 0 },
        }],
    }
    .emit();
    let out = p.handle_host_in(&bytes);
    assert!(out.iter().all(|o| !matches!(o, Out::ToDevice(_))));
    assert!(p.led_cache[1].contains_key(&CacheKey::Note(11)));

    // Cycle to page 1.
    let out = p.handle_device_in(&parse::cc(0, 91, 127));
    assert_eq!(p.current_page, 1);
    let painted: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::ToDevice(b) => Some(b.clone()),
            _ => None,
        })
        .collect();
    assert!(painted.iter().any(|b| b.first() == Some(&0xF0)));

    // Press physical pad 11 -> host sees logical 75.
    let out = p.handle_device_in(&parse::note_on(0, 11, 100));
    let host: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::ToHost(b) => Some(b.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(host, vec![parse::note_on(0, 11 + 64, 100).to_vec()]);
}

#[test]
fn apc_mini_full_cycle_offset_mode() {
    let cfg = cfg(Driver::ApcMini, 2, Mode::NoteOffset);
    let mut p = Proxy::new(&cfg, Box::new(ApcMini));

    for n in 0u8..64 {
        let logical = 64 + n;
        let out = p.handle_host_in(&parse::note_on(0, logical, 1));
        assert!(out.is_empty());
    }
    assert_eq!(p.led_cache[1].len(), 64);
    for n in 0u8..64 {
        assert_eq!(
            p.led_cache[1].get(&CacheKey::Note(n)),
            Some(&LedCell::NoteOn {
                channel: 0,
                velocity: 1
            })
        );
    }

    let out = p.change_page_to(1);
    let on_count = out
        .iter()
        .filter_map(|o| match o {
            Out::ToDevice(b) => Some(b),
            _ => None,
        })
        .filter(|b| b[0] & 0xF0 == 0x90 && b[2] != 0)
        .count();
    assert_eq!(on_count, 64);
}

#[test]
fn end_to_end_per_port_mode_eight_pages() {
    let cfg = cfg(Driver::ApcMini, 8, Mode::PerPort);
    let mut p = Proxy::new(&cfg, Box::new(ApcMini));

    // Host paints LED on every page's virtual port.
    for page in 0u8..8 {
        let _ = p.handle_host_in_per_port(page, &parse::note_on(0, 5, 1 + page));
    }
    for page in 0u8..8 {
        assert_eq!(
            p.led_cache[page as usize].get(&CacheKey::Note(5)),
            Some(&LedCell::NoteOn {
                channel: 0,
                velocity: 1 + page,
            })
        );
    }

    // Switching to page 5 replays its cache (note 5 with velocity 6).
    let out = p.change_page_to(5);
    let dev: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::ToDevice(b) => Some(b.clone()),
            _ => None,
        })
        .collect();
    assert!(dev.iter().any(|b| b == &parse::note_on(0, 5, 6).to_vec()));

    // Press the physical pad — host should see it on page 5's port, raw note.
    let out = p.handle_device_in(&parse::note_on(0, 5, 127));
    let host: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::ToHostPage { page, bytes } => Some((*page, bytes.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(host, vec![(5, parse::note_on(0, 5, 127).to_vec())]);
}
