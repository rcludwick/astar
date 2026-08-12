// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! CM108/CM119 HID backend — compile-only stub (iax-53da). It exists to
//! prove [`crate::PttBackend`] fits a second real device shape:
//!
//! - Operator key: the COS bit in the HID input report (the line the UCI150's
//!   "`MicPTT` Dest" switch routes in its COS position). `read_key` would read
//!   the latest input report and extract that bit.
//! - Radio key: GPIO3 via a HID output report — the standard CM108 PTT mod.
//!
//! A real implementation needs a HID dependency (e.g. hidapi) — not approved
//! yet. Constructing this backend returns [`PttError::Unsupported`].

use crate::{PttBackend, PttError};

/// CM108-class HID GPIO backend (stub).
pub struct Cm108Hid {
    _private: (),
}

impl Cm108Hid {
    /// Stub constructor: always `Err(PttError::Unsupported)` until a HID
    /// dependency is approved and the report I/O is implemented.
    pub fn open() -> Result<Self, PttError> {
        Err(PttError::Unsupported(
            "CM108 HID backend not implemented (needs a HID dependency)",
        ))
    }
}

impl PttBackend for Cm108Hid {
    fn read_key(&mut self) -> Result<bool, PttError> {
        Err(PttError::Unsupported("CM108 HID"))
    }
    fn set_radio_key(&mut self, _level: bool) -> Result<(), PttError> {
        Err(PttError::Unsupported("CM108 HID"))
    }
    fn fail_safe(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_constructor_reports_unsupported() {
        assert!(matches!(Cm108Hid::open(), Err(PttError::Unsupported(_))));
    }
}
