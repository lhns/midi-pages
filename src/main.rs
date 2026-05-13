use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Cross-device registration gate: a counter shared by all per-device
/// worker threads. Each worker waits until the counter matches its
/// declaration index before registering its host-side endpoints, and
/// bumps the counter when finished. The result: WMS Virtual Device
/// registration runs in strict config-declaration order across devices,
/// midisrv assigns WinMM indices in that order, and DasLight bindings
/// (which persist by index) stay stable across runs. Phase 2 of the
/// startup (wait_for_ports, supervisor, etc.) still runs fully in
/// parallel — the gate only serializes the fast registration step.
type RegisterGate = Arc<(Mutex<usize>, Condvar)>;

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
use tracing::{debug, error, info, warn};

/// Shared per-device connection state. The supervisor thread is the only
/// writer; dispatch paths and host-port callbacks hold `Arc<DeviceLink>`s
/// and use `send_device` to push bytes to the physical device. When the
/// device is disconnected, `out` is `None` and writes are silently dropped
/// (their LED state is still cached in the proxy and will be replayed on
/// reconnect). `error_signal` is set by `send_device` on a failed send so
/// the supervisor can react on the next tick.
struct DeviceLink {
    out: Mutex<Option<midir::MidiOutputConnection>>,
    /// Holds the input connection alive; midir's callback fires from its
    /// own thread. Dropping this stops the callback.
    in_conn: Mutex<Option<midir::MidiInputConnection<()>>>,
    error_signal: AtomicBool,
}

/// Boxed input-event callback shape used everywhere midir hands us bytes.
/// Aliased here mostly so the closure-factory return types stay readable
/// (and so clippy::type_complexity doesn't trip on the four `make_cb`
/// sites that build per-mode dispatch closures).
type InputCallback = Box<dyn Fn(&[u8]) + Send + 'static>;

impl DeviceLink {
    fn new() -> Self {
        Self {
            out: Mutex::new(None),
            in_conn: Mutex::new(None),
            error_signal: AtomicBool::new(false),
        }
    }

