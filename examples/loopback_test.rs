//! Quick round-trip test for a WMS loopback via midir / WinMM.
//! Run: `cargo run --release --example loopback_test`.

use std::sync::mpsc;
use std::time::Duration;

use midir::{MidiInput, MidiOutput};

fn main() {
    let needle_a = std::env::args().nth(1).unwrap_or_else(|| "midi-pages-test-a".into());
    let needle_b = std::env::args().nth(2).unwrap_or_else(|| "midi-pages-test-b".into());

    let mi = MidiInput::new("loopback-test-in").expect("MidiInput");
    let in_port = mi
        .ports()
        .into_iter()
        .find(|p| mi.port_name(p).unwrap_or_default() == needle_b)
        .unwrap_or_else(|| panic!("no input port named {needle_b}"));

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let _conn_in = mi
        .connect(&in_port, "loopback-test-in", move |_, msg, _| {
            let _ = tx.send(msg.to_vec());
        }, ())
        .expect("connect input");

    let mo = MidiOutput::new("loopback-test-out").expect("MidiOutput");
    let out_port = mo
        .ports()
        .into_iter()
        .find(|p| mo.port_name(p).unwrap_or_default() == needle_a)
        .unwrap_or_else(|| panic!("no output port named {needle_a}"));
    let mut conn_out = mo.connect(&out_port, "loopback-test-out").expect("connect output");

    let payload = vec![0x90, 60, 100]; // Note On, channel 1, note 60, velocity 100
    println!("sending {payload:02X?} to `{needle_a}`...");
    conn_out.send(&payload).expect("send");

    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(got) => {
            if got == payload {
                println!("ROUND-TRIP OK: received {got:02X?} on `{needle_b}`");
            } else {
                println!("MISMATCH: sent {payload:02X?} got {got:02X?}");
                std::process::exit(2);
            }
        }
        Err(_) => {
            println!("TIMEOUT: nothing received on `{needle_b}` within 2s");
            std::process::exit(1);
        }
    }
}
