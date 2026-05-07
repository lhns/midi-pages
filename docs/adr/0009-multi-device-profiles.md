# 0009 — One process, N independent device profiles

- **Status:** accepted
- **Date:** 2026-05-07

## Context

The user owns both a Launchpad Mini MK3 and an APC mini and wants to use them simultaneously, each with its own paging.

## Decision

The config is a list of `[[device]]` profiles. The proxy spawns one set of MIDI port pairs and one `PageState` per profile. Profiles are independent — pages and caches are not shared.

## Consequences

- Adding a third controller is "add another `[[device]]` block".
- One process, one log, one place to crash if any device errors. (Per-device error isolation can be added later.)
- Port matching is by **substring** of the OS port name — config validation rejects overlapping `port_match` strings.

## Alternatives considered

- **One process per device.** Possible but harder to manage; users would need to launch and supervise each.
- **Shared paging across devices.** Surprising and rarely useful.
