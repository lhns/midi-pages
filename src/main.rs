use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Global shutdown flag set by the Ctrl-C handler. Per-device worker threads
/// poll this and return cleanly so their stack-allocated WMS resources (the
/// `WindowsVirtualHostPort`s on Windows) are dropped — without this, force
/// exit leaks Virtual Device registrations into `midisrv` and eventually
/// wedges the MIDI subsystem (requires a reboot to recover).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Per-device worker thread handles. The Win32 console handler and the
/// named-event watcher both read this to unpark workers when shutdown is
/// requested. Populated once at startup.
#[cfg(target_os = "windows")]
static THREADS: std::sync::OnceLock<Vec<thread::Thread>> = std::sync::OnceLock::new();

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
    /// Open all configured Windows host ports, sleep briefly, drop them, exit.
    /// Used to validate the Drop chain without booting DasLight. After running
    /// this, `--list-ports` should return promptly with no surviving
    /// `*-page*` endpoints. Windows-only and no-op on other platforms.
    #[arg(long)]
    shutdown_smoke: bool,
    /// Spawn an elevated PowerShell to `Restart-Service midisrv`. Recovery
    /// escape hatch when midisrv has wedged despite our cleanup chain.
    /// Pops a UAC prompt; we do not auto-elevate silently.
    #[arg(long)]
    restart_midisrv: bool,

    /// Send a graceful-shutdown signal to a running midi-pages.exe and
    /// return. With no argument, auto-discovers a single running instance
    /// (fails if zero or multiple exist). With a pid argument, targets
    /// that specific instance. Windows-only.
    #[arg(long, value_name = "PID", num_args = 0..=1, default_missing_value = "0")]
    stop: Option<u32>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Set a panic hook that flags shutdown and gives worker threads a moment
    // to run their Drop chains (which release WMS resources). If we panic
    // without this, the process bails before midisrv has processed our
    // ref-drops and the Virtual Device registration leaks.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        SHUTDOWN.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1_000));
    }));

    let args = Args::parse();

    if args.list_ports {
        for line in ports::list_ports()? {
            println!("{line}");
        }
        return Ok(());
    }

    if args.restart_midisrv {
        return restart_midisrv();
    }

    if let Some(raw) = args.stop {
        let target = if raw == 0 { None } else { Some(raw) };
        return graceful_stop(target);
    }

    let cfg = Config::load(&args.config)
        .with_context(|| format!("load config {}", args.config.display()))?;

    if args.shutdown_smoke {
        return shutdown_smoke(&cfg);
    }

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

    // Install a shutdown handler that flips SHUTDOWN and unparks all workers
    // so they return cleanly and Drop releases WMS resources. On Windows we
    // hook *every* console control event (Ctrl+C, Ctrl+Break, Close button
    // on the terminal, Logoff, Shutdown) so any user-driven exit path runs
    // cleanup. On other platforms we use the `ctrlc` crate (SIGINT / SIGTERM).
    let thread_handles: Vec<thread::Thread> = handles.iter().map(|h| h.thread().clone()).collect();
    install_shutdown_handler(thread_handles);
    #[cfg(target_os = "windows")]
    install_shutdown_event_watcher();

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn trigger_shutdown(threads: &[thread::Thread]) {
    SHUTDOWN.store(true, Ordering::SeqCst);
    for t in threads {
        t.unpark();
    }
}

#[cfg(not(target_os = "windows"))]
fn install_shutdown_handler(threads: Vec<thread::Thread>) {
    if let Err(e) = ctrlc::set_handler(move || {
        info!("shutdown signal received");
        trigger_shutdown(&threads);
    }) {
        warn!(
            "failed to install signal handler: {e}. Force-killing this process may leak resources."
        );
    }
}

