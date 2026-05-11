# 0012 — Auto-create virtual host-side ports

- **Status:** accepted (revised 2026-05-12)
- **Date:** 2026-05-08, revised 2026-05-11, revised 2026-05-12

## Context

Per-port mode (ADR 0011) needs N virtual port pairs per controller. Asking users to create 8+ ports manually before each session is friction we want to avoid.

Available primitives:

- **Linux & macOS:** `midir::MidiInput::create_virtual` and `MidiOutput::create_virtual` create OS-level virtual ports natively.
- **Windows:** historically the only option was loopMIDI (a GUI app on top of Tobias Erichsen's `teVirtualMIDI` driver). Microsoft is now shipping **Windows MIDI Services** (WMS) — a new MIDI 2.0 service with native loopback endpoint creation.

### Approaches we tried and rejected

1. **`loopMIDI.exe -new "PortName"`** — claimed in the original draft of this ADR. The flag does not exist in loopMIDI v1.0.16.27; the binary contains GUI labels only. Calling `loopMIDI.exe -new foo` foregrounds the GUI and does nothing.
2. **Direct FFI to `teVirtualMIDI64.dll::virtualMIDICreatePortEx2`** — returns a non-NULL handle with `GetLastError() == 0` but creates no port. The driver is licensed only for use inside loopMIDI / loopBe1 / rtpMIDI; calls from other processes are silently no-op'd.
3. **SendMessage-driving loopMIDI's GUI** to fill its `TEdit` and click `+` — physically adds the port to loopMIDI's grid. But on Windows MIDI Services 2026 the new `midisrv` service does not catch the PnP event for the new port (microsoft/MIDI#835), so the port doesn't appear in WinMM until midisrv restarts. Restarting midisrv with loopMIDI still running causes loopMIDI to lose its friendly port names permanently (until the next reboot), leaving every port shown as a generic `teVirtualMIDI - Virtual MIDI Dr` slot. The 16-port driver-wide cap is also a hard ceiling here.
4. **Shelling out to `midi.exe loopback create`** (WMS Tools CLI) — works, but requires users to install the full WMS Tools (~180 MB).

## Decision

Use the WMS WinRT API directly, with **two transports** selectable per device via `windows_transport`:

- **`loopback` (default)** uses `MidiLoopbackEndpointManager::CreateTransientLoopbackEndpoints`. Each logical page is an A↔B pair, both endpoints visible to MIDI consumers. The DAW binds to the `…-in` endpoint; the proxy uses `…-out`. Two ports per page is a slight UX footgun (we warn in README) but **compatible with WinMM-based DAWs** like DasLight 5 — verified working end-to-end on 2026-05-11.
- **`virtual-device` (opt-in)** uses `MidiVirtualDeviceManager::CreateVirtualDevice`. Each logical page is a single externally-visible endpoint; the proxy's side is callback-only and never enumerated. Cleaner naming, but the WMS WinMM compatibility shim has known bugs ([microsoft/MIDI#886](https://github.com/microsoft/MIDI/issues/886) and friends) that prevent older WinMM-based DAWs from receiving MIDI. Use only with DAWs verified to consume MIDI from WMS Virtual Device endpoints.

We attempted to switch the default to Virtual Device on 2026-05-12. midir consumed traffic correctly, but DasLight 5 received nothing despite the endpoint being enumerable — pinned to the WMS shim issue above. Reverted Loopback to default, kept Virtual Device available as opt-in for future-proofing.

- **Linux & macOS:** call `midir::create_virtual_*` for each host-side port that isn't already present (unchanged; we still create two virtual ports per page on Unix because midir's bidirectional virtual-port story is platform-dependent and we kept the existing layout to avoid breaking Linux/macOS users mid-flight).
- **Windows:** for each page, call `MidiVirtualDeviceCreationConfig::CreateInstance` + `MidiVirtualDeviceManager::CreateVirtualDevice` from `Microsoft.Windows.Devices.Midi2.dll`, then open a `MidiEndpointConnection` to the device's internal endpoint and attach a Rust-implemented `IMidiEndpointMessageProcessingPlugin` (via `windows_core::implement`). Incoming UMP messages → decoded to MIDI 1.0 byte stream by `src/midi/ump.rs` → handed to the proxy's existing `handle_host_in*` paths. Outgoing bytes → encoded as UMP (MT2 channel-voice + MT3 SysEx packets) → `MidiEndpointConnection::SendSingleMessageWords*`. The bindings are generated from `vendor/wms/Microsoft.Windows.Devices.Midi2.winmd` at build time via `windows-bindgen`, so end users don't need any .NET/Tools install — just the ~3 MB WMS runtime DLL.
- The proxy never creates the *device* port — that's real hardware; missing-device is always a hard error.
- The proxy never restarts `midisrv` — we explicitly avoid the friendly-name-loss footgun from approach #3.

### Implementation notes

- For unpackaged Win32/Rust apps, WinRT activation needs the runtime DLL to be findable. We `LoadLibraryW` `Microsoft.Windows.Devices.Midi2.dll` from its install path before the first `RoActivateInstance` call; the embedded factory metadata is then discoverable.
- We cache the load via `OnceLock` so multiple ports don't reload the DLL.
- Per Virtual Device we derive a deterministic `ProductInstanceId` from a hash of the host name so re-runs reuse the same logical device identity.
- After `MidiEndpointConnection::Open()` we poll midir until the public endpoint appears in WinMM (up to 20 s; `midisrv` is slow when several endpoints are created in succession).
- We set `MidiVirtualDevice::SetSuppressHandledMessages(true)` to swallow MIDI 2.0 stream-config negotiation traffic; we declare `SupportsMidi20Protocol = false` and only speak MIDI 1.0 bytes.
- **Force-kill caveat**: WMS doesn't reliably clean up Virtual Device registrations when the owner process dies without dropping its `MidiVirtualDevice` references. Repeated force-kills wedge `midisrv` until the system is rebooted; even `Restart-Service midisrv` doesn't reliably recover. We install a Ctrl-C handler in `main.rs` that flips a global `SHUTDOWN` flag and unparks all worker threads so they return cleanly and Drop runs. Users must exit with Ctrl-C, not Task Manager.

### What end users install

- The WMS Desktop App SDK Runtime, which includes `Microsoft.Windows.Devices.Midi2.dll` (~3 MB). Available via [aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64](https://aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64) or `winget install Microsoft.WindowsMIDIServicesSDK`. Eventually this will ship in Windows by default as the MIDI Services rollout completes.

## Consequences

- Linux & macOS: zero-config experience.
- Windows: small one-time WMS runtime install. midi-pages auto-creates one Virtual Device endpoint per page on every startup; deterministic `ProductInstanceId` lets re-runs reuse the same logical identity. Startup takes ~5 s × *pages* because `midisrv` is slow to register endpoints in succession.
- DAW only sees one MIDI port per page (no `-in`/`-out` confusion). The proxy's side is a callback-only handle, never enumerable.
- We don't bundle the teVirtualMIDI SDK (paid licence, not redistributable for general use).
- We vendor `Microsoft.Windows.Devices.Midi2.winmd` (137 KB, MIT-licensed) so contributors can build without WMS installed locally; the actual runtime is needed only at runtime.

## Alternatives considered

- **Stay on Loopback transport.** Cleaner to implement but exposes two endpoints per page (the source of the DasLight-bypass class of bug). Force-kill safer than Virtual Device — no midisrv leak.
- **Bundle the WMS Tools install** (~180 MB on the user side) and shell out to `midi.exe loopback create`. Rejected: heavy install for the same outcome.
- **Bundle teVirtualMIDI SDK.** Rejected: paid commercial licence, plus we'd still hit microsoft/MIDI#835 + the friendly-name corruption.
- **Document manual loopMIDI port creation.** Rejected: per-config-change friction and 16-port driver cap.
