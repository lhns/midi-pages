//! End-to-end integration: drive a Proxy with a sequence of MIDI events
//! and assert the per-page LED cache and the streams it emits look right.

use midi_pages::config::{ButtonRef, DeviceConfig};
use midi_pages::midi::apc_mini::ApcMini;
use midi_pages::midi::device::Driver;
use midi_pages::midi::mini_mk3::MiniMk3;
use midi_pages::midi::parse;
use midi_pages::midi::sysex_lighting::{ColorSpec, LedSpec, LightingSysex, MINI_MK3};
use midi_pages::proxy::{LedCell, Out, Proxy};

fn cfg(driver: Driver, pages: u8) -> DeviceConfig {
    DeviceConfig {
        name: "test".into(),
        port_match: "x".into(),
        host_port_in: "x".into(),
        host_port_out: "x".into(),
        driver,
        pages,
        note_offset: 64,
        boot_sysex: None,
        page_up_button: match driver {
            Driver::MiniMk3 => ButtonRef::Cc { number: 91 },
            Driver::ApcMini => ButtonRef::Note { number: 98 },
        },
        page_down_button: match driver {
            Driver::MiniMk3 => ButtonRef::Cc { number: 92 },
            Driver::ApcMini => ButtonRef::Note { number: 99 },
        },
        indicator_leds: vec![],
    }
}

#[test]
fn end_to_end_paged_press_and_led_cache() {
    let cfg = cfg(Driver::MiniMk3, 2);
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
    // current_page = 0, target page 1 -> nothing reaches the device, only cache.
    assert!(out.iter().all(|o| !matches!(o, Out::ToDevice(_))));
    assert!(p.led_cache[1].contains_key(&11));

    // User cycles to page 1 with Mini MK3 arrow up (CC 91).
    let out = p.handle_device_in(&parse::cc(0, 91, 127));
    assert_eq!(p.current_page, 1);
    let painted: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let Out::ToDevice(b) = o {
                Some(b.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(painted.iter().any(|b| b.first() == Some(&0xF0)));

    // User presses physical pad 11 -> host sees logical 75.
    let out = p.handle_device_in(&parse::note_on(0, 11, 100));
    let host: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let Out::ToHost(b) = o {
                Some(b.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(host, vec![parse::note_on(0, 11 + 64, 100).to_vec()]);
}

#[test]
fn apc_mini_full_cycle() {
    let cfg = cfg(Driver::ApcMini, 2);
    let mut p = Proxy::new(&cfg, Box::new(ApcMini));

    // Host paints all 64 grid notes on page 1 (logical 64..127).
    for n in 0u8..64 {
        let logical = 64 + n;
        let out = p.handle_host_in(&parse::note_on(0, logical, 1));
        // Page 1 is not active -> nothing to device, only cache.
        assert!(out.is_empty());
    }
    assert_eq!(p.led_cache[1].len(), 64);
    for n in 0u8..64 {
        assert_eq!(
            p.led_cache[1].get(&n),
            Some(&LedCell::NoteOn {
                channel: 0,
                velocity: 1
            })
        );
    }

    // Switch to page 1 and confirm 64 LED messages get sent.
    let out = p.change_page_to(1);
    let on_count = out
        .iter()
        .filter_map(|o| {
            if let Out::ToDevice(b) = o {
                Some(b)
            } else {
                None
            }
        })
        .filter(|b| b[0] & 0xF0 == 0x90 && b[2] != 0)
        .count();
    assert_eq!(on_count, 64);
}
