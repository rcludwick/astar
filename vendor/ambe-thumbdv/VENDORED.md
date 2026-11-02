# Vendored: `ambe-thumbdv`

This directory is a **verbatim vendored copy** of the `ambe-thumbdv` crate — the
DVSI AMBE-3000 packet driver that talks to a ThumbDV / DV3000 USB dongle. It is
the hardware vocoder path for D-Star; nothing else in this repository can encode
or decode AMBE.

| | |
|---|---|
| Upstream | <https://github.com/rcludwick/ambe> |
| Path upstream | `crates/ambe-thumbdv` |
| Revision | `41c35d817af39016707f58b1bbbc6b1bfaeedabe` (2026-09-06) |
| Licence | MIT OR Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE`) |
| Copyright | Rob Ludwick |

## Why it is vendored

The rest of this workspace is AGPL-3.0-only. `ambe-thumbdv` is dual MIT/Apache
and stays that way: it lives under `vendor/` rather than `crates/` so the
licence boundary is visible in the tree, and its `Cargo.toml` hard-codes
`license`, `version`, `edition` and `repository` instead of inheriting them from
the workspace — inheriting would resolve `license` to `AGPL-3.0-only` and
silently relicense someone else's terms.

Vendoring also removes the last `git = "https://github.com/rcludwick/ambe"`
dependency from the build, so a clean checkout builds fully offline.

`astar-codec` consumes it as a path dependency behind the `ambe-hw` feature:

```toml
ambe-thumbdv = { path = "../../vendor/ambe-thumbdv", optional = true }
```

## What is vendored, and only that

`src/` — the packet driver and its `thumbdv-rig` binary. Nothing else from
upstream is used: D-Star, System Fusion and every other AMBE network in this
repository decode and encode on the dongle, and there is no software AMBE
backend of any kind here.

## Local changes (diverges from upstream)

**2026-10-11 — vocoder trust (on-air safety).** FTDI's `0403:6015` covers the
whole FT-X family, which is common in USB CAT/PTT cables, and opening a tty
raises DTR/RTS. The old `candidate_ports()` + `detect()` therefore opened, and
wrote AMBE bytes into, any such cable. Changed here, in this copy only:

- `SerialTransport::candidate_ports()` is gone. `candidate_ports_trusted(&[serial])`
  returns only FT-X-class ports whose USB serial number is on the list (pure
  policy: `trusted_ports_from`). `list_candidates()` / `candidates_from` list
  FT-X ports by serial and path for enrolment, without opening anything.
- `SerialTransport::open` drives DTR and RTS low straight after open
  (`Transport::deassert_control_lines`, default no-op for mocks), and fails the
  open if it cannot.
- `detect()` is gone; `detect_trusted(&[serial])` walks only trusted ports and
  returns the new `DeviceError::NoTrustedDevice` when there are none.
- New `enrol` module: `ChipClass::of(prodid)` (AMBE3000 / AMBE3003 / other) and
  `probe_with(port, factory)`, a RESET + PRODID probe over a transport factory,
  deasserting DTR/RTS before the first write.
- The `detect_without_hardware_errors_not_hangs` test was removed: it called
  the untrusted `detect()` and so opened every FT-X port on the machine running
  `cargo test --workspace`.
- `thumbdv-rig` opens only ports whose serial is in `ASTAR_VOCODER_SERIALS`.

These are deliberate and must survive a re-vendor: port them upstream or
re-apply them, never drop them.

## Updating

Copy `src/` from upstream verbatim (then re-apply the local changes above), keep this file's revision row current, and
do **not** let the four hard-coded metadata keys revert to `.workspace = true`.
Local edits are discouraged — fix upstream and re-vendor, so the two copies do
not drift.
