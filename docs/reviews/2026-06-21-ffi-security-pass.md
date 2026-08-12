# FFI security pass — 2026-06-21

Security review of the C-ABI surface: `crates/astar-sys/src/ffi.rs` (28
`extern "C"` fns) and `crates/astar-serial-sys/src/ffi.rs` (5), plus the
engine code behind every caller-buffer write. Covers the surface through
`iax-2095` (mic profile) and `iax-6b58` (WT token mint).

## Findings & fixes (all four applied)

| # | Severity | Issue | Fix |
|---|----------|-------|-----|
| F1 | Low (UB class) | `iax_serial_close` ran `Poller::fail_safe()` + `Drop` (serial I/O) **without** `catch_unwind` — the only worker extern fn that didn't, violating the module's own invariant. A panic in teardown would unwind across `extern "C"` (UB). | Wrapped the body in `catch_unwind(AssertUnwindSafe(..))`, mirroring `iax_station_free`. |
| F2 | Low (contract) | The `IaxSerial*` handle isn't thread-safe (`iax_serial_ptt_tick` takes `&mut`), but that was undocumented — unlike `IaxStation*`, which is internally synchronized. A consumer could wrongly assume parity. | Documented "one thread per handle" on the `IaxSerial` struct and `iax_serial_ptt_tick` (now also in the generated `astarserial.h`). |
| F3 | Low (hardening) | The credential-resolver scrubbed its 512-byte secret buffer with `buf.fill(0)`, which the optimizer may elide on a soon-dropped buffer. | Switched to `zeroize::Zeroize::zeroize` (non-elidable volatile write). Added `zeroize` dep to `astar-sys`. |
| F4 | Info (hardening) | `iax_station_mic_spectrum` built `from_raw_parts_mut(out, cap)` from the caller's `cap`. **Verified safe** (the engine's `copy_into` bounds writes to `out.len().min(48)`), but a caller over-stating `cap` is inherent C-ABI risk. | Clamp `cap` to `IAX_SPECTRUM_BINS` before constructing the slice, matching the documented "size to `IAX_SPECTRUM_BINS`" contract. |

## What was already solid (no change needed)

- **No panic crosses the boundary** in `astar-sys` (28/28 bodies guarded;
  the 2 exempt are pure `'static`-string matches; `list_inputs`/`list_outputs`
  forward to a guarded helper). With F1, the serial ABI is now equally covered.
- Null checks on every handle + out-pointer, including `out == NULL && cap > 0`.
- Every caller-buffer writer (`fill_buf`, `iax_serial_autodetect`, `copy_into`)
  is truncation-safe, reserves the NUL, and returns the needed size for retry.
- C-string ingress is panic-free: `opt_str` (lossy) / `req_str` (→ `IAX_ERR_UTF8`);
  no `to_str().unwrap()`.
- **Secret-free out-surface confirmed:** `IaxState`/`IaxEvent` carry no credential
  fields; `iax_error_text` returns generic `'static` strings; hangup/register
  reason strings are deliberately not carried. Enforced by `check-cbindgen.sh`.
- Ownership is clean: handles are `Box::into_raw`/`from_raw` with a single-free
  contract; `error_text` returns `'static` (never freed by the caller); no raw
  allocation is handed across for the caller to free.
- The WT token-mint path (`iax_6b58`) takes **no** secret parameter — it mints
  from the station's already-held `PortalCredentials` and discards the token.

## Consumer (astar) impact

No function signatures changed. The one consumer-facing item is **F2**: the
`IaxSerial*` handle must be driven from a single thread (see the astar follow-up
ticket). The new APIs landed this session — `monitor_start/stop`, `mic_spectrum`,
`characterize`, `set_mic_profile`, `mint_token`, and the `input_db` snapshot
field — are tracked by their existing consumer tickets (astar-ee09, astar-2fde,
astar-e9ff).
