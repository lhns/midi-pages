//! Open one MIDI input and print every message that arrives.
//! Run: `cargo run --release --example listen_port -- launchpad-mini-mk3-page1-in`

use std::time::Duration;

use midir::MidiInput;

fn main() {
    let needle = std::env::args()
        .nth(1)
        .expect("usage: listen_port <port-substring>");
    let mi = MidiInput::new("listen_port-in").expect("MidiInput");
    let port = mi
        .ports()
        .into_iter()
        .find(|p| mi.port_name(p).unwrap_or_default().contains(&needle))
        .unwrap_or_else(|| panic!("no input port matching {needle}"));
    let name = mi.port_name(&port).unwrap_or_default();
    println!("listening on `{name}` (Ctrl-C to exit)...");
    let _conn = mi
        .connect(
            &port,
            "listen_port-in",
            move |stamp, msg, _| println!("{stamp:>10} {msg:02X?}"),
            (),
        )
        .expect("connect");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
