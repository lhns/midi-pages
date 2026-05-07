//! Thin wrappers over `midir` for port discovery and matching.

use anyhow::{Context, Result, anyhow};
use midir::{MidiInput, MidiInputPort, MidiOutput, MidiOutputPort};

pub fn list_ports() -> Result<Vec<String>> {
    let mut all = Vec::new();
    let mi = MidiInput::new("midi-pages-list-in")?;
    for port in mi.ports() {
        if let Ok(name) = mi.port_name(&port) {
            all.push(format!("[in]  {name}"));
        }
    }
    let mo = MidiOutput::new("midi-pages-list-out")?;
    for port in mo.ports() {
        if let Ok(name) = mo.port_name(&port) {
            all.push(format!("[out] {name}"));
        }
    }
    Ok(all)
}

pub fn find_input(mi: &MidiInput, needle: &str) -> Result<MidiInputPort> {
    for port in mi.ports() {
        let name = mi.port_name(&port).unwrap_or_default();
        if name.contains(needle) {
            return Ok(port);
        }
    }
    Err(anyhow!(
        "no MIDI input port matching `{needle}` found. Run with --list-ports to see available ports."
    ))
}

pub fn find_output(mo: &MidiOutput, needle: &str) -> Result<MidiOutputPort> {
    for port in mo.ports() {
        let name = mo.port_name(&port).unwrap_or_default();
        if name.contains(needle) {
            return Ok(port);
        }
    }
    Err(anyhow!(
        "no MIDI output port matching `{needle}` found. Run with --list-ports to see available ports."
    ))
}

pub fn open_input(client: &str, needle: &str) -> Result<(MidiInput, MidiInputPort)> {
    let mi = MidiInput::new(client).context("create MidiInput")?;
    let port = find_input(&mi, needle)?;
    Ok((mi, port))
}

pub fn open_output_named(client: &str, needle: &str) -> Result<midir::MidiOutputConnection> {
    let mo = MidiOutput::new(client).context("create MidiOutput")?;
    let port = find_output(&mo, needle)?;
    mo.connect(&port, client)
        .map_err(|e| anyhow!("connect output `{needle}`: {e}"))
}
