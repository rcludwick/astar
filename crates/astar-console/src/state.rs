// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The state every front-end renders (iax-dd42). Single source of truth — the
//! console drains call events into it; front-ends only read it. `serde` derives
//! are feature-gated so the library stays lean for non-serializing consumers.

pub use astar_iax::CallSnapshot;
pub use astar_iax_core::VoiceFormat;

/// High-level call lifecycle, derived from the client's `CallEvent` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "kind", rename_all = "lowercase"))]
pub enum CallStatus {
    /// No call in progress.
    Idle,
    /// NEW sent, awaiting answer.
    Dialing,
    /// Peer answered; media flowing.
    Answered,
    /// Peer hung up (normal or with a cause).
    Hangup { reason: String },
    /// The call failed to establish.
    Failed { reason: String },
}

/// Top-level operating mode of a station: dial-out `WebTransceiver` (`Wt`) or
/// inbound IAX2 node (`Node`). Default `Wt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum OperatingMode {
    #[default]
    Wt,
    Node,
}

/// A snapshot of live call state for front-ends to render.
// A plain poll/render DTO, not a control-flow state machine — the bools are
// independently-meaningful UI flags (ptt/remote_ptt/m17_available/m17_active/
// dstar_available/dstar_active),
// not a hidden enum in disguise.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ConsoleState {
    pub status: CallStatus,
    /// Smoothed round-trip estimate in milliseconds, if known.
    pub rtt_ms: Option<u32>,
    /// TX (transmitted, post-DSP) level in dBFS, `-60.0..=0.0`. Meters only
    /// while keyed; floors to -60 when unkeyed.
    pub tx_level_db: f32,
    /// RX (decoded node audio) level in dBFS, `-60.0..=0.0`.
    pub rx_level_db: f32,
    /// Continuous mic INPUT level in dBFS, `-60.0..=0.0`, metered even while
    /// unkeyed (post-gain, pre-noise-reduction) so a consumer's VOX can key from
    /// silence (iax-5c30).
    pub input_level_db: f32,
    /// Local transmit state (`true` = keyed; render the PTT light red).
    pub ptt: bool,
    /// Remote-keyed state from `RemotePtt` events (for a future RX indicator).
    pub remote_ptt: bool,
    /// Top-level operating mode (Wt dial-out vs inbound Node).
    pub mode: OperatingMode,
    /// Cumulative voice-ts-ladder re-anchors (>80 ms TX-clock drift events;
    /// iax-5530/iax-9e55). A growing value signals choppy TX. `0` when idle.
    /// A plain `u64` health counter, credential-free.
    pub tx_reanchors: u64,
    /// Cumulative cpal capture overruns (dropped input buffers — holes in the
    /// captured mic PCM; iax-9e55) on the active call's routed mic. The lead
    /// suspect for choppy TX. `0` when idle / monitor-only. A plain `u64` health
    /// counter, credential-free.
    pub tx_capture_overruns: u64,
    /// Negotiated voice codec of the active call, once known (`None` while
    /// idle or still negotiating) — mirrors the primary call's
    /// `CallSnapshot::negotiated_format` (iax-3e53) so a UI can show
    /// "wideband" when slin16 is live. A plain codec id, credential-free.
    pub negotiated_format: Option<VoiceFormat>,
    /// Full concurrent-call list from `Manager::snapshot().calls` (iax-a1fb P5).
    /// Secret-free: every `CallSnapshot` field is a plain health counter, a node
    /// id, or a device-name string — no credentials. Empty when the session has
    /// no Manager (idle before first connect). The flat fields above mirror the
    /// primary/selected call for single-handset back-compat.
    pub calls: Vec<CallSnapshot>,
    /// Monotonic count of calls that have reached the answered state — bumped
    /// once per newly-answered call (inbound adopt AND WT dial). A consumer
    /// derives an "answered" edge from a change in this value; unlike the
    /// single `status`, it re-fires for every caller in a multi-call node
    /// (iax-a82f: the join greeting used to fire on the first caller only).
    pub answered_seq: u64,
    /// Digits already sent of the active `send_dtmf_string` sequence
    /// (iax-4b7a); `0` when no sequence is playing. Populated by the Station
    /// layer (which owns the queue) — the console itself always reports 0.
    /// A plain progress counter, credential-free.
    pub dtmf_played: u32,
    /// Total digits of the active `send_dtmf_string` sequence; `0` when no
    /// sequence is playing. Station-populated, like `dtmf_played`.
    pub dtmf_total: u32,
    /// `true` when M17 voice is available: the `m17` feature is compiled in
    /// AND a working Codec 2 backend was found (iax-f2b8 Task 4). A
    /// cheap-cached mirror of [`crate::m17_available`] populated every
    /// snapshot so a UI can gate its M17 connect affordance without an extra
    /// call. Always `false` when the `m17` feature isn't compiled in.
    pub m17_available: bool,
    /// `true` while an [`crate::session::ConsoleSession`]'s M17 session is
    /// live (mutually exclusive with an active IAX2 call — see
    /// `ConsoleSession::m17_connect`). Always `false` when the `m17` feature
    /// isn't compiled in.
    pub m17_active: bool,
    /// `true` when D-Star voice is available: the `dstar` feature is compiled
    /// in AND a `ThumbDV` is currently attached (iax-4c8e). Unlike
    /// [`Self::m17_available`], this tracks HOTPLUG — D-Star has no software
    /// vocoder, so the dongle being present is the whole of its availability.
    /// A cheap-cached mirror of [`crate::dstar_available`] (500 ms TTL)
    /// populated every snapshot, so a UI can grey out its D-Star affordance
    /// and un-grey it within half a second of the dongle appearing, without
    /// an extra call. Always `false` when the `dstar` feature isn't compiled
    /// in.
    pub dstar_available: bool,
    /// `true` while an [`crate::session::ConsoleSession`]'s D-Star session is
    /// live (mutually exclusive with both an active IAX2 call and an M17
    /// session — see `ConsoleSession::dstar_can_connect`). Always `false`
    /// when the `dstar` feature isn't compiled in.
    ///
    /// This field is feature-INDEPENDENT by design: `astar-server` reads it
    /// to refuse remote keying of a D-Star session without needing to compile
    /// against the `dstar` feature at all (see that crate's `NodeCommand::Key`
    /// handler and iax-d9f4).
    pub dstar_active: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            status: CallStatus::Idle,
            rtt_ms: None,
            tx_level_db: -60.0,
            rx_level_db: -60.0,
            input_level_db: -60.0,
            ptt: false,
            remote_ptt: false,
            mode: OperatingMode::default(),
            tx_reanchors: 0,
            tx_capture_overruns: 0,
            negotiated_format: None,
            calls: Vec::new(),
            answered_seq: 0,
            dtmf_played: 0,
            dtmf_total: 0,
            m17_available: false,
            m17_active: false,
            dstar_available: false,
            dstar_active: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle_and_silent() {
        let s = ConsoleState::default();
        assert_eq!(s.status, CallStatus::Idle);
        assert_eq!(s.rtt_ms, None);
        assert!((s.tx_level_db + 60.0).abs() < 1e-6);
        assert!((s.rx_level_db + 60.0).abs() < 1e-6);
        assert!((s.input_level_db + 60.0).abs() < 1e-6);
        assert!(!s.ptt);
        assert!(!s.remote_ptt);
        assert_eq!(s.mode, OperatingMode::Wt);
        // TX health counters start at zero (iax-9e55).
        assert_eq!(s.tx_reanchors, 0);
        assert_eq!(s.tx_capture_overruns, 0);
        // No call → no negotiated codec (iax-3e53).
        assert_eq!(s.negotiated_format, None);
        // iax-f2b8 Task 4: byte-identical default (no M17 session ever built).
        assert!(!s.m17_available);
        assert!(!s.m17_active);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serializes_with_status_tag() {
        let s = ConsoleState {
            status: CallStatus::Answered,
            rtt_ms: Some(41),
            ..ConsoleState::default()
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            json.contains("\"kind\":\"answered\""),
            "tagged status: {json}"
        );
        assert!(json.contains("\"rtt_ms\":41"));
    }
}