    /// Push bytes to the device if connected. Silently drops while
    /// disconnected. Sets the error signal on send failure so the
    /// supervisor sees it on its next tick.
    fn send_device(&self, bytes: &[u8]) {
        let mut guard = self.out.lock().unwrap();
        if let Some(conn) = guard.as_mut()
            && let Err(e) = conn.send(bytes)
        {
            warn!(
                kind = "device-send",
                bytes = bytes.len(),
                "send failed: {e}"
            );
            self.error_signal.store(true, Ordering::Relaxed);
        }
    }
}

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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Set a panic hook that flags shutdown and gives worker threads a moment
    // to run their Drop chains (which release WMS resources). If we panic
    // without this, the process bails before midisrv has processed our
    // ref-drops and the Virtual Device registration leaks.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        SHUTDOWN.store(true, Ordering::SeqCst);
        // Same window the Win32 console handler uses, so a panicking worker
        // thread gets to run host-port Drop chains (DisconnectEndpointConnection
        // + Session::Close) before the process exits — otherwise midisrv keeps
        // the Virtual Device registrations alive for the dead PID and the next
        // run hangs on CreateVirtualDevice.
        std::thread::sleep(Duration::from_millis(2_500));
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

    // Cross-device registration gate: ensures WMS Virtual Device names
    // register in strict config-declaration order across devices, even
    // though everything downstream runs in parallel. See RegisterGate.
    let register_gate: RegisterGate = Arc::new((Mutex::new(0usize), Condvar::new()));

    let mut handles = Vec::new();
    for (idx, d) in cfg.devices.into_iter().enumerate() {
        let gate = Arc::clone(&register_gate);
        let h = thread::Builder::new()
            .name(format!("midi-pages:{}", d.name))
            .spawn(move || {
                if let Err(e) = run_device(&d, idx, gate) {
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

    info!("all device threads spawned; Ctrl-C to exit");

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

/// Spawn a per-device polling thread that logs whenever the physical port
/// transitions from present to absent or back. Edge-triggered (only logs on
/// change) so steady-state runs are silent. Exits when `SHUTDOWN` flips.
///
/// The thread handle is dropped: the watchdog dies on its own via the
/// SHUTDOWN check on the next `park_timeout` wake-up (within ~5s).
///
/// Polls at 5s rather than 1s — the supervisor's own 1s presence check
/// drives reconnect latency; this watchdog only adds standalone
/// diagnostics, so a slower cadence keeps midisrv enumeration traffic low.
fn spawn_presence_watchdog(device_name: String, needle: String) {
    // JoinHandle dropped by design: the watchdog observes SHUTDOWN on its
    // park_timeout wake-up and self-terminates within ~5 s.
    let _ = thread::Builder::new()
        .name(format!("midi-pages:{device_name}:presence"))
        .spawn(move || {
            let client = format!("midi-pages-{device_name}-presence");
            let mut last_present: Option<bool> = None;
            while !SHUTDOWN.load(Ordering::SeqCst) {
                match ports::port_present(&client, &needle) {
                    Ok(present) => {
                        if last_present != Some(present) {
                            if last_present.is_none() {
                                info!(
                                    device = %device_name,
                                    port = %needle,
                                    present,
                                    "device port presence (initial)"
                                );
                            } else if present {
                                info!(
                                    device = %device_name,
                                    port = %needle,
                                    "device port back"
                                );
                            } else {
                                warn!(
                                    device = %device_name,
                                    port = %needle,
                                    "device port gone"
                                );
                            }
                            last_present = Some(present);
                        }
                    }
                    Err(e) => {
                        warn!(
                            device = %device_name,
                            port = %needle,
                            "presence probe failed: {e}"
                        );
                    }
                }
                thread::park_timeout(Duration::from_secs(5));
            }
        });
}

/// Block until WMS Virtual Devices have actually disappeared from WinMM
/// enumeration, then a small extra margin. The Drop chain is synchronous
/// (DisconnectEndpointConnection + MidiSession::Close return before this
/// runs) but midisrv historically lagged on removing the endpoint from
/// external enumeration; an app enumerating in that window (notably
/// DasLight refreshing its MIDI list right after Ctrl-C) used to wedge.
/// Polling for invisibility gives us an evidence-based shutdown signal
/// instead of a fixed-duration sleep — finishes as soon as midisrv is
/// caught up, and surfaces a warn log if it never catches up.
///
/// The trailing 250 ms margin is a deliberate belt-and-braces grace
/// period in case midisrv keeps reconciling internal state after the
/// WinMM-visibility flip.
#[cfg(target_os = "windows")]
fn settle_midisrv(device_name: &str, port_names: &[&str]) {
    if port_names.is_empty() {
        return;
    }
    info!(
        device = %device_name,
        ports = port_names.len(),
        "waiting for midisrv to drop endpoints from WinMM enumeration"
    );
    match ports::wait_for_ports_gone(port_names, Duration::from_secs(10)) {
        Ok(()) => info!(device = %device_name, "endpoints gone from WinMM"),
        Err(e) => warn!(device = %device_name, "wait_for_ports_gone: {e}"),
    }
    thread::sleep(Duration::from_millis(250));
    info!(device = %device_name, "shutdown complete");
}

/// RAII guard for the cross-device registration gate. Acquired via
/// `await_register_turn`; releases the next device's slot when dropped.
struct RegisterTurn<'a> {
    gate: &'a (Mutex<usize>, Condvar),
    device_idx: usize,
}

impl Drop for RegisterTurn<'_> {
    fn drop(&mut self) {
        let (lock, cvar) = self.gate;
        *lock.lock().unwrap() = self.device_idx + 1;
        cvar.notify_all();
    }
}

/// Block until it's `device_idx`'s turn to register. Returns a guard that
/// releases the next device on drop. Honours `SHUTDOWN` so a Ctrl-C during
/// the very first registration cycle still tears down promptly.
fn await_register_turn(gate: &RegisterGate, device_idx: usize) -> Option<RegisterTurn<'_>> {
    let (lock, cvar) = &**gate;
    let mut next = lock.lock().unwrap();
    while *next != device_idx {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return None;
        }
        // Wake periodically as a safety belt for SHUTDOWN.
        let (n, _) = cvar.wait_timeout(next, Duration::from_millis(250)).unwrap();
        next = n;
    }
    Some(RegisterTurn { gate, device_idx })
}

fn make_device(driver: Driver) -> Box<dyn Device> {
    match driver {
        Driver::MiniMk3 => Box::new(MiniMk3),
        Driver::ApcMini => Box::new(ApcMini),
    }
}

