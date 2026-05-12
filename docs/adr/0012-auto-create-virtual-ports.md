# 0012 — Auto-create virtual host-side ports

- **Status:** accepted (revised 2026-05-12)
- **Date:** 2026-05-08, last revised 2026-05-12

## Context

Per-port mode (ADR 0011) needs N virtual MIDI endpoints per controller — one per page. Asking users to create them manually is friction we want to avoid.

Available primitives:

- **Linux & macOS:** `midir::MidiInput::create_virtual` and `MidiOutput::create_virtual` create OS-level virtual ports natively.
- **Windows:** historically the only option was loopMIDI (a GUI app on top of Tobias Erichsen's `teVirtualMIDI` driver). Microsoft now ships **Windows MIDI Services** (WMS) — a new MIDI 2.0 service with first-class virtual endpoint creation.

### Approaches we tried and rejected

1. **`loopMIDI.exe -new "PortName"`** — claimed in the original draft of this ADR. The flag does not exist in loopMIDI v1.0.16.27; the binary contains GUI labels only. Calling `loopMIDI.exe -new foo` foregrounds the GUI and does nothing.
2. **Direct FFI to `teVirtualMIDI64.dll::virtualMIDICreatePortEx2`** — returns a non-NULL handle with `GetLastError() == 0` but creates no port. The driver is licensed only for use inside loopMIDI / loopBe1 / rtpMIDI; calls from other processes are silently no-op'd.
3. **SendMessage-driving loopMIDI's GUI** — physically adds the port to loopMIDI's grid, but on Windows MIDI Services 2026 the new `midisrv` doesn't catch the PnP event for the port until restarted ([microsoft/MIDI#835](https://github.com/microsoft/MIDI/issues/835)). The naive midisrv-restart workaround corrupts loopMIDI's friendly-name registration permanently until reboot. Plus a 16-port driver-wide cap.
4. **Shelling out to `midi.exe loopback create`** (WMS Tools CLI) — works, but requires users to install the full WMS Tools (~180 MB) and DAWs see *two* ports per pair (host + proxy-internal), inviting binding mistakes.
5. **`MidiLoopbackEndpointManager::CreateTransientLoopbackEndpoints` via WinRT** — the API behind option 4. Works fine for WinMM consumers but inherits the same two-ports-per-page UX wart.

## Decision

Use the WMS **Virtual Device** transport: `MidiVirtualDeviceManager::CreateVirtualDevice` exposes exactly one endpoint per page to peers, and gives the creator process a callback-based `MidiVirtualDevice` handle for the in-process side. From `Microsoft.Windows.Devices.Midi2.dll`, bindings generated at build time from the vendored `Microsoft.Windows.Devices.Midi2.winmd` via `windows-bindgen`.

- **Linux & macOS:** `midir::create_virtual_*` for each per-page name (unchanged).
- **Windows:** for each page, `MidiVirtualDeviceCreationConfig::CreateInstance` + `MidiVirtualDeviceManager::CreateVirtualDevice`, then open a `MidiEndpointConnection` to the device's internal endpoint and attach a Rust-implemented `IMidiEndpointMessageProcessingPlugin` (via `windows_core::implement`). Incoming UMP messages → decoded to MIDI 1.0 byte stream by `src/midi/ump.rs` → handed to the proxy's `handle_host_in*` paths. Outgoing bytes → encoded as UMP (MT2 channel-voice + MT3 SysEx packets) → `MidiEndpointConnection::SendSingleMessageWords*`.
- The proxy never creates the *device* port — that's real hardware; missing-device is always a hard error.

End users only need the **WMS Desktop App SDK Runtime** (~3 MB DLL — install via [aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64](https://aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64) or `winget install Microsoft.WindowsMIDIServicesSDK`). Eventually this ships with Windows by default as the MIDI Services rollout completes.

### Implementation notes

- Unpackaged Win32/Rust apps don't get WinRT activation registration via side-by-side manifest. We `LoadLibraryW` `Microsoft.Windows.Devices.Midi2.dll` from its install path before the first `RoActivateInstance` call; the embedded factory metadata is then discoverable. Cached via `OnceLock`.
- Per Virtual Device we derive a deterministic `ProductInstanceId` from a hash of the host name so re-runs reuse the same logical device identity.
- After `MidiEndpointConnection::Open()` we poll midir until the public endpoint appears in WinMM (up to 20 s; midisrv can be slow when several endpoints are created in succession).
- We set `MidiVirtualDevice::SetSuppressHandledMessages(true)` to swallow MIDI 2.0 stream-config negotiation traffic; we declare `SupportsMidi20Protocol = false` and only speak MIDI 1.0 bytes.
- **Force-kill caveat**: WMS doesn't reliably clean up Virtual Device registrations when the owner process dies without dropping its `MidiVirtualDevice` references. Repeated force-kills wedge `midisrv` until reboot. We install a Win32 console-control handler in `main.rs` that catches Ctrl+C, Ctrl+Break, terminal close (X button), Logoff and Shutdown; it flips a global `SHUTDOWN` flag and unparks all worker threads so they return cleanly and `Drop` runs. **Do not** stop the proxy with Task Manager → End Task or `Stop-Process` / `taskkill /F` — Windows `TerminateProcess` runs no userland cleanup.

### Compatibility footnote (DasLight 5)

Some old WinMM-based DAWs ([microsoft/MIDI#886](https://github.com/microsoft/MIDI/issues/886)) reportedly don't see MIDI from WMS Virtual Device endpoints. We tested DasLight 5 against the proxy: it works correctly once the DAW is configured to bind both directions to the per-page port (and not to the physical Launchpad outputs as well). The earlier "DasLight doesn't see traffic" bug turned out to be a DAW config error, not a WMS shim bug.

## Consequences

- Linux & macOS: zero-config experience.
- Windows: small one-time WMS runtime install. midi-pages auto-creates one Virtual Device endpoint per page on every startup; deterministic `ProductInstanceId` lets re-runs reuse the same logical identity. Startup takes ~5 s × *pages* because midisrv is slow to register endpoints in succession.
- DAW sees one MIDI port per page. The proxy's side is a callback-only handle, never enumerable — no "wrong endpoint" footgun.
- We don't bundle the teVirtualMIDI SDK (paid licence, not redistributable for general use).
- We vendor `Microsoft.Windows.Devices.Midi2.winmd` (137 KB, MIT-licensed) so contributors can build without WMS installed locally; the actual runtime is needed only at runtime.

## Alternatives considered

- **Stay on the WMS Loopback transport.** Simpler — no plugin, no UMP transcoder — but exposes two endpoints per page (the source of the wrong-binding class of bug). Force-kill safer than Virtual Device — no midisrv leak.
- **Bundle the WMS Tools install** (~180 MB on the user side) and shell out to `midi.exe loopback create`. Rejected: heavy install for the same outcome.
- **Bundle teVirtualMIDI SDK.** Rejected: paid commercial licence, plus we'd still hit microsoft/MIDI#835 + the friendly-name corruption.
- **Document manual loopMIDI port creation.** Rejected: per-config-change friction and 16-port driver cap.
