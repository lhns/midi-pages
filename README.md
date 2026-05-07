# midi-pages

A virtual MIDI paging proxy for the Novation **Launchpad Mini MK3** and the Akai **APC mini**, primarily intended for use with **DasLight** (and any other host that maps MIDI notes to scenes/effects).

The 8x8 grid only gives you 64 buttons. `midi-pages` sits between your host software and the controller, reserves two physical buttons as **page up / page down**, and offsets the remaining 64 buttons by `page_index * 64`. From the host's point of view the controller has effectively `pages * 64` buttons.

LED state is **cached per page**, so switching pages instantly redraws the correct LEDs without the host re-sending anything. SysEx LED messages from the host are split per page on the fly.

## Status

Early. Tested manually on a Launchpad Mini MK3 (DasLight 5) and an APC mini.

## How it works

```
                          loopMIDI virtual ports                real USB-MIDI
   ┌──────────┐  ──────►  ┌─────────────────────┐  ──────►  ┌──────────────────┐
   │ DasLight │           │     midi-pages      │           │ Launchpad / APC  │
   └──────────┘  ◄──────  └─────────────────────┘  ◄──────  └──────────────────┘
```

Direction-by-direction:

- **Device → host:** pad press at physical note `n` while page `p` is active becomes note `n + p*64`. The two configured page-cycle buttons are swallowed and never reach the host.
- **Host → device:** Note On / Note Off / CC / Lighting-SysEx messages targeting note `m` are routed to physical pad `m % 64` only if `m / 64 == current_page`; otherwise they update an in-memory cache for that page.
- **Page change:** the device is cleared, then `led_cache[new_page]` is replayed in one batch. Held physical pads emit a Note Off on the previous page so the host sees no stuck notes.

## Setup (Windows)

1. Install [loopMIDI](https://www.tobias-erichsen.de/software/loopmidi.html) and create two ports, e.g. `midi-pages-host-in` and `midi-pages-host-out`.
2. Plug in your Launchpad / APC mini.
3. Copy `config.toml.example` to `config.toml` and adjust port names.
4. Run `midi-pages --list-ports` to confirm names match.
5. Run `midi-pages --config config.toml`.
6. In DasLight, select the loopMIDI ports as the MIDI controller (instead of the device directly).

On Linux/macOS `midir` can create virtual ports natively; loopMIDI is not needed.

## Configuration

See `config.toml.example`. One `[[device]]` section per controller. The two reserved page-cycle buttons are configurable per device (kind = `note` or `cc`, plus number).

## Building

```
cargo build --release
cargo test
```

Linux requires `libasound2-dev` (ALSA headers used by `midir`).

## License

MIT. See [LICENSE](LICENSE).