fn run_device(cfg: &DeviceConfig, device_idx: usize, register_gate: RegisterGate) -> Result<()> {
    info!(device = %cfg.name, idx = device_idx, "run_device: start");
    let device = make_device(cfg.driver);
    let proxy = Arc::new(Mutex::new(Proxy::new(cfg, device)));

    // Diagnostics: 1-second port-presence poll so we get an info-level log
    // whenever the physical device disappears or reappears.
    spawn_presence_watchdog(cfg.name.clone(), cfg.port_match.output().to_string());

    // Shared device-link. The mode-specific runner does the initial connect
    // and spawns a supervisor that handles unplug + replug.
    let link = Arc::new(DeviceLink::new());

    match cfg.mode {
        Mode::NoteOffset => run_note_offset(cfg, proxy, link, device_idx, register_gate),
        Mode::PerPort => run_per_port(cfg, proxy, link, device_idx, register_gate),
    }
}

/// Open the device input + output, run the boot sequence, and push the
/// proxy's current paint + cached page state. Used both at startup and by
/// the supervisor on reconnect; same exact restore each time.
#[cfg(target_os = "windows")]
fn connect_device_link<F>(
    cfg: &DeviceConfig,
    proxy: &Arc<Mutex<Proxy>>,
    link: &Arc<DeviceLink>,
    on_recv: F,
) -> Result<()>
where
    F: Fn(&[u8]) + Send + 'static,
{
    let mut out = ports::open_output_named(
        &format!("midi-pages-{}-device-out", cfg.name),
        cfg.port_match.output(),
    )?;

    // Boot SysEx and the driver's boot bytes are fire-and-forget: a
    // failed write here would surface as a `device send` warn from the
    // supervisor's first real message anyway, and the user doesn't want
    // a startup error if the device is just slow to accept bytes.
    if let Some(sysex) = &cfg.boot_sysex {
        let _ = out.send(sysex);
    }
    for bytes in make_device(cfg.driver).boot() {
        let _ = out.send(&bytes);
    }

    // Restore the device's visible state: page-button indicators + cached
    // LED state for the persistent (or previewed) page. Same fire-and-
    // forget rationale as the boot bytes above.
    let restore: Vec<Vec<u8>> = {
        let p = proxy.lock().unwrap();
        let mut bytes_list = p.paint_indicator_state();
        for o in p.replay_page_to_device() {
            if let Out::ToDevice(b) = o {
                bytes_list.push(b);
            }
        }
        bytes_list
    };
    for bytes in restore {
        let _ = out.send(&bytes);
    }

    let in_conn = open_device_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        cfg.port_match.input(),
        on_recv,
    )?;
    info!(
        device = %cfg.name,
        port = %cfg.port_match.input(),
        "device input connected"
    );

    *link.out.lock().unwrap() = Some(out);
    *link.in_conn.lock().unwrap() = Some(in_conn);
    link.error_signal.store(false, Ordering::Relaxed);
    Ok(())
}

/// Linux/macOS variant: midir-native opens (no virtual fallback for the
/// device side, since the physical device is supposed to exist).
#[cfg(not(target_os = "windows"))]
fn connect_device_link<F>(
    cfg: &DeviceConfig,
    proxy: &Arc<Mutex<Proxy>>,
    link: &Arc<DeviceLink>,
    on_recv: F,
) -> Result<()>
where
    F: Fn(&[u8]) + Send + 'static,
{
    let mut out = ports::open_output_named(
        &format!("midi-pages-{}-device-out", cfg.name),
        cfg.port_match.output(),
    )?;

    // Boot SysEx + driver boot + restore are fire-and-forget. Same
    // rationale as the Windows variant: a failure here surfaces as a
    // device send warn from the supervisor's first real message.
    if let Some(sysex) = &cfg.boot_sysex {
        let _ = out.send(sysex);
    }
    for bytes in make_device(cfg.driver).boot() {
        let _ = out.send(&bytes);
    }

    let restore: Vec<Vec<u8>> = {
        let p = proxy.lock().unwrap();
        let mut bytes_list = p.paint_indicator_state();
        for o in p.replay_page_to_device() {
            if let Out::ToDevice(b) = o {
                bytes_list.push(b);
            }
        }
        bytes_list
    };
    for bytes in restore {
        let _ = out.send(&bytes);
    }

    let in_conn = open_or_virtual_input(
        &format!("midi-pages-{}-device-in", cfg.name),
        cfg.port_match.input(),
        on_recv,
        false,
    )?;
    info!(
        device = %cfg.name,
        port = %cfg.port_match.input(),
        "device input connected"
    );

    *link.out.lock().unwrap() = Some(out);
    *link.in_conn.lock().unwrap() = Some(in_conn);
    link.error_signal.store(false, Ordering::Relaxed);
    Ok(())
}

