// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The backend trait: raw line I/O only. Debounce, polarity, and edge logic
//! live in [`crate::PttBridge`] — backends just read/write hardware lines.

use std::fmt;

/// Errors from a PTT backend.
#[derive(Debug)]
pub enum PttError {
    /// Underlying transport failure (serial, HID).
    Io(std::io::Error),
    /// The backend is a stub / unsupported on this platform.
    Unsupported(&'static str),
}

impl fmt::Display for PttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ptt backend I/O: {e}"),
            Self::Unsupported(what) => write!(f, "ptt backend unsupported: {what}"),
        }
    }
}

impl std::error::Error for PttError {}

/// One hardware (or virtual) PTT interface: an operator-key input line and an
/// optional radio-key output line. RAW levels only — no polarity/debounce.
pub trait PttBackend: Send {
    /// Raw state of the operator key line (true = asserted).
    fn read_key(&mut self) -> Result<bool, PttError>;
    /// Drive the radio-key output line (raw level).
    fn set_radio_key(&mut self, level: bool) -> Result<(), PttError>;
    /// Best-effort radio unkey; called on ANY exit (drop, stop, panic).
    fn fail_safe(&mut self);
}
