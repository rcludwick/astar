// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Vocoder trust: which AMBE vocoder sticks this machine may open, and the
//! enrolment that adds one (on-air safety).
//!
//! Not behind `ambe-hw`: station config, the C ABI and every front-end carry
//! a [`VocoderTrust`] whether or not this build can open hardware. Without
//! `ambe-hw`, listing returns nothing and enrolment returns
//! [`VocoderEnrolment::NotCompiled`]. Re-exported from `crate::ambe`.

/// The environment variable CLI tools read their vocoder trust list from:
/// comma-separated USB serial numbers, e.g. `ASTAR_VOCODER_SERIALS=A1B2,C3D4`.
pub const VOCODER_SERIALS_ENV: &str = "ASTAR_VOCODER_SERIALS";

/// What a front-end shows when a hardware-vocoder connect is refused because
/// nothing is trusted.
pub const NO_TRUSTED_VOCODER: &str = "No trusted vocoder — add one in Settings → Vocoders";

/// The USB serial numbers of the vocoder sticks this machine may open.
///
/// # ON-AIR SAFETY
///
/// FTDI's `0403:6015` covers `DVstick`s, `ThumbDV`s and a great many CAT/PTT
/// cables alike, and opening a tty raises DTR/RTS. So **the engine opens a
/// vocoder port only when its USB serial number is in this list.** An empty
/// list means no hardware vocoder at all: every opener returns `None` without
/// enumerating, let alone opening, anything. `IAX_THUMBDV_PORT` narrows within
/// the list and can never widen it.
///
/// There is no global or ambient trust: every opener takes one of these
/// explicitly. Serials get onto the list only through enrolment
/// ([`enrol_vocoder`]), which is always a user action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VocoderTrust {
    /// Trusted USB serial numbers, trimmed, non-empty, de-duplicated.
    pub serials: Vec<String>,
}

impl VocoderTrust {
    /// Trust exactly `serials` (trimmed; empty entries and repeats dropped).
    pub fn new<I, S>(serials: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out: Vec<String> = Vec::new();
        for s in serials {
            let s = s.as_ref().trim();
            if !s.is_empty() && !out.iter().any(|o| o == s) {
                out.push(s.to_string());
            }
        }
        VocoderTrust { serials: out }
    }

    /// Trust nothing: no hardware vocoder will be opened.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Parse a list separated by commas, whitespace or newlines.
    #[must_use]
    pub fn parse_list(list: &str) -> Self {
        Self::new(list.split(|c: char| c == ',' || c.is_whitespace()))
    }

    /// The trust list from [`VOCODER_SERIALS_ENV`]; empty when it is unset.
    /// For CLI tools only — the app and the node pass their configured list.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var(VOCODER_SERIALS_ENV)
            .map(|v| Self::parse_list(&v))
            .unwrap_or_default()
    }

    /// `true` when nothing is trusted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.serials.is_empty()
    }

    /// `true` when `serial` (trimmed) is trusted.
    #[must_use]
    pub fn contains(&self, serial: &str) -> bool {
        let serial = serial.trim();
        self.serials.iter().any(|s| s == serial)
    }
}

/// An FT-X-class port that could be enrolled: its USB serial number and the
/// path it enumerates as. Listing never opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VocoderCandidate {
    /// USB serial number — what trust is recorded against.
    pub serial: String,
    /// OS port path, for display.
    pub port: String,
}

/// Every FT-X-class port with a USB serial number, for an "Add vocoder…"
/// picker. **Enumeration only: no port is opened.** Empty without the
/// `ambe-hw` feature.
#[must_use]
pub fn list_vocoder_candidates() -> Vec<VocoderCandidate> {
    #[cfg(feature = "ambe-hw")]
    {
        ambe_thumbdv::SerialTransport::list_candidates()
            .into_iter()
            .map(|c| VocoderCandidate {
                serial: c.serial,
                port: c.port,
            })
            .collect()
    }
    #[cfg(not(feature = "ambe-hw"))]
    {
        Vec::new()
    }
}

/// Model label recorded for an enrolled `AMBE3000*` stick.
pub const MODEL_AMBE3000: &str = "DVstick 30 / ThumbDV-class";
/// Model label for an `AMBE3003*` stick, recognised but not supported yet.
pub const MODEL_AMBE3003: &str = "DVstick 33 / AMBE-3003";

