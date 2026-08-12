// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The UI-facing snapshot: a small, render-ready projection of station state.
//!
//! The real core ([`astar_station::Station::snapshot`]) returns a rich
//! `ConsoleState` (dBFS meters, RTT, health counters, a full call list). The
//! GUI does not need all of that to draw the main screen — it needs a connection
//! status, who we dialed, whether audio is flowing each way, and two VU levels.
//! [`Snapshot`] is that reduced view; [`crate::conn::Conn::snapshot`] is the
//! seam that produces one (from a demo script or from the live `Station`).

/// Coarse connection status, rendered as the headline state of the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No call in progress.
    Disconnected,
    /// A connect is in flight (dialing / awaiting answer).
    Connecting,
    /// A call is up and media is flowing.
    Connected,
}

impl Status {
    /// Human-readable label for the status row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Status::Disconnected => "Disconnected",
            Status::Connecting => "Connecting…",
            Status::Connected => "Connected",
        }
    }
}

/// A render-ready snapshot of the station for one frame of the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Coarse connection status.
    pub status: Status,
    /// The node we dialed (e.g. `"546054"`), if any.
    pub dialed_node: Option<String>,
    /// A friendly name for the connected node, if known.
    pub node_name: Option<String>,
    /// Local transmit state (PTT keyed): light the TX indicator.
    pub transmitting: bool,
    /// Remote audio arriving: light the RX indicator.
    pub receiving: bool,
    /// Transmit VU level, `0.0..=1.0` (already normalized from dBFS).
    pub tx_level: f32,
    /// Receive VU level, `0.0..=1.0`.
    pub rx_level: f32,
    /// Why the last action failed (connect refused, call dropped, …), shown
    /// on the status card. `None` = all good.
    pub error: Option<String>,
    /// True when no account/credentials are configured — the status card
    /// shows the "add your AllStarLink account" hint (mirrors the Mac).
    pub needs_account: bool,
    /// Negotiated voice codec of the active call, or `None` while idle or
    /// still negotiating (astar-eb6c / astar-efba). The status card names it
    /// via [`Snapshot::codec_badge`] whenever set — green-tinted when
    /// `Slin16` (wideband) is live (astar-ef35).
    pub negotiated_format: Option<astar_station::VoiceFormat>,
    /// Digits already sent of the active `send_dtmf_string` sequence
    /// (astar-7d21 / iax-4b7a); `0` when no sequence is playing. The dialpad
    /// dims the played prefix from this.
    pub dtmf_played: u32,
    /// Total digits of the active sequence; `0` when none — the falling edge
    /// back to 0 is the dialpad's "sequence finished" signal.
    pub dtmf_total: u32,
}

impl Snapshot {
    /// The status-card tag naming the negotiated codec, or `None` while there
    /// isn't one (astar-ef35: every call shows its codec, not just wideband —
    /// mirrors the Mac's `VoiceFormat.badge`).
    #[must_use]
    pub fn codec_badge(&self) -> Option<&'static str> {
        use astar_station::VoiceFormat;
        // The engine's codec policies only ever negotiate these four
        // (CodecPolicy, iax-4348); the wildcard covers the rest of the IAX2
        // format bits so an unexpected peer still gets an honest tag.
        self.negotiated_format.map(|f| match f {
            VoiceFormat::G711U => "G.711 µ",
            VoiceFormat::G711A => "G.711 A",
            VoiceFormat::Slin => "slin8",
            VoiceFormat::Slin16 => "slin16",
            _ => "other codec",
        })
    }

    /// Whether the negotiated codec is 16 kHz wideband — the tag's green tint
    /// (narrowband tags stay muted; mirrors the Mac's `VoiceFormat.isWideband`).
    #[must_use]
    pub fn codec_is_wideband(&self) -> bool {
        self.negotiated_format == Some(astar_station::VoiceFormat::Slin16)
    }

    /// The long codec name for the tag's tooltip (matches the Mac's
    /// `VoiceFormat.description`).
    #[must_use]
    pub fn codec_description(&self) -> Option<&'static str> {
        use astar_station::VoiceFormat;
        self.negotiated_format.map(|f| match f {
            VoiceFormat::G711U => "µ-law (8 kHz)",
            VoiceFormat::G711A => "A-law (8 kHz)",
            VoiceFormat::Slin => "slin (8 kHz)",
            VoiceFormat::Slin16 => "slin16 (16 kHz wideband)",
            _ => "an unexpected codec",
        })
    }

    /// Nominal bitrate of the negotiated codec's audio, for the tag's tooltip
    /// (astar-bitrate: expose the bit rate alongside the negotiated codec;
    /// matches the Mac's `VoiceFormat.bitrateLabel`).
    #[must_use]
    pub fn codec_bitrate(&self) -> Option<&'static str> {
        use astar_station::VoiceFormat;
        self.negotiated_format.map(|f| match f {
            VoiceFormat::G711U | VoiceFormat::G711A => "64 kbit/s",
            VoiceFormat::Slin => "128 kbit/s",
            VoiceFormat::Slin16 => "256 kbit/s",
            _ => "an unknown bit rate",
        })
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Disconnected,
            dialed_node: None,
            node_name: None,
            transmitting: false,
            receiving: false,
            tx_level: 0.0,
            rx_level: 0.0,
            error: None,
            needs_account: false,
            negotiated_format: None,
            dtmf_played: 0,
            dtmf_total: 0,
        }
    }
}

