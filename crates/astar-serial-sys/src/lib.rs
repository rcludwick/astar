// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-serial-sys` — a poll C-ABI over the IAX2-free `astar-ptt`
//! crate: a cross-platform serial radio-interface client.
//!
//! v1 ships the PTT facet (control-line keying via the poll/tick model). The
//! open port is a first-class `IaxSerial*` handle; a data facet (TXD/RXD byte
//! read/write) is reserved for v2 on the same handle.
//!
//! Contract mirrors `astar-sys`: opaque handle, poll + no callbacks,
//! caller passes config in, integer error codes, secret-free.

mod ffi;

pub use ffi::*;