/// Generic reconnect loop. The supervisor wakes every second (or sooner if
/// shutdown is requested) and decides whether to tear down + reconnect.
/// `make_callback` returns a fresh input-callback closure on each reconnect
/// (midir consumes the closure when registering it).
fn spawn_supervisor<F>(
    device_name: String,
    needle: String,
    cfg: DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    link: Arc<DeviceLink>,
    make_callback: F,
) where
    F: Fn() -> InputCallback + Send + 'static,
{
    // JoinHandle dropped by design: the supervisor observes SHUTDOWN at
    // each tick (every ~1 s park_timeout) and self-terminates after
    // releasing both link.out and link.in_conn.
    let _ = thread::Builder::new()
        .name(format!("midi-pages:{device_name}:supervisor"))
        .spawn(move || {
            let client = format!("midi-pages-{device_name}-supervisor");
            let mut backoff_ms = 250u64;
            while !SHUTDOWN.load(Ordering::SeqCst) {
                let connected = link.out.lock().unwrap().is_some();
                if connected {
                    let error = link.error_signal.swap(false, Ordering::Relaxed);
                    let port_present = matches!(ports::port_present(&client, &needle), Ok(true));
                    if !port_present {
                        warn!(
                            device = %device_name,
                            "device link down (port absent); will retry"
                        );
                        *link.out.lock().unwrap() = None;
                        *link.in_conn.lock().unwrap() = None;
                        backoff_ms = 250;
                    } else if error {
                        // Send failed but the port still shows. Probably a
                        // transient hiccup; clear the flag and keep going.
                        debug!(
                            device = %device_name,
                            "send error while port present; not reconnecting"
                        );
                    }
                    thread::park_timeout(Duration::from_secs(1));
                } else {
                    let cb = make_callback();
                    match connect_device_link(&cfg, &proxy, &link, move |msg| cb(msg)) {
                        Ok(()) => {
                            info!(device = %device_name, "device reconnected");
                            backoff_ms = 250;
                        }
                        Err(e) => {
                            debug!(
                                device = %device_name,
                                "reconnect attempt failed: {e}"
                            );
                            thread::sleep(Duration::from_millis(backoff_ms));
                            backoff_ms = (backoff_ms * 2).min(2000);
                        }
                    }
                }
            }
            // On shutdown, drop both. Their Drop releases midir resources.
            *link.out.lock().unwrap() = None;
            *link.in_conn.lock().unwrap() = None;
        });
}

// =========================================================================
// Windows: WMS Virtual Device (one endpoint per page, plugin callback).
// =========================================================================

