// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The SSTV modes astar names, independent of the decoder's own types.

use serde::{Serialize, Serializer};

/// An SSTV transmission mode, as the VIS header announced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Pd120,
    Pd180,
    Pd240,
    Robot24,
    Robot36,
    Robot72,
    Robot24Bw,
    Robot12Bw,
    Robot8Bw,
    Scottie1,
    Scottie2,
    ScottieDx,
    Martin1,
    Martin2,
    Wraase120,
    Wraase180,
    /// A mode a newer decoder knows and this build does not name.
    Other,
}

impl Mode {
    /// The name people know it by, as shown in the app: `"Robot 36"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pd120 => "PD120",
            Self::Pd180 => "PD180",
            Self::Pd240 => "PD240",
            Self::Robot24 => "Robot 24",
            Self::Robot36 => "Robot 36",
            Self::Robot72 => "Robot 72",
            Self::Robot24Bw => "Robot 24 B/W",
            Self::Robot12Bw => "Robot 12 B/W",
            Self::Robot8Bw => "Robot 8 B/W",
            Self::Scottie1 => "Scottie 1",
            Self::Scottie2 => "Scottie 2",
            Self::ScottieDx => "Scottie DX",
            Self::Martin1 => "Martin 1",
            Self::Martin2 => "Martin 2",
            Self::Wraase120 => "Wraase 120",
            Self::Wraase180 => "Wraase 180",
            Self::Other => "SSTV",
        }
    }

    /// The name with no spaces or slashes, for file names: `"Robot36"`.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pd120 => "PD120",
            Self::Pd180 => "PD180",
            Self::Pd240 => "PD240",
            Self::Robot24 => "Robot24",
            Self::Robot36 => "Robot36",
            Self::Robot72 => "Robot72",
            Self::Robot24Bw => "Robot24BW",
            Self::Robot12Bw => "Robot12BW",
            Self::Robot8Bw => "Robot8BW",
            Self::Scottie1 => "Scottie1",
            Self::Scottie2 => "Scottie2",
            Self::ScottieDx => "ScottieDX",
            Self::Martin1 => "Martin1",
            Self::Martin2 => "Martin2",
            Self::Wraase120 => "Wraase120",
            Self::Wraase180 => "Wraase180",
            Self::Other => "SSTV",
        }
    }

    pub(crate) fn from_slowrx(mode: slowrx::SstvMode) -> Self {
        use slowrx::SstvMode as S;
        match mode {
            S::Pd120 => Self::Pd120,
            S::Pd180 => Self::Pd180,
            S::Pd240 => Self::Pd240,
            S::Robot24 => Self::Robot24,
            S::Robot36 => Self::Robot36,
            S::Robot72 => Self::Robot72,
            S::Robot24Bw => Self::Robot24Bw,
            S::Robot12Bw => Self::Robot12Bw,
            S::Robot8Bw => Self::Robot8Bw,
            S::Scottie1 => Self::Scottie1,
            S::Scottie2 => Self::Scottie2,
            S::ScottieDx => Self::ScottieDx,
            S::Martin1 => Self::Martin1,
            S::Martin2 => Self::Martin2,
            S::Wraase120 => Self::Wraase120,
            S::Wraase180 => Self::Wraase180,
            _ => Self::Other,
        }
    }
}

impl Serialize for Mode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    const ALL: [Mode; 17] = [
        Mode::Pd120,
        Mode::Pd180,
        Mode::Pd240,
        Mode::Robot24,
        Mode::Robot36,
        Mode::Robot72,
        Mode::Robot24Bw,
        Mode::Robot12Bw,
        Mode::Robot8Bw,
        Mode::Scottie1,
        Mode::Scottie2,
        Mode::ScottieDx,
        Mode::Martin1,
        Mode::Martin2,
        Mode::Wraase120,
        Mode::Wraase180,
        Mode::Other,
    ];

    #[test]
    fn tokens_and_names_match_r7() {
        assert_eq!(Mode::Pd120.token(), "PD120");
        assert_eq!(Mode::Robot36.token(), "Robot36");
        assert_eq!(Mode::Martin1.token(), "Martin1");
        assert_eq!(Mode::ScottieDx.token(), "ScottieDX");
        assert_eq!(Mode::Robot24Bw.token(), "Robot24BW");
        assert_eq!(Mode::Wraase180.token(), "Wraase180");
        assert_eq!(Mode::Other.token(), "SSTV");
        assert_eq!(Mode::Pd120.name(), "PD120");
        assert_eq!(Mode::Robot36.name(), "Robot 36");
        assert_eq!(Mode::ScottieDx.name(), "Scottie DX");
        assert_eq!(Mode::Robot24Bw.name(), "Robot 24 B/W");
        assert_eq!(Mode::Other.name(), "SSTV");
        // A token is always the display name without spaces or slashes.
        for m in ALL {
            assert_eq!(m.token(), m.name().replace([' ', '/'], ""), "{m:?}");
        }
        assert_eq!(
            serde_json::to_string(&Mode::Robot36).unwrap(),
            "\"Robot 36\""
        );
    }
}