#[cfg(target_os = "windows")]
fn install_shutdown_handler(threads: Vec<thread::Thread>) {
    if THREADS.set(threads).is_err() {
        warn!("shutdown handler already installed");
        return;
    }

    // Win32 console control event types.
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    type PhandlerRoutine = unsafe extern "system" fn(ctrl_type: u32) -> i32;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<PhandlerRoutine>, add: i32) -> i32;
    }

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT => {
                info!(ctrl_type, "shutdown signal received");
                if let Some(threads) = THREADS.get() {
                    trigger_shutdown(threads);
                }
                // Sleep before returning so the worker threads have time to
                // wake from `park_timeout`, unwind, run `WindowsHostPort::Drop`
                // (which calls `MidiSession::Close` + `MidiVirtualDevice::Cleanup`),
                // and let `IUnknown::Release` finish before the process exits.
                // For CTRL_CLOSE_EVENT Windows hard-terminates after ~5 s, so we
                // can't sleep longer than that; 2.5 s is a comfortable margin.
                std::thread::sleep(std::time::Duration::from_millis(2_500));
                1 // TRUE = handled, don't pass to next handler
            }
            _ => 0, // not handled
        }
    }

    let rc = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    if rc == 0 {
        warn!(
            "SetConsoleCtrlHandler failed: {}. Force-killing this process may leak WMS resources.",
            std::io::Error::last_os_error()
        );
    }
}

/// Park the current thread until the global SHUTDOWN flag is set (by the
/// Ctrl-C handler). Wakes periodically as a safety belt; the handler also
/// explicitly unparks every worker thread so wake-up is immediate.
fn wait_for_shutdown() {
    while !SHUTDOWN.load(Ordering::SeqCst) {
        thread::park_timeout(Duration::from_secs(60));
    }
}

fn make_device(driver: Driver) -> Box<dyn Device> {
    match driver {
        Driver::MiniMk3 => Box::new(MiniMk3),
        Driver::ApcMini => Box::new(ApcMini),
    }
}

fn run_device(cfg: &DeviceConfig) -> Result<()> {
    info!(device = %cfg.name, "run_device: start");
    let device = make_device(cfg.driver);
    let proxy = Arc::new(Mutex::new(Proxy::new(cfg, device)));

    info!(device = %cfg.name, port_match = %cfg.port_match, "opening real device output");
    // Open device side (real USB MIDI) — the same in either mode.
    let device_out = Arc::new(Mutex::new(ports::open_output_named(
        &format!("midi-pages-{}-device-out", cfg.name),
        cfg.port_match.output(),
    )?));
    info!("real device output opened");

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
        let initial = p.paint_indicator_state();
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

// =========================================================================
// Windows: WMS Virtual Device (one endpoint per page, plugin callback).
// =========================================================================

#[cfg(target_os = "windows")]
fn run_note_offset(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    device_out: Arc<Mutex<midir::MidiOutputConnection>>,
) -> Result<()> {
    let host_name = cfg
        .host_port_in
        .as_deref()
        .ok_or_else(|| anyhow!("note_offset mode requires host_port_in"))?;

    // Set up host side: ONE Virtual Device endpoint, plugin callback writes
    // into the proxy; outgoing goes via .send() on the same handle.
    let p_host = Arc::clone(&proxy);
    let device_out_host = Arc::clone(&device_out);
    // host_port.send is needed from the closure (for proxy outputs heading to
    // host). We must construct it AFTER setting up the Arc<...>, but the
    // closure also needs to call its send method — so we share it via Arc.
    let host_port: Arc<Mutex<Option<ports::WindowsHostPort>>> = Arc::new(Mutex::new(None));
    let host_port_for_cb = Arc::clone(&host_port);
    let host_port_obj = ports::WindowsHostPort::create(host_name, move |msg| {
        let outs = {
            let mut p = p_host.lock().unwrap();
            p.handle_host_in(msg)
        };
        dispatch_offset_windows(&outs, &host_port_for_cb, &device_out_host);
    })?;
    *host_port.lock().unwrap() = Some(host_port_obj);

    // Device -> proxy -> host.
    let p_dev = Arc::clone(&proxy);
    let host_port_dev = Arc::clone(&host_port);
    let device_out_dev = Arc::clone(&device_out);
    let _device_in = open_device_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        cfg.port_match.input(),
        move |msg| {
            let outs = {
                let mut p = p_dev.lock().unwrap();
                p.handle_device_in(msg)
            };
            dispatch_offset_windows(&outs, &host_port_dev, &device_out_dev);
        },
    )?;

    info!(device = %cfg.name, mode = "note_offset", "running. Ctrl-C to exit.");
    wait_for_shutdown();
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_per_port(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    device_out: Arc<Mutex<midir::MidiOutputConnection>>,
) -> Result<()> {
    let pages = cfg.pages;
    let port_names = cfg.page_port_names();

    // ONE Virtual Device endpoint per page; the proxy reads via its plugin
    // callback and writes via the same handle's `.send()`.
    let host_ports: Arc<Mutex<Vec<Option<ports::WindowsHostPort>>>> =
        Arc::new(Mutex::new((0..pages as usize).map(|_| None).collect()));

    // Create all page endpoints in parallel. Each `WindowsHostPort::create`
    // spends ~5 s polling midisrv's WinMM bridge for the new endpoint to
    // appear in enumeration; running them concurrently turns ~pages × 5 s
    // into roughly max(per-port) instead of the sum.
    thread::scope(|s| -> Result<()> {
        let mut handles = Vec::with_capacity(port_names.len());
        for (page_idx, host_name) in port_names.iter().enumerate() {
            let p = Arc::clone(&proxy);
            let host_ports_for_cb = Arc::clone(&host_ports);
            let device_out_for_cb = Arc::clone(&device_out);
            let host_ports_insert = Arc::clone(&host_ports);
            let page = page_idx as u8;
            handles.push(s.spawn(move || -> Result<()> {
                let port = ports::WindowsHostPort::create(host_name, move |msg| {
                    let outs = {
                        let mut p = p.lock().unwrap();
                        p.handle_host_in_per_port(page, msg)
                    };
                    dispatch_per_port_windows(&outs, &host_ports_for_cb, &device_out_for_cb);
                })?;
                host_ports_insert.lock().unwrap()[page_idx] = Some(port);
                Ok(())
            }));
        }
        for h in handles {
            h.join()
                .map_err(|_| anyhow!("host port creation thread panicked"))??;
        }
        Ok(())
    })?;

    // Device -> proxy -> currently-active host port.
    let p_dev = Arc::clone(&proxy);
    let host_ports_dev = Arc::clone(&host_ports);
    let device_out_dev = Arc::clone(&device_out);
    let _device_in = open_device_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        cfg.port_match.input(),
        move |msg| {
            let outs = {
                let mut p = p_dev.lock().unwrap();
                p.handle_device_in(msg)
            };
            dispatch_per_port_windows(&outs, &host_ports_dev, &device_out_dev);
        },
    )?;

    info!(
        device = %cfg.name,
        mode = "per_port",
        pages,
        "running. Ctrl-C to exit."
    );
    wait_for_shutdown();
    Ok(())
}