#[cfg(target_os = "windows")]
fn run_note_offset(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    link: Arc<DeviceLink>,
    device_idx: usize,
    register_gate: RegisterGate,
) -> Result<()> {
    let host_name = cfg
        .host_port_in
        .as_deref()
        .ok_or_else(|| anyhow!("note_offset mode requires host_port_in"))?;

    // Host side: ONE Virtual Device endpoint, plugin callback feeds the proxy.
    // Two-phase: register (synchronous WinRT), wait for WinMM visibility,
    // then install. With a single port there's no order question, but the
    // cross-device register gate still enforces declaration order across
    // multiple devices.
    let host_port: Arc<Mutex<Option<ports::WindowsHostPort>>> = Arc::new(Mutex::new(None));

    // Wrap the per-device startup + serve sequence in a closure so we can
    // run host-port teardown unconditionally on any exit path. Without
    // this, an early Err (e.g. wait_for_ports timeout) would skip the
    // Drop chain and leak the partially-registered WMS endpoints into
    // midisrv for this PID, poisoning subsequent runs.
    let result: Result<()> = (|| {
        let pending = {
            let _turn = match await_register_turn(&register_gate, device_idx) {
                Some(t) => t,
                None => return Ok(()),
            };
            let p_host = Arc::clone(&proxy);
            let host_port_for_cb = Arc::clone(&host_port);
            let link_for_cb = Arc::clone(&link);
            ports::PendingHostPort::register(host_name, move |msg| {
                let outs = {
                    let mut p = p_host.lock().unwrap();
                    p.handle_host_in(msg)
                };
                dispatch_offset_windows(&outs, &host_port_for_cb, &link_for_cb);
            })?
            // _turn drops here, releasing the next device.
        };
        ports::wait_for_ports(&[host_name], Duration::from_secs(20))?;
        *host_port.lock().unwrap() = Some(pending.into_ready());

        // Device-side callback factory. Used both for the initial connect and
        // by the supervisor on every reconnect. `first_seen` is shared across
        // all callbacks the factory ever produces, so we log exactly once per
        // device for the very first input event seen — a quick check that
        // midir's input subscription actually fires.
        let make_cb = {
            let proxy = Arc::clone(&proxy);
            let host_port = Arc::clone(&host_port);
            let link = Arc::clone(&link);
            let first_seen = Arc::new(AtomicBool::new(false));
            let device_name = cfg.name.clone();
            move || -> InputCallback {
                let p = Arc::clone(&proxy);
                let h = Arc::clone(&host_port);
                let l = Arc::clone(&link);
                let fs = Arc::clone(&first_seen);
                let dn = device_name.clone();
                Box::new(move |msg: &[u8]| {
                    if !fs.swap(true, Ordering::Relaxed) {
                        info!(device = %dn, bytes = msg.len(), "first device input event");
                    }
                    let outs = {
                        let mut p = p.lock().unwrap();
                        p.handle_device_in(msg)
                    };
                    dispatch_offset_windows(&outs, &h, &l);
                })
            }
        };

        // Initial connect. Soft-fail: if the physical device isn't
        // reachable yet, let the supervisor pick up with its retry loop.
        let cb = make_cb();
        if let Err(e) = connect_device_link(cfg, &proxy, &link, move |msg| cb(msg)) {
            warn!(
                device = %cfg.name,
                error = %e,
                "physical device not reachable at startup; supervisor will keep retrying"
            );
        }

        spawn_supervisor(
            cfg.name.clone(),
            cfg.port_match.output().to_string(),
            cfg.clone(),
            Arc::clone(&proxy),
            Arc::clone(&link),
            make_cb,
        );

        info!(device = %cfg.name, mode = "note_offset", "device ready");
        wait_for_shutdown();
        Ok(())
    })();

    // Force WindowsHostPort::Drop unconditionally — same teardown the
    // success path needs (Arc-cycle through the plugin shim's input
    // callback keeps the value alive otherwise) and the error path
    // needs (otherwise partially-registered endpoints leak into midisrv
    // and break subsequent runs).
    let _dropped = host_port.lock().unwrap().take();
    settle_midisrv(&cfg.name, &[host_name]);
    result
}

