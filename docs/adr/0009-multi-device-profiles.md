# 0009 — One process, N independent device profiles

- **Status:** accepted (revised 2026-05-13: split `port_match` form)
- **Date:** 2026-05-07, last revised 2026-05-13

## Context

The user owns both a Launchpad Mini MK3 and an APC mini and wants to use them simultaneously, each with its own paging.

## Decision

The config is a list of `[[device]]` profiles. The proxy spawns one set of MIDI port pairs and one `PageState` per profile. Profiles are independent — pages and caches are not shared.

## Consequences

- Adding a third controller is "add another `[[device]]` block".
- One process, one log, one place to crash if any device errors. (Per-device error isolation can be added later.)
- Port matching is by **substring** of the OS port name. Config validation rejects overlapping `port_match` strings.

### `port_match` shape

`port_match` accepts either form:

- A plain string: `port_match = "Launchpad Mini"`. Used as a substring for both input and output port names.
- A split table: `port_match = { in = "Foo IN", out = "Foo OUT" }`. Each direction gets its own substring. Useful for devices whose driver gives the input and output ports completely different names (no shared substring).

The duplicate-detection check runs per direction: two devices conflict if their input substrings are equal OR their output substrings are equal.

## Alternatives considered

- **One process per device.** Possible but harder to manage; users would need to launch and supervise each.
- **Shared paging across devices.** Surprising and rarely useful.