/// Map a dBFS meter level in `-60.0..=0.0` to a normalized `0.0..=1.0` bar
/// height. Shared by the live mapping in [`crate::conn::RealConn`] and exposed
/// for tests; `-60` (the silence floor) → `0.0`, `0` dBFS → `1.0`.
#[must_use]
pub fn db_to_unit(db: f32) -> f32 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_floor_is_zero_ceiling_is_one() {
        assert_eq!(db_to_unit(-60.0), 0.0);
        assert_eq!(db_to_unit(0.0), 1.0);
        assert_eq!(db_to_unit(-30.0), 0.5);
        // Out-of-range is clamped, not wrapped.
        assert_eq!(db_to_unit(-100.0), 0.0);
        assert_eq!(db_to_unit(10.0), 1.0);
    }

    #[test]
    fn default_is_disconnected_and_silent() {
        let s = Snapshot::default();
        assert_eq!(s.status, Status::Disconnected);
        assert!(!s.transmitting && !s.receiving);
        assert_eq!(s.tx_level, 0.0);
        assert_eq!(s.rx_level, 0.0);
    }

    #[test]
    fn negotiated_format_defaults_to_none() {
        // Existing fixtures don't set it; idle carries none.
        assert_eq!(Snapshot::default().negotiated_format, None);
    }

    #[test]
    fn every_codec_names_its_badge() {
        use astar_station::VoiceFormat;
        let with = |f: Option<VoiceFormat>| Snapshot {
            negotiated_format: f,
            ..Snapshot::default()
        };
        // Every negotiated codec is named (astar-ef35, matches the Mac's
        // VoiceFormat.badge); no codec yet = no tag.
        assert_eq!(
            with(Some(VoiceFormat::G711U)).codec_badge(),
            Some("G.711 µ")
        );
        assert_eq!(
            with(Some(VoiceFormat::G711A)).codec_badge(),
            Some("G.711 A")
        );
        assert_eq!(with(Some(VoiceFormat::Slin)).codec_badge(), Some("slin8"));
        assert_eq!(
            with(Some(VoiceFormat::Slin16)).codec_badge(),
            Some("slin16")
        );
        assert_eq!(with(None).codec_badge(), None);
    }

    #[test]
    fn only_slin16_is_wideband() {
        use astar_station::VoiceFormat;
        let with = |f: Option<VoiceFormat>| Snapshot {
            negotiated_format: f,
            ..Snapshot::default()
        };
        assert!(with(Some(VoiceFormat::Slin16)).codec_is_wideband());
        assert!(!with(Some(VoiceFormat::G711U)).codec_is_wideband());
        assert!(!with(Some(VoiceFormat::G711A)).codec_is_wideband());
        assert!(!with(Some(VoiceFormat::Slin)).codec_is_wideband());
        assert!(!with(None).codec_is_wideband());
    }

    #[test]
    fn codec_description_feeds_the_tooltip() {
        use astar_station::VoiceFormat;
        let with = |f: Option<VoiceFormat>| Snapshot {
            negotiated_format: f,
            ..Snapshot::default()
        };
        assert_eq!(
            with(Some(VoiceFormat::G711U)).codec_description(),
            Some("µ-law (8 kHz)")
        );
        assert_eq!(
            with(Some(VoiceFormat::Slin16)).codec_description(),
            Some("slin16 (16 kHz wideband)")
        );
        assert_eq!(with(None).codec_description(), None);
    }

    #[test]
    fn codec_bitrate_feeds_the_tooltip() {
        use astar_station::VoiceFormat;
        let with = |f: Option<VoiceFormat>| Snapshot {
            negotiated_format: f,
            ..Snapshot::default()
        };
        assert_eq!(
            with(Some(VoiceFormat::G711U)).codec_bitrate(),
            Some("64 kbit/s")
        );
        assert_eq!(
            with(Some(VoiceFormat::G711A)).codec_bitrate(),
            Some("64 kbit/s")
        );
        assert_eq!(
            with(Some(VoiceFormat::Slin)).codec_bitrate(),
            Some("128 kbit/s")
        );
        assert_eq!(
            with(Some(VoiceFormat::Slin16)).codec_bitrate(),
            Some("256 kbit/s")
        );
        assert_eq!(with(None).codec_bitrate(), None);
    }

    #[test]
    fn status_labels_are_distinct() {
        assert_ne!(Status::Disconnected.label(), Status::Connecting.label());
        assert_ne!(Status::Connecting.label(), Status::Connected.label());
    }
}
