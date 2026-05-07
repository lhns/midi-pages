use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use tracing::{error, info, warn};

use midi_pages::config::{Config, DeviceConfig};
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
    /// Path to config.toml.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// List available MIDI ports and exit.
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

    // Output connections (writers).
    let host_out = ports::open_output_named(
        &format!("midi-pages-{}-host-out", cfg.name),
        &cfg.host_port_out,
    )?;
    let device_out = ports::open_output_named(
        &format!("midi-pages-{}-device-out", cfg.name),
        &cfg.port_match,
    )?;

    let host_out = Arc::new(Mutex::new(host_out));
    let device_out = Arc::new(Mutex::new(device_out));

    // Send boot SysEx (e.g. programmer mode) plus the device's own boot bytes.
    {
        let mut dev = device_out.lock().unwrap();
        if let Some(sysex) = &cfg.boot_sysex {
            let _ = dev.send(sysex);
        }
        for bytes in make_device(cfg.driver).boot() {
            let _ = dev.send(&bytes);
        }
    }
    // Paint initial indicators.
    {
        let p = proxy.lock().unwrap();
        let initial = p.device().paint_indicators(0, &cfg.indicator_leds);
        let mut dev = device_out.lock().unwrap();
        for bytes in initial {
            let _ = dev.send(&bytes);
        }
    }

    // Device -> proxy -> host.
    let (mi_dev, dev_port) = ports::open_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        &cfg.port_match,
    )?;
    let p_dev = Arc::clone(&proxy);
    let host_out_dev = Arc::clone(&host_out);
    let device_out_dev = Arc::clone(&device_out);
    let _conn_dev = mi_dev
        .connect(
            &dev_port,
            "midi-pages-device-in",
            move |_, msg, _| {
                let mut proxy = p_dev.lock().unwrap();
                let outs = proxy.handle_device_in(msg);
                drop(proxy);
                dispatch(&outs, &host_out_dev, &device_out_dev);
            },
            (),
        )
        .map_err(|e| anyhow!("connect device input: {e}"))?;

    // Host -> proxy -> device.
    let (mi_host, host_port) = ports::open_input(
        &format!("midi-pages-{}-host-in", cfg.name),
        &cfg.host_port_in,
    )?;
    let p_host = Arc::clone(&proxy);
    let host_out_host = Arc::clone(&host_out);
    let device_out_host = Arc::clone(&device_out);
    let _conn_host = mi_host
        .connect(
            &host_port,
            "midi-pages-host-in",
            move |_, msg, _| {
                let mut proxy = p_host.lock().unwrap();
                let outs = proxy.handle_host_in(msg);
                drop(proxy);
                dispatch(&outs, &host_out_host, &device_out_host);
            },
            (),
        )
        .map_err(|e| anyhow!("connect host input: {e}"))?;

    info!(device = %cfg.name, "running. Ctrl-C to exit.");
    // Park the thread; the connection callbacks own the I/O.
    loop {
        thread::park();
    }
}

fn dispatch(
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
        }
    }
}
