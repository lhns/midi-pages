# 0010 — GitHub Actions CI and release pipeline

- **Status:** accepted
- **Date:** 2026-05-07

## Context

The proxy is a single binary distributed to end users on Windows, Linux, and macOS. We want every push to produce a downloadable binary the user can sanity-check against real hardware, and tagged releases to ship to the GitHub Releases page automatically.

## Decision

A single `.github/workflows/ci.yml` with three jobs:

1. **`test`** — runs `cargo fmt`, `clippy -D warnings`, and `cargo test --locked` on Linux, Windows, and macOS. Linux additionally runs `cargo llvm-cov` with `--fail-under-lines 90`.
2. **`build`** — cross-builds release binaries for `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, and `aarch64-apple-darwin`. Packages each as a zip (Windows) or tar.gz (Unix) and uploads as a workflow artifact on every push and PR.
3. **`release`** — only runs on tags matching `v*`. Downloads the artifacts and attaches them to a GitHub Release using `softprops/action-gh-release`.

## Consequences

- Every PR yields a downloadable binary on each OS for hardware testing.
- Tagging `vX.Y.Z` is the only step needed to cut a release.
- Coverage gate prevents silent regressions in the bug-prone parser and state-machine code.

### Negative

- Three runners per push is more CI minutes than a single-OS pipeline.
- Coverage gate means the first PR after a refactor occasionally needs an explicit test addition.

## Alternatives considered

- **`cargo-dist`.** Higher-level, opinionated. Rejected for now in favour of an explicit workflow we can read at a glance and tweak.
- **No coverage gate.** Rejected: the SysEx parser and proxy state machine are exactly where bugs would live and never be caught by manual testing.