/// The outcome of [`enrol_vocoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VocoderEnrolment {
    /// An `AMBE3000*` chip answered: add `serial` to the trust list, and
    /// record `prodid` and `model` alongside it.
    Trusted {
        /// USB serial number to trust.
        serial: String,
        /// The port it answered on.
        port: String,
        /// PRODID as reported (e.g. `AMBE3000R`).
        prodid: String,
        /// [`MODEL_AMBE3000`].
        model: String,
    },
    /// An `AMBE3003*` chip (`DVstick` 33) answered. Recognised, not supported
    /// yet, so it is **not** to be trusted.
    Unsupported {
        /// USB serial number.
        serial: String,
        /// The port it answered on.
        port: String,
        /// PRODID as reported.
        prodid: String,
        /// [`MODEL_AMBE3003`].
        model: String,
    },
    /// Something answered the AMBE protocol with a PRODID that is neither.
    WrongDevice {
        /// USB serial number.
        serial: String,
        /// The port.
        port: String,
        /// PRODID as reported.
        prodid: String,
    },
    /// The port opened but nothing answered as an AMBE chip (timeout or
    /// garbage). It may be a CAT cable. Refused.
    NoAnswer {
        /// USB serial number.
        serial: String,
        /// The port.
        port: String,
    },
    /// The port would not open (busy, gone, permission), or its DTR/RTS
    /// could not be driven low. Refused; nothing was written.
    OpenFailed {
        /// USB serial number.
        serial: String,
        /// The port.
        port: String,
        /// The OS error, for display.
        reason: String,
    },
    /// No FT-X-class port with that serial number is attached.
    NotFound {
        /// The serial asked for.
        serial: String,
    },
    /// This build has no hardware vocoder support (`ambe-hw` off).
    NotCompiled,
}

impl VocoderEnrolment {
    /// `true` only for [`VocoderEnrolment::Trusted`].
    #[must_use]
    pub fn is_trusted(&self) -> bool {
        matches!(self, VocoderEnrolment::Trusted { .. })
    }

    /// The serial this outcome is about (`None` only for `NotCompiled`).
    #[must_use]
    pub fn serial(&self) -> Option<&str> {
        match self {
            VocoderEnrolment::Trusted { serial, .. }
            | VocoderEnrolment::Unsupported { serial, .. }
            | VocoderEnrolment::WrongDevice { serial, .. }
            | VocoderEnrolment::NoAnswer { serial, .. }
            | VocoderEnrolment::OpenFailed { serial, .. }
            | VocoderEnrolment::NotFound { serial } => Some(serial),
            VocoderEnrolment::NotCompiled => None,
        }
    }

    /// The port that was probed, when one was.
    #[must_use]
    pub fn port(&self) -> Option<&str> {
        match self {
            VocoderEnrolment::Trusted { port, .. }
            | VocoderEnrolment::Unsupported { port, .. }
            | VocoderEnrolment::WrongDevice { port, .. }
            | VocoderEnrolment::NoAnswer { port, .. }
            | VocoderEnrolment::OpenFailed { port, .. } => Some(port),
            VocoderEnrolment::NotFound { .. } | VocoderEnrolment::NotCompiled => None,
        }
    }

    /// The PRODID the chip reported, when it answered.
    #[must_use]
    pub fn prodid(&self) -> Option<&str> {
        match self {
            VocoderEnrolment::Trusted { prodid, .. }
            | VocoderEnrolment::Unsupported { prodid, .. }
            | VocoderEnrolment::WrongDevice { prodid, .. } => Some(prodid),
            _ => None,
        }
    }

    /// The model label, for `Trusted` and `Unsupported`.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        match self {
            VocoderEnrolment::Trusted { model, .. }
            | VocoderEnrolment::Unsupported { model, .. } => Some(model),
            _ => None,
        }
    }

    /// A stable lowercase tag for this outcome, for JSON across the C ABI.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            VocoderEnrolment::Trusted { .. } => "trusted",
            VocoderEnrolment::Unsupported { .. } => "unsupported",
            VocoderEnrolment::WrongDevice { .. } => "wrong_device",
            VocoderEnrolment::NoAnswer { .. } => "no_answer",
            VocoderEnrolment::OpenFailed { .. } => "open_failed",
            VocoderEnrolment::NotFound { .. } => "not_found",
            VocoderEnrolment::NotCompiled => "not_compiled",
        }
    }

    /// One operator-facing sentence saying what happened and why.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            VocoderEnrolment::Trusted { prodid, model, .. } => {
                format!("Added: {model} ({prodid}).")
            }
            VocoderEnrolment::Unsupported { prodid, model, .. } => format!(
                "This is a {model} ({prodid}). It is recognised but not supported yet, so it \
                 was not added."
            ),
            VocoderEnrolment::WrongDevice { prodid, .. } => format!(
                "This device answered as {prodid}, which is not an AMBE-3000 vocoder. It was \
                 not added."
            ),
            VocoderEnrolment::NoAnswer { port, .. } => format!(
                "Nothing at {port} answered as a vocoder. It may be a radio's CAT/PTT cable; it \
                 was not added."
            ),
            VocoderEnrolment::OpenFailed { port, reason, .. } => {
                format!("Could not open {port} ({reason}). It was not added.")
            }
            VocoderEnrolment::NotFound { serial } => {
                format!("No vocoder-class USB device with serial {serial} is attached.")
            }
            VocoderEnrolment::NotCompiled => {
                "This build has no hardware vocoder support.".to_string()
            }
        }
    }
}