#[cfg(target_os = "windows")]
fn run_per_port(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    link: Arc<DeviceLink>,
    device_idx: usize,
    register_gate: RegisterGate,
) -> Result<()> {
    let pages = cfg.pages;
    let port_names = cfg.page_port_names();

    // ONE Virtual Device endpoint per page; the proxy reads via its plugin
    // callback and writes via the same handle's `.send()`.
    let host_ports: Arc<Mutex<Vec<Option<ports::WindowsHostPort>>>> =
        Arc::new(Mutex::new((0..pages as usize).map(|_| None).collect()));
    let name_refs: Vec<&str> = port_names.iter().map(String::as_str).collect();

    // Wrap the startup + serve sequence in a closure so the host-port
    // teardown runs unconditionally on any exit path. Without this, an
    // early Err (e.g. wait_for_ports timing out under midisrv load)
    // would leak the partially-registered WMS endpoints into midisrv
    // for this PID and poison subsequent runs.
    let result: Result<()> = (|| {
        // Two-phase creation. Phase 1 registers all per-page endpoints
        // serially in page order on this thread — fast (~tens of ms each)
        // and gives midisrv the CreateVirtualDevice calls back-to-back
        // so the WinMM device-index assignments come out in page order
        // (DasLight persists its MIDI bindings by index, so this
        // stability matters across runs). The cross-device register gate
        // ensures multiple [[device]] entries also register in
        // declaration order. Phase 2 (the slow visibility wait and
        // everything downstream) runs in parallel across devices.
        let pending: Vec<(usize, ports::PendingHostPort)> = {
            let _turn = match await_register_turn(&register_gate, device_idx) {
                Some(t) => t,
                None => return Ok(()),
            };
            let mut pending = Vec::with_capacity(port_names.len());
            for (page_idx, host_name) in port_names.iter().enumerate() {
                let p = Arc::clone(&proxy);
                let host_ports_for_cb = Arc::clone(&host_ports);
                let link_for_cb = Arc::clone(&link);
                let page = page_idx as u8;
                let pending_port = ports::PendingHostPort::register(host_name, move |msg| {
                    let outs = {
                        let mut p = p.lock().unwrap();
                        p.handle_host_in_per_port(page, msg)
                    };
                    dispatch_per_port_windows(&outs, &host_ports_for_cb, &link_for_cb);
                })?;
                pending.push((page_idx, pending_port));
            }
            pending
        };

        ports::wait_for_ports(&name_refs, Duration::from_secs(20))?;

        {
            let mut guard = host_ports.lock().unwrap();
            for (page_idx, pending_port) in pending {
                guard[page_idx] = Some(pending_port.into_ready());
            }
        }

        // Device-side callback factory; rebuilt each reconnect by the
        // supervisor. `first_seen` is shared across all callbacks the
        // factory produces so the first input event per device logs once.
        let make_cb = {
            let proxy = Arc::clone(&proxy);
            let host_ports = Arc::clone(&host_ports);
            let link = Arc::clone(&link);
            let first_seen = Arc::new(AtomicBool::new(false));
            let device_name = cfg.name.clone();
            move || -> InputCallback {
                let p = Arc::clone(&proxy);
                let h = Arc::clone(&host_ports);
                let l = Arc::clone(&link);
                let fs = Arc::clone(&first_seen);
                let dn = device_name.clone();
                Box::new(move |msg: &[u8]| {
                    if !fs.swap(true, Ordering::Relaxed) {
                        info!(device = %dn, bytes = msg.len(), "first device input event");
                    }
                    let outs = {
                        let mut p = p.lock().unwrap();
                        p.handle_device_in(msg)
                    };
                    dispatch_per_port_windows(&outs, &h, &l);
                })
            }
        };

        // Initial connect. Soft-fail: supervisor will keep retrying if
        // the physical device isn't reachable yet.
        let cb = make_cb();
        if let Err(e) = connect_device_link(cfg, &proxy, &link, move |msg| cb(msg)) {
            warn!(
                device = %cfg.name,
                error = %e,
                "physical device not reachable at startup; supervisor will keep retrying"
            );
        }

        spawn_supervisor(
            cfg.name.clone(),
            cfg.port_match.output().to_string(),
            cfg.clone(),
            Arc::clone(&proxy),
            Arc::clone(&link),
            make_cb,
        );

        info!(
            device = %cfg.name,
            mode = "per_port",
            pages,
            "device ready"
        );
        wait_for_shutdown();
        Ok(())
    })();

    // Force WindowsHostPort::Drop on each per-page endpoint, unconditionally.
    // Two reasons: (1) the plugin shim's input callback captures
    // Arc<host_ports>, forming a reference cycle that would otherwise keep
    // every WindowsHostPort alive past process exit; (2) on an error path
    // (e.g. wait_for_ports timeout) we still need to release whatever was
    // registered so midisrv doesn't accumulate leaked endpoints across
    // runs. Drop in parallel via thread::scope; each WindowsHostPort::Drop
    // takes ~500 ms in midisrv RPCs.
    let ports_to_drop: Vec<ports::WindowsHostPort> = {
        let mut guard = host_ports.lock().unwrap();
        guard.iter_mut().filter_map(|slot| slot.take()).collect()
    };
    thread::scope(|s| {
        for port in ports_to_drop {
            s.spawn(move || drop(port));
        }
    });
    settle_midisrv(&cfg.name, &name_refs);
    result
}

