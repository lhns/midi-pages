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

pub fn open_output_named(client: &str, needle: &str) -> Result<midir::MidiOutputConnection> {
    let mo = MidiOutput::new(client).context("create MidiOutput")?;
    let port = find_output(&mo, needle)?;
    mo.connect(&port, client)
        .map_err(|e| anyhow!("connect output `{needle}`: {e}"))
}

/// On Windows, ensure a Windows MIDI Services loopback pair exists with the
/// given names. The endpoints are *transient* — they live until the next
/// `midisrv` restart or PC reboot — so we re-create on every startup if
/// needed. Skips creation if the host endpoint is already enumerable.
/// No-op on non-Windows (Linux/macOS create virtual ports inline via midir).
///
/// Kept for legacy callers; new code should use the Virtual Device path —
/// see `WindowsVirtualHostPort`.
pub fn ensure_loopback_pair(host_name: &str, proxy_name: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows_wms::ensure_pair(host_name, proxy_name)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (host_name, proxy_name);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub use windows_wms_virt::WindowsVirtualHostPort;


#[cfg(target_os = "windows")]
mod windows_wms {
    use super::*;
    use std::time::{Duration, Instant};
    use tracing::{debug, info};
    use windows_core::HSTRING;

    use crate::wms_bindings::Microsoft::Windows::Devices::Midi2::Endpoints::Loopback::{
        MidiLoopbackEndpointCreationConfig, MidiLoopbackEndpointDefinition,
        MidiLoopbackEndpointManager,
    };

    pub(super) fn ensure_pair(host_name: &str, proxy_name: &str) -> Result<()> {
        if port_exists(host_name)? && port_exists(proxy_name)? {
            return Ok(());
        }
        info!(host = %host_name, proxy = %proxy_name, "creating WMS loopback pair");
        ensure_wms_dll_loaded()?;

        let unique_a = unique_id_for(host_name);
        let unique_b = unique_id_for(proxy_name);
        let association = association_id_for(host_name, proxy_name);

        let def_a = MidiLoopbackEndpointDefinition {
            Name: HSTRING::from(host_name),
            UniqueId: HSTRING::from(unique_a.as_str()),
            Description: HSTRING::new(),
        };
        let def_b = MidiLoopbackEndpointDefinition {
            Name: HSTRING::from(proxy_name),
            UniqueId: HSTRING::from(unique_b.as_str()),
            Description: HSTRING::new(),
        };

        let config = MidiLoopbackEndpointCreationConfig::new()
            .map_err(|e| anyhow!("MidiLoopbackEndpointCreationConfig::new failed: {e}. \
                Is the Windows MIDI Services runtime installed? See \
                https://aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64"))?;
        config.SetAssociationId(association)
            .map_err(|e| anyhow!("SetAssociationId failed: {e}"))?;
        config.SetEndpointDefinitionA(&def_a)
            .map_err(|e| anyhow!("SetEndpointDefinitionA failed: {e}"))?;
        config.SetEndpointDefinitionB(&def_b)
            .map_err(|e| anyhow!("SetEndpointDefinitionB failed: {e}"))?;

        let result = MidiLoopbackEndpointManager::CreateTransientLoopbackEndpoints(&config)
            .map_err(|e| anyhow!("CreateTransientLoopbackEndpoints failed: {e}"))?;
        let success = result
            .Success()
            .map_err(|e| anyhow!("read result.Success: {e}"))?;
        if !success {
            let info = result
                .ErrorInformation()
                .map(|h| h.to_string())
                .unwrap_or_default();
            let code = result
                .ErrorCode()
                .map(|c| c.0)
                .unwrap_or(-1);
            return Err(anyhow!(
                "WMS loopback creation reported failure (code {code}): {info}"
            ));
        }
        debug!(association = ?association, "WMS loopback creation succeeded");

        wait_for_port(host_name, Duration::from_secs(3))?;
        wait_for_port(proxy_name, Duration::from_secs(3))?;
        Ok(())
    }

    fn port_exists(name: &str) -> Result<bool> {
        let mi = MidiInput::new("midi-pages-probe-in")?;
        for port in mi.ports() {
            if mi.port_name(&port).unwrap_or_default() == name {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn wait_for_port(name: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if port_exists(name)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "WMS loopback `{name}` was created but didn't appear in WinMM \
                     within {:?}.",
                    timeout
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Stable per-name unique identifier for a loopback endpoint. WMS uses
    /// these to distinguish persistent identities; deterministic IDs let
    /// re-runs reuse the same logical endpoint.
    pub(super) fn unique_id_for(name: &str) -> String {
        format!("{:016x}", fnv1a64(name.as_bytes()))
    }

    /// Stable per-pair association GUID. Both endpoints in a pair must share
    /// the same association ID. We derive deterministic bytes from a hash of
    /// the pair so re-runs produce the same GUID.
    fn association_id_for(host: &str, proxy: &str) -> windows_core::GUID {
        let h1 = fnv1a64(host.as_bytes());
        let h2 = fnv1a64(proxy.as_bytes());
        let bytes = [
            (h1 >> 56) as u8, (h1 >> 48) as u8, (h1 >> 40) as u8, (h1 >> 32) as u8,
            (h1 >> 24) as u8, (h1 >> 16) as u8, (h1 >> 8) as u8, h1 as u8,
            (h2 >> 56) as u8, (h2 >> 48) as u8, (h2 >> 40) as u8, (h2 >> 32) as u8,
            (h2 >> 24) as u8, (h2 >> 16) as u8, (h2 >> 8) as u8, h2 as u8,
        ];
        windows_core::GUID::from_values(
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            u16::from_be_bytes([bytes[4], bytes[5]]),
            u16::from_be_bytes([bytes[6], bytes[7]]),
            [bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]],
        )
    }

    /// Load `Microsoft.Windows.Devices.Midi2.dll` from the WMS install dir so
    /// that WinRT activation can locate the loopback factory classes. The DLL
    /// itself contains the activatable factory exports; pre-loading it makes
    /// `RoActivateInstance` find the embedded metadata even though the class
    /// isn't system-registered.
    pub(super) fn ensure_wms_dll_loaded() -> Result<()> {
        use std::sync::OnceLock;
        static LOADED: OnceLock<Result<()>> = OnceLock::new();
        let result: &Result<()> = LOADED.get_or_init(|| {
            let candidates = [
                r"C:\Program Files\Windows MIDI Services\Desktop App SDK Runtime\Microsoft.Windows.Devices.Midi2.dll",
                r"C:\Program Files (x86)\Windows MIDI Services\Desktop App SDK Runtime\Microsoft.Windows.Devices.Midi2.dll",
            ];
            for c in candidates {
                if std::path::Path::new(c).exists() {
                    let wide: Vec<u16> = c.encode_utf16().chain(std::iter::once(0)).collect();
                    // SAFETY: LoadLibraryW is safe to call with a valid wide string.
                    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
                    if h.is_null() {
                        return Err(anyhow!(
                            "LoadLibraryW({c}) failed: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                    return Ok(());
                }
            }
            Err(anyhow!(
                "Microsoft.Windows.Devices.Midi2.dll not found. Install the \
                 Windows MIDI Services runtime from \
                 https://aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64."
            ))
        });
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow!("{e}")),
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut core::ffi::c_void;
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
}

#[cfg(target_os = "windows")]
mod windows_wms_virt {
    //! Virtual Device-based host port: exposes ONE MIDI endpoint to the OS
    //! and uses an in-process plugin callback for the proxy side. Replaces
    //! the loopback A↔B pair model (which exposed both endpoints to WinMM).

    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tracing::info;
    use windows_core::{HSTRING, IInspectable, Ref, GUID, implement};

    use crate::midi::ump;
    use crate::wms_bindings::Microsoft::Windows::Devices::Midi2::Endpoints::Virtual::{
        MidiVirtualDeviceCreationConfig, MidiVirtualDeviceManager,
    };
    use crate::wms_bindings::Microsoft::Windows::Devices::Midi2::{
        IMidiEndpointConnectionSource, IMidiEndpointMessageProcessingPlugin,
        IMidiEndpointMessageProcessingPlugin_Impl, MidiDeclaredEndpointInfo,
        MidiEndpointConnection, MidiMessageReceivedEventArgs, MidiSession,
    };

    /// Owned WMS Virtual Device endpoint + session + open connection + plugin.
    /// Dropping this releases all of them and the endpoint disappears from
    /// the system MIDI enumeration.
    ///
    /// SAFETY: the wrapped WinRT class types are marked `Send + Sync` in the
    /// generated bindings, but our wrapping struct doesn't auto-inherit those
    /// marks because of the `Box<dyn Fn>` field. WinRT objects activated by
    /// `RoActivateInstance` live in the MTA and are safe to call from any
    /// thread; the COM runtime serializes per-object as needed.
    pub struct WindowsVirtualHostPort {
        // Order matters for Drop: connection first, then session, then device.
        // (Actually all three are RAII-managed by the WMS service; we just
        // hold references to keep them alive.)
        _connection: MidiEndpointConnection,
        _session: MidiSession,
        _device: crate::wms_bindings::Microsoft::Windows::Devices::Midi2::Endpoints::Virtual::MidiVirtualDevice,
        _plugin: IMidiEndpointMessageProcessingPlugin,
        // Used for sending bytes out to peers (DAW reads).
        sender: MidiEndpointConnection,
    }

    unsafe impl Send for WindowsVirtualHostPort {}
    unsafe impl Sync for WindowsVirtualHostPort {}

    impl WindowsVirtualHostPort {
        /// Create a Virtual Device with the given external-facing name and
        /// register `on_recv` to be called with byte-format MIDI 1.0 messages
        /// arriving from peers.
        pub fn create<F>(name: &str, on_recv: F) -> Result<Self>
        where
            F: Fn(&[u8]) + Send + Sync + 'static,
        {
            super::windows_wms::ensure_wms_dll_loaded()?;

            // Generate a stable ProductInstanceId per name so reruns of
            // midi-pages reuse the same logical device identity.
            let product_id =
                format!("midi-pages.{}", super::windows_wms::unique_id_for(name));
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
            // we don't speak MIDI 2.0 and don't want it in our plugin callback.
            let _ = device.SetSuppressHandledMessages(true);

            let dev_id = device
                .DeviceEndpointDeviceId()
                .map_err(|e| anyhow!("DeviceEndpointDeviceId: {e}"))?;
            let session = MidiSession::Create(&HSTRING::from(format!("midi-pages-{name}")))
                .map_err(|e| anyhow!("MidiSession::Create: {e}"))?;
            let connection = session
                .CreateEndpointConnection(&dev_id)
                .map_err(|e| anyhow!("CreateEndpointConnection: {e}"))?;

            // Build the plugin that translates UMP -> MIDI 1.0 bytes -> on_recv.
            let plugin_obj = PluginShim {
                plugin_id: GUID::from_u128(0xb1d10001_0000_4000_8000_000000000000 ^ super::windows_wms::unique_id_for(name).parse::<u128>().unwrap_or(0)),
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

            // Wait for the externally-visible endpoint to appear in legacy
            // enumeration so downstream `find_*` calls succeed. midisrv can
            // be slow when multiple endpoints are created in quick succession.
            wait_for_port(name, Duration::from_secs(20))?;

            Ok(Self {
                sender: connection.clone(),
                _connection: connection,
                _session: session,
                _device: device,
                _plugin: plugin,
            })
        }

        /// Send byte-format MIDI 1.0 message(s) to peers attached to this
        /// virtual endpoint. The input may be one complete message; encoded
        /// as UMP and pushed out word-by-word.
        pub fn send(&self, bytes: &[u8]) -> Result<()> {
            let words = ump::encode(bytes, 0);
            if words.is_empty() {
                return Ok(());
            }
            // SendSingleMessageWords variants take 1..=4 words as ONE UMP message.
            // Channel voice is always 1; MT3 packets are 2; we may need multiple
            // calls (one per MT3 packet) for long SysEx.
            let mut i = 0;
            while i < words.len() {
                let mt = (words[i] >> 28) & 0xF;
                let packet_words: usize = match mt {
                    0x0 | 0x1 | 0x2 => 1,
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

    fn wait_for_port(name: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let mi = MidiInput::new("midi-pages-probe-in")?;
            if mi
                .ports()
                .iter()
                .any(|p| mi.port_name(p).unwrap_or_default() == name)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "WMS Virtual endpoint `{name}` didn't appear in WinMM within {:?}",
                    timeout
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[implement(IMidiEndpointMessageProcessingPlugin)]
    struct PluginShim {
        plugin_id: GUID,
        plugin_name: Mutex<HSTRING>,
        plugin_tag: Mutex<Option<IInspectable>>,
        is_enabled: Mutex<bool>,
        decoder: Mutex<ump::Decoder>,
        callback: Box<dyn Fn(&[u8]) + Send + Sync>,
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