/// Enrol the stick with USB serial `serial`: find its port among
/// [`list_vocoder_candidates`], open it, drive DTR/RTS low before writing
/// anything, send RESET + PRODID (each bounded at 300 ms, at 460800 then
/// 230400 baud), close it, and classify the reply.
///
/// **This is the only operation that opens a port that is not yet trusted.**
/// Call it only from an explicit user action, after the user has confirmed
/// the device is not a radio's CAT/PTT cable. It blocks for up to about two
/// seconds; call it off any UI thread. It never modifies a trust list: on
/// [`VocoderEnrolment::Trusted`] the caller records the serial.
#[must_use]
pub fn enrol_vocoder(serial: &str) -> VocoderEnrolment {
    #[cfg(feature = "ambe-hw")]
    {
        enrol_vocoder_with(
            serial,
            &list_vocoder_candidates(),
            ambe_thumbdv::SerialTransport::open,
        )
    }
    #[cfg(not(feature = "ambe-hw"))]
    {
        let _ = serial;
        VocoderEnrolment::NotCompiled
    }
}

/// [`enrol_vocoder`] with the candidate listing and the transport factory
/// injected — the hardware-free test seam.
#[cfg(feature = "ambe-hw")]
pub(crate) fn enrol_vocoder_with<T, F>(
    serial: &str,
    candidates: &[VocoderCandidate],
    open: F,
) -> VocoderEnrolment
where
    T: ambe_thumbdv::Transport,
    F: FnMut(&str, u32) -> std::io::Result<T>,
{
    use ambe_thumbdv::{ChipClass, ProbeOutcome};
    let serial = serial.trim().to_string();
    let Some(cand) = candidates.iter().find(|c| c.serial == serial) else {
        return VocoderEnrolment::NotFound { serial };
    };
    let port = cand.port.clone();
    match ambe_thumbdv::probe_with(&port, open) {
        ProbeOutcome::Answered {
            prodid,
            class: ChipClass::Ambe3000,
            ..
        } => VocoderEnrolment::Trusted {
            serial,
            port,
            prodid,
            model: MODEL_AMBE3000.to_string(),
        },
        ProbeOutcome::Answered {
            prodid,
            class: ChipClass::Ambe3003,
            ..
        } => VocoderEnrolment::Unsupported {
            serial,
            port,
            prodid,
            model: MODEL_AMBE3003.to_string(),
        },
        ProbeOutcome::Answered { prodid, .. } => VocoderEnrolment::WrongDevice {
            serial,
            port,
            prodid,
        },
        ProbeOutcome::NoAnswer => VocoderEnrolment::NoAnswer { serial, port },
        ProbeOutcome::OpenFailed(e) => VocoderEnrolment::OpenFailed {
            serial,
            port,
            reason: e.to_string(),
        },
    }
}

#[cfg(test)]
mod vocoder_trust_tests {
    use super::{VOCODER_SERIALS_ENV, VocoderTrust};

    #[test]
    fn trust_list_is_trimmed_and_deduplicated() {
        let t = VocoderTrust::new([" TESTSER1 ", "", "TESTSER2", "TESTSER1"]);
        assert_eq!(t.serials, vec!["TESTSER1", "TESTSER2"]);
        assert!(t.contains("TESTSER2"));
        assert!(t.contains(" TESTSER1"));
        assert!(!t.contains("CATCABLE"));
    }

    #[test]
    fn parse_list_accepts_commas_and_whitespace() {
        let t = VocoderTrust::parse_list("TESTSER1, TESTSER2\nTESTSER3,,");
        assert_eq!(t.serials, vec!["TESTSER1", "TESTSER2", "TESTSER3"]);
        assert!(VocoderTrust::parse_list(" , ").is_empty());
    }

    #[test]
    fn the_default_trusts_nothing() {
        assert!(VocoderTrust::default().is_empty());
        assert!(VocoderTrust::none().is_empty());
        assert_eq!(VOCODER_SERIALS_ENV, "ASTAR_VOCODER_SERIALS");
    }
}