#[cfg(target_os = "windows")]
fn dispatch_offset_windows(
    outs: &[Out],
    host_port: &Arc<Mutex<Option<ports::WindowsHostPort>>>,
    link: &Arc<DeviceLink>,
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
            Out::ToDevice(b) => link.send_device(b),
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
    link: &Arc<DeviceLink>,
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
            Out::ToDevice(b) => link.send_device(b),
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
    link: Arc<DeviceLink>,
    device_idx: usize,
    register_gate: RegisterGate,
) -> Result<()> {
    let host_in_name = cfg
        .host_port_in
        .as_deref()
        .ok_or_else(|| anyhow!("note_offset mode requires host_port_in"))?;
    let host_out_name = cfg
        .host_port_out
        .as_deref()
        .ok_or_else(|| anyhow!("note_offset mode requires host_port_out"))?;

    // Note on cleanup-on-error: unlike the Windows variant (which has the
    // WMS plugin shim's Arc-cycle issue requiring an explicit take()
    // teardown wrapped in a closure — see commit 5151b94), midir on
    // ALSA/CoreMIDI doesn't have an equivalent hidden-ref hazard. Every
    // resource here is owned via plain stack locals + Arc, so an early
    // `?` propagation drops them in reverse order naturally and midir
    // releases the OS-side ports. No closure-wrap needed on this path.

    // Cross-device register gate: midir's virtual-port creation assigns
    // IDs in call order; serializing across devices keeps that ordering
    // deterministic the same way it does on Windows/midisrv. The gate
    // covers the output side only; the input side registers right after,
    // outside the gate (input ordering is rarely what host apps persist).
    let host_out: Arc<Mutex<midir::MidiOutputConnection>> = {
        let _turn = match await_register_turn(&register_gate, device_idx) {
            Some(t) => t,
            None => return Ok(()),
        };
        let conn =
            open_or_virtual_output(&format!("midi-pages-{}-host-out", cfg.name), host_out_name)?;
        Arc::new(Mutex::new(conn))
    };

    let p_host = Arc::clone(&proxy);
    let host_out_host = Arc::clone(&host_out);
    let link_host = Arc::clone(&link);
    let _host_in = open_or_virtual_input(
        &format!("midi-pages-{}-host-in", cfg.name),
        host_in_name,
        move |msg| {
            let outs = {
                let mut p = p_host.lock().unwrap();
                p.handle_host_in(msg)
            };
            dispatch_offset(&outs, &host_out_host, &link_host);
        },
        true,
    )?;

    let make_cb = {
        let proxy = Arc::clone(&proxy);
        let host_out = Arc::clone(&host_out);
        let link = Arc::clone(&link);
        let first_seen = Arc::new(AtomicBool::new(false));
        let device_name = cfg.name.clone();
        move || -> InputCallback {
            let p = Arc::clone(&proxy);
            let h = Arc::clone(&host_out);
            let l = Arc::clone(&link);
            let fs = Arc::clone(&first_seen);
            let dn = device_name.clone();
            Box::new(move |msg: &[u8]| {
                if !fs.swap(true, Ordering::Relaxed) {
                    info!(device = %dn, bytes = msg.len(), "first device input event");
                }
                let outs = {
                    let mut p = p.lock().unwrap();
                    p.handle_device_in(msg)
                };
                dispatch_offset(&outs, &h, &l);
            })
        }
    };

    let cb = make_cb();
    if let Err(e) = connect_device_link(cfg, &proxy, &link, move |msg| cb(msg)) {
        warn!(
            device = %cfg.name,
            error = %e,
            "physical device not reachable at startup; supervisor will keep retrying"
        );
    }

    spawn_supervisor(
        cfg.name.clone(),
        cfg.port_match.output().to_string(),
        cfg.clone(),
        Arc::clone(&proxy),
        Arc::clone(&link),
        make_cb,
    );

    info!(device = %cfg.name, mode = "note_offset", "device ready");
    wait_for_shutdown();
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_per_port(
    cfg: &DeviceConfig,
    proxy: Arc<Mutex<Proxy>>,
    link: Arc<DeviceLink>,
    device_idx: usize,
    register_gate: RegisterGate,
) -> Result<()> {
    let pages = cfg.pages;
    let port_names = cfg.page_port_names();

    // Note: unlike the Windows run_per_port (commit 5151b94 wraps its body
    // in a closure to guarantee teardown on any exit path), this midir-side
    // path doesn't have the WMS plugin-shim Arc-cycle. midir releases
    // OS-side ports cleanly when the Arcs drop, and an early `?`
    // propagation here drops them in stack order naturally.

    // Each page exposes ONE virtual port name. midir on Unix creates
    // separate input + output sub-ports under the same name. The
    // cross-device gate serialises virtual-port registration across
    // devices so IDs come out in declaration order.
    let host_outs: Arc<Vec<Arc<Mutex<midir::MidiOutputConnection>>>> = {
        let _turn = match await_register_turn(&register_gate, device_idx) {
            Some(t) => t,
            None => return Ok(()),
        };
        let mut outs: Vec<Arc<Mutex<midir::MidiOutputConnection>>> = Vec::new();
        for (page_idx, name) in port_names.iter().enumerate() {
            let conn = open_or_virtual_output(
                &format!("midi-pages-{}-page{}-out", cfg.name, page_idx + 1),
                name,
            )?;
            outs.push(Arc::new(Mutex::new(conn)));
        }
        Arc::new(outs)
    };

    let mut _host_inputs = Vec::new();
    for (page_idx, name) in port_names.iter().enumerate() {
        let p = Arc::clone(&proxy);
        let host_outs = Arc::clone(&host_outs);
        let link = Arc::clone(&link);
        let page = page_idx as u8;
        let conn = open_or_virtual_input(
            &format!("midi-pages-{}-page{}-in", cfg.name, page_idx + 1),
            name,
            move |msg| {
                let outs = {
                    let mut p = p.lock().unwrap();
                    p.handle_host_in_per_port(page, msg)
                };
                dispatch_per_port(&outs, &host_outs, &link);
            },
            true,
        )?;
        _host_inputs.push(conn);
    }

    let make_cb = {
        let proxy = Arc::clone(&proxy);
        let host_outs = Arc::clone(&host_outs);
        let link = Arc::clone(&link);
        let first_seen = Arc::new(AtomicBool::new(false));
        let device_name = cfg.name.clone();
        move || -> InputCallback {
            let p = Arc::clone(&proxy);
            let h = Arc::clone(&host_outs);
            let l = Arc::clone(&link);
            let fs = Arc::clone(&first_seen);
            let dn = device_name.clone();
            Box::new(move |msg: &[u8]| {
                if !fs.swap(true, Ordering::Relaxed) {
                    info!(device = %dn, bytes = msg.len(), "first device input event");
                }
                let outs = {
                    let mut p = p.lock().unwrap();
                    p.handle_device_in(msg)
                };
                dispatch_per_port(&outs, &h, &l);
            })
        }
    };

    let cb = make_cb();
    if let Err(e) = connect_device_link(cfg, &proxy, &link, move |msg| cb(msg)) {
        warn!(
            device = %cfg.name,
            error = %e,
            "physical device not reachable at startup; supervisor will keep retrying"
        );
    }

    spawn_supervisor(
        cfg.name.clone(),
        cfg.port_match.output().to_string(),
        cfg.clone(),
        Arc::clone(&proxy),
        Arc::clone(&link),
        make_cb,
    );

    info!(device = %cfg.name, mode = "per_port", pages, "device ready");
    wait_for_shutdown();
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn dispatch_offset(
    outs: &[Out],
    host_out: &Arc<Mutex<midir::MidiOutputConnection>>,
    link: &Arc<DeviceLink>,
) {
    for o in outs {
        match o {
            Out::ToHost(b) => {
                if let Err(e) = host_out.lock().unwrap().send(b) {
                    warn!("host send: {e}");
                }
            }
            Out::ToDevice(b) => link.send_device(b),
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
    link: &Arc<DeviceLink>,
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
            Out::ToDevice(b) => link.send_device(b),
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
    let mut all_names: Vec<String> = Vec::new();
    let mut pending: Vec<ports::PendingHostPort> = Vec::new();
    for d in &cfg.devices {
        match d.mode {
            Mode::PerPort => {
                for name in d.page_port_names() {
                    info!(port = %name, "open");
                    pending.push(ports::PendingHostPort::register(&name, |_| {})?);
                    all_names.push(name);
                }
            }
            Mode::NoteOffset => {
                if let Some(name) = d.host_port_in.as_deref() {
                    info!(port = %name, "open");
                    pending.push(ports::PendingHostPort::register(name, |_| {})?);
                    all_names.push(name.to_string());
                }
            }
        }
    }
    let name_refs: Vec<&str> = all_names.iter().map(String::as_str).collect();
    ports::wait_for_ports(&name_refs, Duration::from_secs(20))?;
    let ports_ready: Vec<ports::WindowsHostPort> =
        pending.into_iter().map(|p| p.into_ready()).collect();
    info!(count = ports_ready.len(), "opened; settling 500 ms");
    thread::sleep(Duration::from_millis(500));
    info!("dropping all ports (exercises Drop chain)");
    drop(ports_ready);
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
