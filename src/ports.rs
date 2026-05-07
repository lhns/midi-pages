//! Thin wrappers over `midir` for port discovery and matching, plus
//! cross-platform helpers to **create** virtual host-side ports:
//!
//! - On Linux & macOS: `midir`'s built-in `create_virtual_*` API.
//! - On Windows: shell out to `loopMIDI.exe -new "<name>"`. The user must have
//!   loopMIDI installed (and ideally its GUI running so the port persists).

use anyhow::{Context, Result, anyhow};
use midir::{MidiInput, MidiInputPort, MidiOutput, MidiOutputPort};
use tracing::{info, warn};

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

/// Best-effort: ensure a virtual host-side port pair (in + out) with the given
/// names exists. On platforms / paths where we can't create them, we just
/// return Ok(()) and rely on the caller's later `open_*` calls to surface a
/// helpful error.
pub fn ensure_host_ports(in_name: &str, out_name: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        ensure_loopmidi_port(in_name)?;
        ensure_loopmidi_port(out_name)?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        // On Linux/macOS we don't pre-create — the proxy's `connect_virtual_*`
        // creates the port at the moment of registration.
        let _ = (in_name, out_name);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn ensure_loopmidi_port(name: &str) -> Result<()> {
    // First, see if it already exists.
    if loopmidi_port_exists(name)? {
        return Ok(());
    }
    let exe = find_loopmidi_exe().context(
        "loopMIDI.exe not found in PATH or default install locations. \
         Install loopMIDI from https://www.tobias-erichsen.de/software/loopmidi.html, \
         then re-run this program.",
    )?;
    info!(port = %name, "creating loopMIDI port via {}", exe);
    let status = std::process::Command::new(&exe)
        .args(["-new", name])
        .status()
        .with_context(|| format!("invoke {exe}"))?;
    if !status.success() {
        warn!(port = %name, "loopMIDI -new exit status {status:?}");
    }
    // Give loopMIDI a moment to register the port with WinMM.
    std::thread::sleep(std::time::Duration::from_millis(500));
    if !loopmidi_port_exists(name)? {
        return Err(anyhow!(
            "tried to create loopMIDI port `{name}` but it didn't appear afterwards. \
             Make sure the loopMIDI GUI is running (the CLI requires it)."
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn loopmidi_port_exists(name: &str) -> Result<bool> {
    let mi = MidiInput::new("midi-pages-probe-in")?;
    for port in mi.ports() {
        if mi.port_name(&port).unwrap_or_default() == name {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "windows")]
fn find_loopmidi_exe() -> Option<String> {
    let candidates = [
        "loopMIDI.exe",
        r"C:\Program Files (x86)\Tobias Erichsen\loopMIDI\loopMIDI.exe",
        r"C:\Program Files\Tobias Erichsen\loopMIDI\loopMIDI.exe",
    ];
    for c in candidates {
        if std::path::Path::new(c).exists() {
            return Some(c.to_string());
        }
        if !c.contains('\\') && in_path(c) {
            return Some(c.to_string());
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn in_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    for dir in path.split(';') {
        let candidate = std::path::Path::new(dir).join(name);
        if candidate.exists() {
            return true;
        }
    }
    false
}
