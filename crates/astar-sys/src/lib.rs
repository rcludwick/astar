// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-sys` — a flat, opaque-handle **C-ABI** over the
//! [`astar_station::Station`] facade.
//!
//! # Contract
//!
//! - **Opaque handle.** A station is an `IaxStation*` (a `Box<Station>` behind a
//!   raw pointer). The only cross-boundary heap object. Create with
//!   [`iax_station_new`], free with [`iax_station_free`]. Using a handle after
//!   free, or freeing twice, is undefined behaviour (the same rule as C).
//!
//! - **Poll + snapshot, no callbacks.** The ABI never calls back into the
//!   caller. The app drives its own loop: [`iax_station_snapshot`] for live
//!   meters/status, [`iax_station_next_event`] for discrete lifecycle edges.
//!   This keeps the Python/Swift bindings sound (no Rust closures invoked from
//!   a managed runtime).
//!
//! - **Caller-allocated out-structs.** Every "get" fills a caller-provided
//!   `#[repr(C)]` struct or buffer; the library allocates nothing the caller
//!   must free except the opaque handle itself. Strings are written into
//!   caller buffers (truncate-safe, NUL-terminated) or are `'static` and never
//!   freed by C (see [`iax_error_text`]).
//!
//! - **Integer error codes.** `0` ([`IAX_OK`]) is success; negative values are
//!   errors (see the `IAX_ERR_*` constants). [`iax_error_text`] maps a code to a
//!   `'static` human-readable string.
//!
//! - **`catch_unwind` at every boundary.** A Rust panic must never unwind into
//!   C (that is UB). Every `extern "C"` function wraps its body in
//!   `catch_unwind`; a caught panic becomes [`IAX_ERR_PANIC`] (or a null handle
//!   for constructors).
//!
//! - **Null-guarded.** Every handle and out-pointer argument is null-checked
//!   before use; a null returns [`IAX_ERR_NULL`] (or a null handle).
//!
//! - **Secret-free.** The `secret` (guest secret or minted WT token) is a
//!   `const char*` **in-param** to connect, consumed immediately into the
//!   session. It never appears in any out-struct, snapshot, event, status text,
//!   or error text, and the generated header carries no secret field.
//!
//! - **Vendor-neutral.** [`iax_station_connect`] is the generic IAX2 path (works
//!   against stock Asterisk / the local parrot). [`iax_station_connect_wt`] is
//!   the one `AllStar` Web-Transceiver convenience. No `AllStar` policy is baked
//!   into the ABI surface.

mod ffi;

pub use ffi::*;
