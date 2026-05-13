//! Thin wrappers over `midir` for port discovery and matching, plus
//! cross-platform helpers to **create** virtual host-side ports:
//!
//! - On Linux & macOS: `midir`'s built-in `create_virtual_*` API (called from `main.rs`).
//! - On Windows: call the Windows MIDI Services WinRT API directly via
//!   bindings generated at build time from the vendored
//!   `Microsoft.Windows.Devices.Midi2.winmd`. End users only need the WMS
//!   runtime DLLs installed (small — ~3 MB; bundled with current Win11 MIDI
//!   rollout, otherwise via the WMS SDK installer).

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

/// Cheap presence check: open a transient `MidiOutput`, list ports, return
/// true iff at least one name contains `needle`. No `connect` happens here,
/// so this is safe to call from a poll thread once per second. Errors when
/// the OS-side `MidiOutput::new` itself fails (mid-disconnect this can
/// transiently fail; callers should treat that as "unknown" rather than
/// definitive absence).
pub fn port_present(client: &str, needle: &str) -> Result<bool> {
    let mo = MidiOutput::new(client).context("MidiOutput::new (port_present)")?;
    for port in mo.ports() {
        let name = mo.port_name(&port).unwrap_or_default();
        if name.contains(needle) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn open_output_named(client: &str, needle: &str) -> Result<midir::MidiOutputConnection> {
    let mo = MidiOutput::new(client).context("create MidiOutput")?;
    let port = find_output(&mo, needle)?;
    mo.connect(&port, client)
        .map_err(|e| anyhow!("connect output `{needle}`: {e}"))
}

#[cfg(target_os = "windows")]
pub use windows_host::{PendingHostPort, WindowsHostPort, wait_for_ports, wait_for_ports_gone};

#[cfg(target_os = "windows")]
mod windows_host {
    //! WMS Virtual Device-based host port: exposes ONE MIDI endpoint to the
    //! OS and uses an in-process plugin callback for the proxy side.

    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tracing::info;
    use windows_core::{GUID, HSTRING, IInspectable, Ref, implement};

    use crate::midi::ump;
    use crate::wms_bindings::Microsoft::Windows::Devices::Midi2::Endpoints::Virtual::{
        MidiVirtualDevice, MidiVirtualDeviceCreationConfig, MidiVirtualDeviceManager,
    };
    use crate::wms_bindings::Microsoft::Windows::Devices::Midi2::{
        IMidiEndpointConnectionSource, IMidiEndpointMessageProcessingPlugin,
        IMidiEndpointMessageProcessingPlugin_Impl, MidiDeclaredEndpointInfo,
        MidiEndpointConnection, MidiMessageReceivedEventArgs, MidiSession,
    };

    /// Owned WMS Virtual Device endpoint + session + open connection + plugin.
    /// Dropping releases all of them and the endpoint disappears from system
    /// MIDI enumeration.
    ///
    /// SAFETY: the wrapped WinRT class types are marked `Send + Sync` in the
    /// generated bindings; our wrapping struct doesn't auto-inherit those
    /// marks because of the `Box<dyn Fn>` field inside the plugin shim.
    /// WinRT objects activated by `RoActivateInstance` live in the MTA and
    /// are safe to call from any thread; the COM runtime serializes
    /// per-object as needed.
    pub struct WindowsHostPort {
        _connection: MidiEndpointConnection,
        _session: MidiSession,
        _device: MidiVirtualDevice,
        _plugin: IMidiEndpointMessageProcessingPlugin,
        sender: MidiEndpointConnection,
        connection_id: GUID,
        endpoint_name: String,
    }

    unsafe impl Send for WindowsHostPort {}
    unsafe impl Sync for WindowsHostPort {}

    impl Drop for WindowsHostPort {
        fn drop(&mut self) {
            // Matches Microsoft's documented Virtual Device teardown
            // (samples/csharp-net/virtual-device-app-winui/MainWindow.xaml.cs):
            // disconnect the connection from the session, then close the
            // session. `MidiSession::Close` cascades to plugins and other
            // owned connections; the per-PID ProductInstanceId in `create`
            // ensures recreations after this don't get blocked by midisrv's
            // WinMM-bridge cache. Errors are ignored — best-effort in Drop.
            tracing::info!(name = %self.endpoint_name, "WMS Virtual Device dropped");
            let _ = self
                ._session
                .DisconnectEndpointConnection(self.connection_id);
            let _ = self._session.Close();
        }
    }

    /// A WMS Virtual Device whose registration has completed but whose
    /// external WinMM visibility has not yet been confirmed. Produced by
    /// `PendingHostPort::register` (which runs only the fast synchronous
    /// WinRT phase); call `into_ready` after `wait_for_ports` has confirmed
    /// the endpoint name is enumerable to get the final `WindowsHostPort`.
    ///
    /// Splitting registration from visibility-polling lets callers register
    /// many endpoints in deterministic order on a single thread (so midisrv
    /// assigns WinMM indices in that same order) and then wait for all of
    /// them to become visible in one combined poll.
    pub struct PendingHostPort {
        connection: MidiEndpointConnection,
        session: MidiSession,
        device: MidiVirtualDevice,
        plugin: IMidiEndpointMessageProcessingPlugin,
        connection_id: GUID,
        endpoint_name: String,
    }

    unsafe impl Send for PendingHostPort {}
    unsafe impl Sync for PendingHostPort {}

    impl PendingHostPort {
        /// Run the synchronous WinRT registration phase only:
        /// CreateVirtualDevice + MidiSession + connection.Open + plugin
        /// wiring. Does NOT wait for the endpoint to appear in WinMM
        /// enumeration — that's `wait_for_ports`'s job, which can be batched
        /// across many pending ports.
        pub fn register<F>(name: &str, on_recv: F) -> Result<Self>
        where
            F: Fn(&[u8]) + Send + Sync + 'static,
        {
            ensure_wms_dll_loaded()?;

            // Per-process ProductInstanceId. Stable IDs caused midisrv's WinMM
            // bridge to stop propagating recreations after the first cycle:
            // the WMS device was created server-side OK, but the WinMM
            // enumeration never picked it up, so DasLight (which goes through
            // the WinMM API) saw no device. Including the PID makes each run
            // a brand-new device from midisrv's point of view; the friendly
            // Name stays stable so DasLight bindings still match.
            let product_id = format!("midi-pages.{}.{}", unique_id_for(name), std::process::id());
            let info = MidiDeclaredEndpointInfo {
                Name: HSTRING::from(name),
                ProductInstanceId: HSTRING::from(product_id.as_str()),
                SupportsMidi10Protocol: true,
                SupportsMidi20Protocol: false,
                SupportsReceivingJitterReductionTimestamps: false,
                SupportsSendingJitterReductionTimestamps: false,
                HasStaticFunctionBlocks: false,
                DeclaredFunctionBlockCount: 0,
                SpecificationVersionMajor: 1,
                SpecificationVersionMinor: 0,
            };

            info!(name = %name, "creating WMS Virtual Device");
            let cfg = MidiVirtualDeviceCreationConfig::CreateInstance(
                &HSTRING::from(name),
                &HSTRING::from(""),
                &HSTRING::from("midi-pages"),
                &info,
            )
            .map_err(|e| anyhow!("CreateInstance for `{name}`: {e}"))?;

            let device = MidiVirtualDeviceManager::CreateVirtualDevice(&cfg)
                .map_err(|e| anyhow!("CreateVirtualDevice for `{name}`: {e}"))?;
            // Tell WMS to swallow MIDI 2.0 stream-config negotiation traffic;
            // we declare MIDI 1.0 only and don't want it in our callback.
            let _ = device.SetSuppressHandledMessages(true);

            let dev_id = device
                .DeviceEndpointDeviceId()
                .map_err(|e| anyhow!("DeviceEndpointDeviceId: {e}"))?;
            let session = MidiSession::Create(&HSTRING::from(format!("midi-pages-{name}")))
                .map_err(|e| anyhow!("MidiSession::Create: {e}"))?;
            let connection = session
                .CreateEndpointConnection(&dev_id)
                .map_err(|e| anyhow!("CreateEndpointConnection: {e}"))?;

            let plugin_id = GUID::from_u128(
                0xb1d10001_0000_4000_8000_000000000000 ^ u128::from(fnv1a64(name.as_bytes())),
            );
            let plugin_obj = PluginShim {
                plugin_id,
                plugin_name: Mutex::new(HSTRING::from(format!("midi-pages plugin for {name}"))),
                plugin_tag: Mutex::new(None),
                is_enabled: Mutex::new(true),
                decoder: Mutex::new(ump::Decoder::new()),
                callback: Box::new(on_recv),
            };
            let plugin: IMidiEndpointMessageProcessingPlugin = plugin_obj.into();
            connection
                .AddMessageProcessingPlugin(&plugin)
                .map_err(|e| anyhow!("AddMessageProcessingPlugin: {e}"))?;

            let opened = connection
                .Open()
                .map_err(|e| anyhow!("MidiEndpointConnection::Open: {e}"))?;
            if !opened {
                return Err(anyhow!(
                    "MidiEndpointConnection::Open returned false for `{name}`"
                ));
            }

            let connection_id = connection
                .ConnectionId()
                .map_err(|e| anyhow!("ConnectionId: {e}"))?;

            Ok(Self {
                connection,
                session,
                device,
                plugin,
                connection_id,
                endpoint_name: name.to_string(),
            })
        }

        /// Convert a `PendingHostPort` whose WinMM visibility has already
        /// been confirmed (via `wait_for_ports`) into a fully-ready
        /// `WindowsHostPort`. Pure type-shuffle, no work.
        pub fn into_ready(self) -> WindowsHostPort {
            WindowsHostPort {
                sender: self.connection.clone(),
                _connection: self.connection,
                _session: self.session,
                _device: self.device,
                _plugin: self.plugin,
                connection_id: self.connection_id,
                endpoint_name: self.endpoint_name,
            }
        }
    }

    /// Poll WinMM enumeration until none of the listed `names` appear, or
    /// `timeout` elapses. Inverse of `wait_for_ports`; used at shutdown to
    /// confirm that midisrv has actually removed each WMS Virtual Device
    /// from external enumeration before the proxy considers cleanup
    /// complete. Same single-snapshot-per-tick shape; returns the names
    /// that didn't disappear in time.
    pub fn wait_for_ports_gone(names: &[&str], timeout: Duration) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        let target: std::collections::HashSet<String> =
            names.iter().map(|s| s.to_string()).collect();
        loop {
            let mi = MidiInput::new("midi-pages-probe-in")?;
            let visible: std::collections::HashSet<String> = mi
                .ports()
                .iter()
                .map(|p| mi.port_name(p).unwrap_or_default())
                .collect();
            let still_there: Vec<&String> = target.intersection(&visible).collect();
            if still_there.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let mut names: Vec<String> = still_there.into_iter().cloned().collect();
                names.sort();
                return Err(anyhow!(
                    "WMS Virtual endpoints still visible in WinMM after {:?}: {:?}",
                    timeout,
                    names
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Poll WinMM enumeration until every name in `names` is visible, or
    /// `timeout` elapses. Uses one `MidiInput::new` per tick (cheap; just
    /// enumerates, doesn't connect) and checks all names against that
    /// snapshot. Returns an error listing the names that never appeared.
    pub fn wait_for_ports(names: &[&str], timeout: Duration) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        let mut remaining: std::collections::HashSet<String> =
            names.iter().map(|s| s.to_string()).collect();
        loop {
            let mi = MidiInput::new("midi-pages-probe-in")?;
            let visible: std::collections::HashSet<String> = mi
                .ports()
                .iter()
                .map(|p| mi.port_name(p).unwrap_or_default())
                .collect();
            remaining.retain(|n| !visible.contains(n));
            if remaining.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let mut still_missing: Vec<String> = remaining.into_iter().collect();
                still_missing.sort();
                return Err(anyhow!(
                    "WMS Virtual endpoints didn't appear in WinMM within {:?}: {:?}",
                    timeout,
                    still_missing
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    impl WindowsHostPort {
        /// Send byte-format MIDI 1.0 message(s) to peers. The input must be
        /// one complete message (NoteOn/Off/CC/SysEx/...); it's encoded as
        /// UMP and pushed to the connection word-by-word (one UMP packet
        /// per `SendSingleMessageWords*` call — for SysEx that's one MT3
        /// packet per call).
        pub fn send(&self, bytes: &[u8]) -> Result<()> {
            let words = ump::encode(bytes, 0);
            if words.is_empty() {
                return Ok(());
            }
            let mut i = 0;
            while i < words.len() {
                let mt = (words[i] >> 28) & 0xF;
                let packet_words: usize = match mt {
                    0x0..=0x2 => 1,
                    0x3 | 0x4 => 2,
                    0x5 => 4,
                    _ => 1,
                };
                let take = packet_words.min(words.len() - i);
                match take {
                    1 => self
                        .sender
                        .SendSingleMessageWords(0, words[i])
                        .map_err(|e| anyhow!("SendSingleMessageWords: {e}"))?,
                    2 => self
                        .sender
                        .SendSingleMessageWords2(0, words[i], words[i + 1])
                        .map_err(|e| anyhow!("SendSingleMessageWords2: {e}"))?,
                    3 => self
                        .sender
                        .SendSingleMessageWords3(0, words[i], words[i + 1], words[i + 2])
                        .map_err(|e| anyhow!("SendSingleMessageWords3: {e}"))?,
                    4 => self
                        .sender
                        .SendSingleMessageWords4(
                            0,
                            words[i],
                            words[i + 1],
                            words[i + 2],
                            words[i + 3],
                        )
                        .map_err(|e| anyhow!("SendSingleMessageWords4: {e}"))?,
                    _ => break,
                };
                i += take;
            }
            Ok(())
        }
    }

    /// `LoadLibraryW` `Microsoft.Windows.Devices.Midi2.dll` so that WinRT
    /// activation can locate the Virtual Device factory classes. Unpackaged
    /// Win32/Rust apps don't get the WinRT-registration side-by-side manifest
    /// for free; pre-loading the DLL makes `RoActivateInstance` find the
    /// embedded factory metadata. Cached via `OnceLock`.
    fn ensure_wms_dll_loaded() -> Result<()> {
        use std::sync::OnceLock;
        static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
        let result = LOADED.get_or_init(|| {
            let candidates = [
                r"C:\Program Files\Windows MIDI Services\Desktop App SDK Runtime\Microsoft.Windows.Devices.Midi2.dll",
                r"C:\Program Files (x86)\Windows MIDI Services\Desktop App SDK Runtime\Microsoft.Windows.Devices.Midi2.dll",
            ];
            for c in candidates {
                if std::path::Path::new(c).exists() {
                    let wide: Vec<u16> = c.encode_utf16().chain(std::iter::once(0)).collect();
                    // SAFETY: LoadLibraryW with a valid wide-null-terminated string.
                    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
                    if h.is_null() {
                        return Err(format!(
                            "LoadLibraryW({c}) failed: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                    return Ok(());
                }
            }
            Err(
                "Microsoft.Windows.Devices.Midi2.dll not found. Install the Windows MIDI \
                 Services runtime from \
                 https://aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64."
                    .to_string(),
            )
        });
        result.clone().map_err(|e| anyhow!("{e}"))
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut core::ffi::c_void;
    }

    fn unique_id_for(name: &str) -> String {
        format!("{:016x}", fnv1a64(name.as_bytes()))
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Callback invoked from the plugin's `ProcessIncomingMessage` after we
    /// decode the incoming UMP words to a MIDI 1.0 byte stream.
    type RecvCallback = Box<dyn Fn(&[u8]) + Send + Sync>;

    #[implement(IMidiEndpointMessageProcessingPlugin)]
    struct PluginShim {
        plugin_id: GUID,
        plugin_name: Mutex<HSTRING>,
        plugin_tag: Mutex<Option<IInspectable>>,
        is_enabled: Mutex<bool>,
        decoder: Mutex<ump::Decoder>,
        callback: RecvCallback,
    }

    impl IMidiEndpointMessageProcessingPlugin_Impl for PluginShim_Impl {
        fn PluginId(&self) -> windows_core::Result<GUID> {
            Ok(self.plugin_id)
        }
        fn PluginName(&self) -> windows_core::Result<HSTRING> {
            Ok(self.plugin_name.lock().unwrap().clone())
        }
        fn SetPluginName(&self, value: &HSTRING) -> windows_core::Result<()> {
            *self.plugin_name.lock().unwrap() = value.clone();
            Ok(())
        }
        fn PluginTag(&self) -> windows_core::Result<IInspectable> {
            self.plugin_tag
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(windows_core::Error::empty)
        }
        fn SetPluginTag(&self, value: Ref<'_, IInspectable>) -> windows_core::Result<()> {
            *self.plugin_tag.lock().unwrap() = value.as_ref().map(|v| v.clone());
            Ok(())
        }
        fn IsEnabled(&self) -> windows_core::Result<bool> {
            Ok(*self.is_enabled.lock().unwrap())
        }
        fn SetIsEnabled(&self, value: bool) -> windows_core::Result<()> {
            *self.is_enabled.lock().unwrap() = value;
            Ok(())
        }
        fn Initialize(
            &self,
            _endpoint: Ref<'_, IMidiEndpointConnectionSource>,
        ) -> windows_core::Result<()> {
            Ok(())
        }
        fn OnEndpointConnectionOpened(&self) -> windows_core::Result<()> {
            Ok(())
        }
        fn ProcessIncomingMessage(
            &self,
            args: Ref<'_, MidiMessageReceivedEventArgs>,
            _skip_further: &mut bool,
            _skip_main: &mut bool,
        ) -> windows_core::Result<()> {
            if let Some(a) = args.as_ref() {
                let (mut w0, mut w1, mut w2, mut w3) = (0u32, 0u32, 0u32, 0u32);
                let count = a.FillWords(&mut w0, &mut w1, &mut w2, &mut w3)?;
                let words = [w0, w1, w2, w3];
                let mut dec = self.decoder.lock().unwrap();
                for msg in dec.feed(&words[..count as usize]) {
                    (self.callback)(&msg);
                }
            }
            Ok(())
        }
        fn Cleanup(&self) -> windows_core::Result<()> {
            Ok(())
        }
    }
}
