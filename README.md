# midi-pages

A virtual MIDI paging proxy for the Novation **Launchpad Mini MK3** and the Akai **APC mini**, primarily intended for use with **DasLight** (and any other host that maps MIDI notes to scenes/effects).

The 8x8 grid only gives you 64 buttons. `midi-pages` sits between your host software and the controller, reserves two physical buttons as **page up / page down**, and presents N pages of 64 pads to the host. LED state is cached per page so switching pages instantly redraws the correct LEDs without the host re-sending anything.

## Modes

There are two ways the proxy can present pages to the host. Pick one in `config.toml` per device.

### `mode = "per_port"` (default, recommended)

The proxy creates **one virtual port pair per page**. Each page presents an identical Launchpad-shaped controller to the host. No 7-bit MIDI ceiling — you can have many pages. From DasLight's point of view they're independent controllers; you map each page exactly the way you'd map a single Launchpad. SysEx LED messages are forwarded byte-for-byte (no rewriting).

On Linux/macOS the virtual ports are created natively via `midir`. On Windows, `midi-pages` shells out to `loopMIDI.exe -new "<name>"` to create them — loopMIDI must be installed and its GUI running.

### `mode = "note_offset"`

A single virtual port pair; pages are encoded by adding `note_offset` to the note number (default `64`). MIDI notes are 7-bit so `pages * note_offset ≤ 128` — meaning at most 2 pages of 64 pads. SysEx LED messages from the host get walked, partitioned by page, and rewritten on the fly. Useful if you specifically want a single virtual controller in DasLight.

## How it works

```
                         loopMIDI / native virtual ports             real USB-MIDI
   ┌──────────┐  ──────►  ┌─────────────────────────────┐ ──────►  ┌──────────────────┐
   │ DasLight │           │         midi-pages          │          │ Launchpad / APC  │
   └──────────┘  ◄──────  └─────────────────────────────┘ ◄──────  └──────────────────┘
```

- **Device → host:** pad press at physical note `n` while page `p` is active is sent to page `p`'s host port (per-port mode), or rewritten as note `n + p*64` (note-offset mode). The two configured page-cycle buttons are swallowed and never reach the host.
- **Host → device:** message arrives on page `p`'s port (or addressed to logical note in page `p` in note-offset mode). If `p` is active, forwarded to the device; otherwise cached for that page.
- **Page change:** the device LEDs are cleared, then `led_cache[new_page]` is replayed in one batch. Held physical pads emit a Note Off on the previous page so the host sees no stuck notes.

## Setup

### Linux / macOS

1. Plug in your Launchpad / APC mini.
2. Copy `config.toml.example` to `config.toml` and adjust as needed.
3. `cargo run --release -- --config config.toml`. The proxy creates the virtual ports automatically.
4. In DasLight, pick the per-page virtual controllers (`<device-slug>-page1`, `-page2`, ...) as MIDI inputs/outputs.

### Windows

1. Install the [Windows MIDI Services SDK Runtime](https://aka.ms/MidiServicesLatestSdkRuntimeInstaller_Directx64) (or `winget install Microsoft.WindowsMIDIServicesSDK`). The runtime DLL must be present; the GUI tools are optional.
2. Plug in your Launchpad / APC mini.
3. Copy `config.toml.example` to `config.toml` and adjust as needed. On Windows the Launchpad Mini MK3 enumerates as `LPMiniMK3 MIDI` (port 1, the DAW port) and `MIDIIN2/MIDIOUT2 (LPMiniMK3 MIDI)` (port 2, the MIDI port). Programmer-mode SysEx and pad I/O go through port 2 — set `port_match = "LPMiniMK3 MIDI)"` (note the closing paren) to disambiguate.
4. `midi-pages.exe --config config.toml`. The proxy creates two WMS Loopback endpoints per page (one `…-in` for the DAW, one `…-out` internal). See ADR 0012 for the alternative Virtual Device transport.
5. Run `midi-pages --list-ports` if you want to verify what got created.
6. In your DAW, select the per-page **`-in`** port as the MIDI device for both directions (input AND output). See the gotcha below.

### Transport choice (Windows only)

`midi-pages` supports two WMS transports for the host-side virtual ports:

- **`loopback`** (default): each page creates an A↔B loopback pair. The DAW sees and binds to the `…-in` (A) endpoint; the proxy uses the `…-out` (B) endpoint internally. **Compatible with WinMM-based DAWs like DasLight 5.**
- **`virtual-device`** (opt-in via `windows_transport = "virtual-device"` in the device's `[[device]]` section): each page creates a single WMS Virtual Device endpoint. Cleaner UX (no `…-out` companion port), but as of WMS RC4 the WinMM compatibility shim has known bugs that break common WinMM-based DAWs ([microsoft/MIDI#886](https://github.com/microsoft/MIDI/issues/886) and friends). Only use if you've verified your DAW can consume MIDI from WMS Virtual Device endpoints.

## Configuration

See `config.toml.example`. The two reserved page-cycle buttons are configurable per device (`kind = "note" | "cc"`, plus `number`).

### DAW configuration: route everything through the proxy

For paging to work, the DAW must read **and write** to the per-page virtual ports — *not* directly to the physical Launchpad port. If the DAW is configured with the real Launchpad as its output, LED commands will bypass the proxy entirely; pad presses still get paged correctly via input, but switching pages and back leaves all DAW-set LEDs dark because the proxy never saw the LED writes and has nothing to replay.

In DasLight specifically: in the MIDI controller editor, set both *input* and *output* to `<device-slug>-page1-in` (and the other `-pageN-in` ports for the other pages). Disable / unselect any of the real device names (`LPMiniMK3 MIDI`, `MIDIIN2 (LPMiniMK3 MIDI)`, `MIDIOUT2 (LPMiniMK3 MIDI)`).

### Always shut down the proxy gracefully (Ctrl-C, not force-kill)

The proxy installs a Ctrl-C handler that cleanly tears down its WMS Virtual Device endpoints. **Force-killing the process** (Task Manager → End Task, `Stop-Process -Force`, etc.) skips that cleanup and leaks Virtual Device registrations into `midisrv`. After a few force-kills, `midisrv` becomes wedged: enumeration hangs, new endpoints can't be created, and the only recovery is a system reboot (`Restart-Service midisrv` doesn't reliably clear the leak). Always exit with Ctrl-C in the terminal.

## Building

```
cargo build --release
cargo test
```

Linux requires `libasound2-dev` (ALSA headers used by `midir`).

## Documentation

- [Architecture Decision Records](docs/adr/) — every non-trivial choice has an ADR explaining the why.

## License

MIT. See [LICENSE](LICENSE).
