// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Codec-frame events a recorder consumes.

use std::time::{Duration, SystemTime};

/// A frame stream with no events for this long is closed by the writer.
/// Here rather than in the writer so producers built without the `writer`
/// feature can close their own overs ahead of it.
pub const FRAME_IDLE_CLOSE: Duration = Duration::from_secs(1);

/// Which way a stream travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Rx,
    Tx,
}

impl Dir {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Dir::Rx => "rx",
            Dir::Tx => "tx",
        }
    }
}

/// The frame family, named after unamblify's `captured/<mode>` directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Dstar,
    YsfDmr,
    Codec2_3200,
    Nxdn,
    Dmr72,
    /// `EchoLink` GSM. Nothing emits it on this branch (see `record-echolink-gsm`).
    Gsm,
}

impl CaptureMode {
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            CaptureMode::Dstar => "dstar",
            CaptureMode::YsfDmr => "ysf-dmr",
            CaptureMode::Codec2_3200 => "codec2-3200",
            CaptureMode::Nxdn => "nxdn",
            CaptureMode::Dmr72 => "dmr72",
            CaptureMode::Gsm => "gsm",
        }
    }

    #[must_use]
    pub fn frame_bytes(self) -> usize {
        match self {
            CaptureMode::Dstar | CaptureMode::Dmr72 => 9,
            CaptureMode::YsfDmr | CaptureMode::Nxdn => 7,
            CaptureMode::Codec2_3200 => 8,
            CaptureMode::Gsm => 33,
        }
    }

    #[must_use]
    pub fn frames_per_seq(self) -> u32 {
        if self == CaptureMode::Codec2_3200 {
            2
        } else {
            1
        }
    }

    #[must_use]
    pub fn prodid(self) -> &'static str {
        match self {
            CaptureMode::Codec2_3200 => "codec2",
            CaptureMode::Gsm => "gsm",
            _ => "AMBE3000",
        }
    }
}

/// A sequence number and the modulus it wraps at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seq {
    pub value: u16,
    pub modulus: u16,
}

/// One event in a codec-frame stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // events sit in a bounded queue; boxing the PCM block would allocate every tick
pub enum FrameEvent {
    Start {
        dir: Dir,
        network: &'static str,
        mode: CaptureMode,
        talker: Option<String>,
        at: SystemTime,
        vocoder: &'static str,
    },
    Frame {
        dir: Dir,
        bytes: Vec<u8>,
        seq: Option<Seq>,
    },
    Pcm {
        dir: Dir,
        pcm: [i16; 160],
    },
    End {
        dir: Dir,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_modes_match_unamblify() {
        let table = [
            (CaptureMode::Dstar, "dstar", 9, 1, "AMBE3000"),
            (CaptureMode::YsfDmr, "ysf-dmr", 7, 1, "AMBE3000"),
            (CaptureMode::Codec2_3200, "codec2-3200", 8, 2, "codec2"),
            (CaptureMode::Nxdn, "nxdn", 7, 1, "AMBE3000"),
            (CaptureMode::Dmr72, "dmr72", 9, 1, "AMBE3000"),
            (CaptureMode::Gsm, "gsm", 33, 1, "gsm"),
        ];
        for (m, dir, bytes, per, prod) in table {
            assert_eq!(m.dir_name(), dir);
            assert_eq!(m.frame_bytes(), bytes);
            assert_eq!(m.frames_per_seq(), per);
            assert_eq!(m.prodid(), prod);
        }
        assert_eq!(Dir::Rx.as_str(), "rx");
        assert_eq!(Dir::Tx.as_str(), "tx");
    }
}