#[cfg(target_os = "windows")]
fn dispatch_offset_windows(
    outs: &[Out],
    host_port: &Arc<Mutex<Option<ports::WindowsHostPort>>>,
    device_out: &Arc<Mutex<midir::MidiOutputConnection>>,
) {
    for o in outs {
        match o {
            Out::ToHost(b) => {
                if let Some(p) = host_port.lock().unwrap().as_ref()
                    && let Err(e) = p.send(b)
                {
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

#[cfg(target_os = "windows")]
fn dispatch_per_port_windows(
    outs: &[Out],
    host_ports: &Arc<Mutex<Vec<Option<ports::WindowsHostPort>>>>,
    device_out: &Arc<Mutex<midir::MidiOutputConnection>>,
) {
    for o in outs {
        match o {
            Out::ToHostPage { page, bytes } => {
                let guard = host_ports.lock().unwrap();
                if let Some(Some(p)) = guard.get(*page as usize)
                    && let Err(e) = p.send(bytes)
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

#[cfg(target_os = "windows")]
fn open_device_input<F>(
    client: &str,
    needle: &str,
    callback: F,
) -> Result<midir::MidiInputConnection<()>>
where
    F: Fn(&[u8]) + Send + 'static,
{
    let mi = midir::MidiInput::new(client)?;
    let port = ports::find_input(&mi, needle)?;
    mi.connect(&port, client, move |_, msg, _| callback(msg), ())
        .map_err(|e| anyhow!("connect input `{needle}`: {e}"))
}

// =========================================================================
// Linux / macOS: midir's `create_virtual_*` path. Each page exposes one
// virtual port name; midir creates separate input + output sub-ports under
// it (the DAW sees one bidirectional device).
// =========================================================================

#[cfg(not(target_os = "windows"))]
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

    let host_out: Arc<Mutex<midir::MidiOutputConnection>> = Arc::new(Mutex::new(
        open_or_virtual_output(&format!("midi-pages-{}-host-out", cfg.name), host_out_name)?,
    ));

    let p_dev = Arc::clone(&proxy);
    let host_out_dev = Arc::clone(&host_out);
    let device_out_dev = Arc::clone(&device_out);
    let _device_in = open_or_virtual_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        cfg.port_match.input(),
        move |msg| {
            let outs = {
                let mut p = p_dev.lock().unwrap();
                p.handle_device_in(msg)
            };
            dispatch_offset(&outs, &host_out_dev, &device_out_dev);
        },
        false,
    )?;

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
    wait_for_shutdown();
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_per_port(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    device_out: Arc<Mutex<midir::MidiOutputConnection>>,
) -> Result<()> {
    let pages = cfg.pages;
    let port_names = cfg.page_port_names();

    // Each page exposes ONE virtual port name. midir on Unix creates separate
    // input + output sub-ports under the same name (the DAW sees one device
    // with both directions).
    let mut host_outs: Vec<Arc<Mutex<midir::MidiOutputConnection>>> = Vec::new();
    for (page_idx, name) in port_names.iter().enumerate() {
        let conn = open_or_virtual_output(
            &format!("midi-pages-{}-page{}-out", cfg.name, page_idx + 1),
            name,
        )?;
        host_outs.push(Arc::new(Mutex::new(conn)));
    }
    let host_outs = Arc::new(host_outs);

    let p_dev = Arc::clone(&proxy);
    let host_outs_dev = Arc::clone(&host_outs);
    let device_out_dev = Arc::clone(&device_out);
    let _device_in = open_or_virtual_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        cfg.port_match.input(),
        move |msg| {
            let outs = {
                let mut p = p_dev.lock().unwrap();
                p.handle_device_in(msg)
            };
            dispatch_per_port(&outs, &host_outs_dev, &device_out_dev);
        },
        false,
    )?;

    let mut _host_inputs = Vec::new();
    for (page_idx, name) in port_names.iter().enumerate() {
        let p = Arc::clone(&proxy);
        let host_outs = Arc::clone(&host_outs);
        let device_out = Arc::clone(&device_out);
        let page = page_idx as u8;
        let conn = open_or_virtual_input(
            &format!("midi-pages-{}-page{}-in", cfg.name, page_idx + 1),
            name,
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

    info!(device = %cfg.name, mode = "per_port", pages, "running. Ctrl-C to exit.");
    wait_for_shutdown();
    Ok(())
}

#[cfg(not(target_os = "windows"))]
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

#[cfg(not(target_os = "windows"))]
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
    use midir::os::unix::VirtualInput;
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

// =========================================================================
// Recovery / smoke-test subcommands.
// =========================================================================

#[cfg(target_os = "windows")]
fn shutdown_smoke(cfg: &Config) -> Result<()> {
    info!("shutdown_smoke: opening all configured host ports");
    let mut ports = Vec::new();
    for d in &cfg.devices {
        match d.mode {
            Mode::PerPort => {
                for name in d.page_port_names() {
                    info!(port = %name, "open");
                    ports.push(ports::WindowsHostPort::create(&name, |_| {})?);
                }
            }
            Mode::NoteOffset => {
                if let Some(name) = d.host_port_in.as_deref() {
                    info!(port = %name, "open");
                    ports.push(ports::WindowsHostPort::create(name, |_| {})?);
                }
            }
        }
    }
    info!(count = ports.len(), "opened; settling 500 ms");
    thread::sleep(Duration::from_millis(500));
    info!("dropping all ports (exercises Drop chain)");
    drop(ports);
    info!("dropped; waiting 500 ms for midisrv to settle");
    thread::sleep(Duration::from_millis(500));
    info!("shutdown_smoke complete; now run --list-ports to confirm no ghosts");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn shutdown_smoke(_cfg: &Config) -> Result<()> {
    info!("shutdown_smoke is a no-op on non-Windows platforms");
    Ok(())
}

#[cfg(target_os = "windows")]
fn restart_midisrv() -> Result<()> {
    info!("spawning elevated PowerShell to restart midisrv (accept the UAC prompt)");
    // Start-Process -Verb RunAs triggers the UAC prompt; the inner command
    // calls Restart-Service. We do NOT silently elevate or Stop-Process the
    // service from this process — the user explicitly clicks Allow.
    let status = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile','-Command','Restart-Service midisrv'",
        ])
        .status()
        .context("spawn powershell.exe")?;
    if !status.success() {
        return Err(anyhow!("powershell exited with {status}"));
    }
    info!("UAC dispatched. Once midisrv has restarted, re-run midi-pages normally.");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn restart_midisrv() -> Result<()> {
    Err(anyhow!("--restart-midisrv is Windows-only"))
}

#[cfg(target_os = "windows")]
fn graceful_stop(target_pid: Option<u32>) -> Result<()> {
    use midi_pages::shutdown;

    let pid = match target_pid {
        Some(p) => p,
        None => {
            let mut pids = find_proxy_pids_except_self()?;
            match pids.len() {
                0 => {
                    info!("no other midi-pages.exe instance found; nothing to stop");
                    return Ok(());
                }
                1 => pids.remove(0),
                _ => {
                    return Err(anyhow!(
                        "{} midi-pages.exe instances running ({:?}). Re-run with --stop <pid>.",
                        pids.len(),
                        pids
                    ));
                }
            }
        }
    };

    let (handle, namespace) = shutdown::open_shutdown_event(pid).ok_or_else(|| {
        anyhow!(
            "could not open shutdown event for PID {pid} in Global\\ or Local\\. \
             Is a midi-pages proxy with the event watcher actually running there?"
        )
    })?;
    info!(target_pid = pid, namespace = %namespace.prefix(), "signalling shutdown event");
    shutdown::signal_event(handle).map_err(|e| anyhow!("SetEvent failed: {e}"))?;

    // Match the proxy's handler 2.5 s settle window plus a small margin.
    thread::sleep(Duration::from_millis(3000));
    info!("graceful_stop complete");
    Ok(())
}

/// Spawn a Win32 named-event watcher thread. The thread blocks on
/// `WaitForSingleObject` until something else opens the event by name and
/// `SetEvent`s it (typically `midi-pages --stop`), then flips `SHUTDOWN`
/// and unparks all workers, identical to what the console handler does
/// for Ctrl+C.
#[cfg(target_os = "windows")]
fn install_shutdown_event_watcher() {
    use midi_pages::shutdown;

    let pid = std::process::id();
    let created = match shutdown::create_shutdown_event(pid) {
        Some(c) => c,
        None => {
            warn!(
                "could not create shutdown event in Global\\ or Local\\; \
                 `midi-pages --stop` will not be able to reach this process"
            );
            return;
        }
    };
    info!(
        namespace = %created.namespace.prefix(),
        event_name = %shutdown::event_name(created.namespace, pid),
        "shutdown event watcher armed"
    );
    let handle = created.handle;
    let _ = thread::Builder::new()
        .name("midi-pages-shutdown-event".into())
        .spawn(move || {
            let rc = unsafe { shutdown::WaitForSingleObject(handle, shutdown::INFINITE) };
            if rc == shutdown::WAIT_OBJECT_0 {
                info!("shutdown event signalled");
                if let Some(threads) = THREADS.get() {
                    trigger_shutdown(threads);
                }
            } else {
                warn!(rc, "shutdown event waiter returned unexpected code");
            }
            // Intentional leak: the OS reclaims the handle on process exit.
            // CloseHandle here would race with another --stop attempt that
            // could open before we exit.
        });
}

#[cfg(target_os = "windows")]
fn find_proxy_pids_except_self() -> Result<Vec<u32>> {
    let self_pid = std::process::id();
    let output = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq midi-pages.exe", "/FO", "CSV", "/NH"])
        .output()
        .context("running tasklist to enumerate midi-pages.exe")?;
    if !output.status.success() {
        return Err(anyhow!("tasklist exited with {}", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // CSV row: "midi-pages.exe","1234","Console","1","18,044 K"
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 2 {
            continue;
        }
        let pid_str = fields[1].trim().trim_matches('"');
        if let Ok(pid) = pid_str.parse::<u32>()
            && pid != self_pid
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(not(target_os = "windows"))]
fn graceful_stop(_target_pid: Option<u32>) -> Result<()> {
    Err(anyhow!("--stop is Windows-only"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_or_virtual_output(client: &str, needle: &str) -> Result<midir::MidiOutputConnection> {
    use midir::os::unix::VirtualOutput;
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
