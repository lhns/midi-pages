use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use tracing::{error, info, warn};

use midi_pages::config::{Config, DeviceConfig, Mode};
use midi_pages::midi::apc_mini::ApcMini;
use midi_pages::midi::device::{Device, Driver};
use midi_pages::midi::mini_mk3::MiniMk3;
use midi_pages::ports;
use midi_pages::proxy::{Out, Proxy};

#[derive(Debug, Parser)]
#[command(
    name = "midi-pages",
    about = "Virtual MIDI paging proxy for grid controllers"
)]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(long)]
    list_ports: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    if args.list_ports {
        for line in ports::list_ports()? {
            println!("{line}");
        }
        return Ok(());
    }

    let cfg = Config::load(&args.config)
        .with_context(|| format!("load config {}", args.config.display()))?;

    info!("loaded {} device profile(s)", cfg.devices.len());

    let mut handles = Vec::new();
    for d in cfg.devices {
        let h = thread::Builder::new()
            .name(format!("midi-pages:{}", d.name))
            .spawn(move || {
                if let Err(e) = run_device(&d) {
                    error!(device = %d.name, error = %e, "device thread exited");
                }
            })?;
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn make_device(driver: Driver) -> Box<dyn Device> {
    match driver {
        Driver::MiniMk3 => Box::new(MiniMk3),
        Driver::ApcMini => Box::new(ApcMini),
    }
}

fn run_device(cfg: &DeviceConfig) -> Result<()> {
    let device = make_device(cfg.driver);
    let proxy = Arc::new(Mutex::new(Proxy::new(cfg, device)));

    // Open device side (real USB MIDI) — the same in either mode.
    let device_out = Arc::new(Mutex::new(ports::open_output_named(
        &format!("midi-pages-{}-device-out", cfg.name),
        &cfg.port_match,
    )?));

    // Boot the device (programmer mode etc.).
    {
        let mut dev = device_out.lock().unwrap();
        if let Some(sysex) = &cfg.boot_sysex {
            let _ = dev.send(sysex);
        }
        for bytes in make_device(cfg.driver).boot() {
            let _ = dev.send(&bytes);
        }
    }
    {
        let p = proxy.lock().unwrap();
        let initial = p.device().paint_indicators(0, &cfg.indicator_leds);
        let mut dev = device_out.lock().unwrap();
        for bytes in initial {
            let _ = dev.send(&bytes);
        }
    }

    // Wire host-side I/O per mode.
    match cfg.mode {
        Mode::NoteOffset => run_note_offset(cfg, proxy, device_out),
        Mode::PerPort => run_per_port(cfg, proxy, device_out),
    }
}

fn run_note_offset(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    device_out: Arc<Mutex<midir::MidiOutputConnection>>,
) -> Result<()> {
    let host_in_name = cfg
        .host_port_in
        .as_deref()
        .ok_or_else(|| anyhow!("note_offset mode requires host_port_in"))?;
    let host_out_name = cfg
        .host_port_out
        .as_deref()
        .ok_or_else(|| anyhow!("note_offset mode requires host_port_out"))?;

    ports::ensure_host_ports(host_in_name, host_out_name)?;

    let host_out: Arc<Mutex<midir::MidiOutputConnection>> = Arc::new(Mutex::new(
        open_or_virtual_output(&format!("midi-pages-{}-host-out", cfg.name), host_out_name)?,
    ));

    // Device -> proxy -> host.
    let p_dev = Arc::clone(&proxy);
    let host_out_dev = Arc::clone(&host_out);
    let device_out_dev = Arc::clone(&device_out);
    let _device_in = open_or_virtual_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        &cfg.port_match,
        move |msg| {
            let outs = {
                let mut p = p_dev.lock().unwrap();
                p.handle_device_in(msg)
            };
            dispatch_offset(&outs, &host_out_dev, &device_out_dev);
        },
        false, // never create the *device* port; the device is real hardware.
    )?;

    // Host -> proxy -> device.
    let p_host = Arc::clone(&proxy);
    let host_out_host = Arc::clone(&host_out);
    let device_out_host = Arc::clone(&device_out);
    let _host_in = open_or_virtual_input(
        &format!("midi-pages-{}-host-in", cfg.name),
        host_in_name,
        move |msg| {
            let outs = {
                let mut p = p_host.lock().unwrap();
                p.handle_host_in(msg)
            };
            dispatch_offset(&outs, &host_out_host, &device_out_host);
        },
        true,
    )?;

    info!(device = %cfg.name, mode = "note_offset", "running. Ctrl-C to exit.");
    loop {
        thread::park();
    }
}

fn run_per_port(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    device_out: Arc<Mutex<midir::MidiOutputConnection>>,
) -> Result<()> {
    let pages = cfg.pages;
    let port_names = cfg.page_port_names();

    // Pre-create all needed virtual port pairs (Windows: loopMIDI CLI).
    for (in_name, out_name) in &port_names {
        ports::ensure_host_ports(in_name, out_name)?;
    }

    // Open all the per-page output connections.
    let mut host_outs: Vec<Arc<Mutex<midir::MidiOutputConnection>>> = Vec::new();
    for (page_idx, (_, out_name)) in port_names.iter().enumerate() {
        let conn = open_or_virtual_output(
            &format!("midi-pages-{}-page{}-out", cfg.name, page_idx + 1),
            out_name,
        )?;
        host_outs.push(Arc::new(Mutex::new(conn)));
    }
    let host_outs = Arc::new(host_outs);

    // Device -> proxy -> currently-active host port.
    let p_dev = Arc::clone(&proxy);
    let host_outs_dev = Arc::clone(&host_outs);
    let device_out_dev = Arc::clone(&device_out);
    let _device_in = open_or_virtual_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        &cfg.port_match,
        move |msg| {
            let outs = {
                let mut p = p_dev.lock().unwrap();
                p.handle_device_in(msg)
            };
            dispatch_per_port(&outs, &host_outs_dev, &device_out_dev);
        },
        false,
    )?;

    // One Host->proxy listener per page, tagging each message with its page.
    let mut _host_inputs = Vec::new();
    for (page_idx, (in_name, _)) in port_names.iter().enumerate() {
        let p = Arc::clone(&proxy);
        let host_outs = Arc::clone(&host_outs);
        let device_out = Arc::clone(&device_out);
        let page = page_idx as u8;
        let conn = open_or_virtual_input(
            &format!("midi-pages-{}-page{}-in", cfg.name, page_idx + 1),
            in_name,
            move |msg| {
                let outs = {
                    let mut p = p.lock().unwrap();
                    p.handle_host_in_per_port(page, msg)
                };
                dispatch_per_port(&outs, &host_outs, &device_out);
            },
            true,
        )?;
        _host_inputs.push(conn);
    }

    info!(
        device = %cfg.name,
        mode = "per_port",
        pages,
        "running. Ctrl-C to exit."
    );
    loop {
        thread::park();
    }
}

fn dispatch_offset(
    outs: &[Out],
    host_out: &Arc<Mutex<midir::MidiOutputConnection>>,
    device_out: &Arc<Mutex<midir::MidiOutputConnection>>,
) {
    for o in outs {
        match o {
            Out::ToHost(b) => {
                if let Err(e) = host_out.lock().unwrap().send(b) {
                    warn!("host send: {e}");
                }
            }
            Out::ToDevice(b) => {
                if let Err(e) = device_out.lock().unwrap().send(b) {
                    warn!("device send: {e}");
                }
            }
            Out::ToHostPage { .. } => {
                warn!("got ToHostPage in note_offset mode; dropping");
            }
        }
    }
}

fn dispatch_per_port(
    outs: &[Out],
    host_outs: &Arc<Vec<Arc<Mutex<midir::MidiOutputConnection>>>>,
    device_out: &Arc<Mutex<midir::MidiOutputConnection>>,
) {
    for o in outs {
        match o {
            Out::ToHostPage { page, bytes } => {
                if let Some(conn) = host_outs.get(*page as usize)
                    && let Err(e) = conn.lock().unwrap().send(bytes)
                {
                    warn!(page = %page, "host send: {e}");
                }
            }
            Out::ToDevice(b) => {
                if let Err(e) = device_out.lock().unwrap().send(b) {
                    warn!("device send: {e}");
                }
            }
            Out::ToHost(_) => {
                warn!("got ToHost in per_port mode; dropping");
            }
        }
    }
}

// -- Platform-specific I/O wrappers ---------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_or_virtual_input<F>(
    client: &str,
    needle: &str,
    callback: F,
    allow_virtual: bool,
) -> Result<midir::MidiInputConnection<()>>
where
    F: Fn(&[u8]) + Send + 'static,
{
    let mi = midir::MidiInput::new(client)?;
    if let Ok(port) = ports::find_input(&mi, needle) {
        return mi
            .connect(&port, client, move |_, msg, _| callback(msg), ())
            .map_err(|e| anyhow!("connect input `{needle}`: {e}"));
    }
    if allow_virtual {
        info!(port = %needle, "creating virtual MIDI input port");
        return mi
            .create_virtual(needle, move |_, msg, _| callback(msg), ())
            .map_err(|e| anyhow!("create virtual input `{needle}`: {e}"));
    }
    Err(anyhow!(
        "no MIDI input port matching `{needle}` and virtual creation not allowed here"
    ))
}

#[cfg(target_os = "windows")]
fn open_or_virtual_input<F>(
    client: &str,
    needle: &str,
    callback: F,
    _allow_virtual: bool,
) -> Result<midir::MidiInputConnection<()>>
where
    F: Fn(&[u8]) + Send + 'static,
{
    let mi = midir::MidiInput::new(client)?;
    let port = ports::find_input(&mi, needle)?;
    mi.connect(&port, client, move |_, msg, _| callback(msg), ())
        .map_err(|e| anyhow!("connect input `{needle}`: {e}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_or_virtual_output(client: &str, needle: &str) -> Result<midir::MidiOutputConnection> {
    let mo = midir::MidiOutput::new(client)?;
    if let Ok(port) = ports::find_output(&mo, needle) {
        return mo
            .connect(&port, client)
            .map_err(|e| anyhow!("connect output `{needle}`: {e}"));
    }
    info!(port = %needle, "creating virtual MIDI output port");
    mo.create_virtual(needle)
        .map_err(|e| anyhow!("create virtual output `{needle}`: {e}"))
}

#[cfg(target_os = "windows")]
fn open_or_virtual_output(client: &str, needle: &str) -> Result<midir::MidiOutputConnection> {
    ports::open_output_named(client, needle)
}
