// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-server vocoder list` and `astar-server vocoder add <serial>`: the
//! node's side of vocoder trust.
//!
//! The engine opens an AMBE vocoder port only when its USB serial number is
//! in `node.toml` `[vocoder] trusted_serials`. A DVstick and a radio's FT-X
//! CAT/PTT cable enumerate identically, and opening a cable's tty raises
//! DTR/RTS, which keys the transmitter — so nothing here scans.
//!
//! * `list` enumerates the FT-X-class ports and says which are trusted.
//!   **Nothing is opened.**
//! * `add` is the one operation that opens a port before it is trusted. It
//!   runs only after the operator has confirmed the device is not a radio's
//!   cable, and it runs the engine's enrolment probe (DTR/RTS held low,
//!   RESET + PRODID only, bounded). The serial is written to `node.toml`
//!   only when an AMBE-3000 answered, through the settings page's own write
//!   path — the same patch, the same loader-as-oracle, the same atomic
//!   rename — so the CLI and the page can never disagree about the file.
//!
//! The probe and the listing are injected, so every test here runs against
//! made-up serials and opens nothing.

use std::fmt::Write as _;
use std::path::Path;

use astar_station::{VocoderCandidate, VocoderEnrolment, VocoderTrust};
use serde_json::Value;

use crate::config::{NodeFileConfig, check_vocoder_serial};
use crate::settings::{ACK_VOCODER_SERIALS, RedactedConfig, SettingsPatch, WriteContext, etag_of};
use crate::settings_store::{SaveError, SettingsStore};

/// What the operator is asked to confirm before `add` opens anything.
pub const CABLE_WARNING: &str = "Make sure this is not a radio's CAT/PTT cable. Enrolment opens \
     the port (DTR/RTS held low) and sends AMBE RESET + PRODID; on a cable wired to a radio \
     that can key the transmitter.";

/// The `list` report: every candidate port with its serial and whether it is
/// trusted, then any trusted serial that is not attached. Plain text, one
/// line each.
#[must_use]
pub fn list_report(candidates: &[VocoderCandidate], trust: &VocoderTrust) -> String {
    let mut out = String::new();
    if candidates.is_empty() {
        out.push_str(
            "No vocoder-class USB devices (FTDI FT-X with a serial number) are attached.\n",
        );
    } else {
        out.push_str("Vocoder-class USB devices (nothing was opened to list them):\n");
        for c in candidates {
            let mark = if trust.contains(&c.serial) {
                "trusted"
            } else {
                "not trusted"
            };
            let _ = writeln!(out, "  {:<16} {:<32} {mark}", c.serial, c.port);
        }
    }
    let absent: Vec<&String> = trust
        .serials
        .iter()
        .filter(|s| !candidates.iter().any(|c| &c.serial == *s))
        .collect();
    if !absent.is_empty() {
        out.push_str("Trusted but not attached:\n");
        for s in absent {
            let _ = writeln!(out, "  {s}");
        }
    }
    if trust.is_empty() {
        out.push_str(
            "No vocoder is trusted: this node opens none. Add one with \
             `astar-server vocoder add <serial> --config <node.toml>`.\n",
        );
    }
    out
}

/// What a successful `add` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    /// The probe answered as an AMBE-3000; the serial is now in `node.toml`.
    Added {
        /// The enrolment result, for its `message()`.
        enrolment: VocoderEnrolment,
    },
    /// The serial was already trusted. Nothing was opened or written.
    AlreadyTrusted,
}

/// `astar-server vocoder add <serial>`, with the probe injected.
///
/// The caller must already have the operator's confirmation
/// ([`CABLE_WARNING`]). Order matters and is the safety argument:
///
/// 1. the file is loaded (a node.toml that does not boot is refused before
///    anything is opened);
/// 2. the serial is checked by the loader's own rule;
/// 3. an already-trusted serial returns at once — no probe;
/// 4. only then is `enrol` called, and anything but `Trusted` is an error
///    carrying the engine's own sentence, with nothing written;
/// 5. the serial is appended through [`SettingsStore::save`].
///
/// # Errors
/// A sentence for the operator: the file, the serial, the probe's verdict, or
/// the write.
pub fn add(
    config_path: &Path,
    serial: &str,
    enrol: impl FnOnce(&str) -> VocoderEnrolment,
) -> Result<AddOutcome, String> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|e| format!("cannot read config {}: {e}", config_path.display()))?;
    let cfg = NodeFileConfig::from_toml_str_redacted(&text)?;
    check_vocoder_serial(serial)?;
    let mut serials: Vec<String> = cfg
        .vocoder
        .as_ref()
        .map(|v| v.trusted_serials.clone())
        .unwrap_or_default();
    if serials.iter().any(|s| s == serial) {
        return Ok(AddOutcome::AlreadyTrusted);
    }

    let enrolment = enrol(serial);
    if !enrolment.is_trusted() {
        return Err(enrolment.message());
    }

    serials.push(serial.to_string());
    let store = SettingsStore::new(
        config_path.to_path_buf(),
        cfg.credentials_path(config_path),
        RedactedConfig::from_config(&cfg),
    );
    let req = SettingsPatch {
        etag: etag_of(text.as_bytes()),
        patch: [("vocoder.trusted_serials".to_string(), Value::from(serials))]
            .into_iter()
            .collect(),
        // The operator confirmed before the probe, and the probe answered as
        // a vocoder: that is what the acknowledgement stands for.
        acknowledge: vec![ACK_VOCODER_SERIALS.to_string()],
    };
    match store.save(&req, &WriteContext::default()) {
        Ok(_) => Ok(AddOutcome::Added { enrolment }),
        Err(SaveError::Conflict) => Err(format!(
            "{} changed while the vocoder was being probed; run the command again",
            config_path.display()
        )),
        Err(SaveError::Field(f)) => Err(f.message),
        Err(SaveError::Oracle(m) | SaveError::Io(m)) => Err(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::template::NODE_TOML_TEMPLATE;

    fn trusted(serial: &str) -> VocoderEnrolment {
        VocoderEnrolment::Trusted {
            serial: serial.to_string(),
            port: "/dev/cu.usbserial-TESTSER1".to_string(),
            prodid: "AMBE3000R".to_string(),
            model: astar_station::MODEL_AMBE3000.to_string(),
        }
    }

    fn node_toml(extra: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(&path, format!("{NODE_TOML_TEMPLATE}{extra}")).unwrap();
        (dir, path)
    }

    fn trust_on_disk(path: &Path) -> VocoderTrust {
        NodeFileConfig::from_toml_str(&std::fs::read_to_string(path).unwrap())
            .unwrap()
            .vocoder_trust()
    }

    #[test]
    fn a_trusted_probe_writes_the_serial_into_node_toml() {
        let (_d, path) = node_toml("");
        let mut probed = Vec::new();
        let out = add(&path, "TESTSER1", |s| {
            probed.push(s.to_string());
            trusted(s)
        })
        .expect("an AMBE-3000 answer is added");
        assert!(matches!(out, AddOutcome::Added { .. }));
        assert_eq!(probed, vec!["TESTSER1"]);
        assert_eq!(trust_on_disk(&path).serials, vec!["TESTSER1"]);
        let text = std::fs::read_to_string(&path).unwrap();
        for line in NODE_TOML_TEMPLATE.lines() {
            assert!(
                text.contains(line),
                "the rest of the file is kept: {line:?}"
            );
        }
    }

    #[test]
    fn a_second_stick_is_appended_and_the_first_is_kept() {
        let (_d, path) = node_toml("\n[vocoder]\ntrusted_serials = [\"TESTSER1\"]\n");
        add(&path, "TESTSER2", trusted).unwrap();
        assert_eq!(trust_on_disk(&path).serials, vec!["TESTSER1", "TESTSER2"]);
    }

    #[test]
    fn an_already_trusted_serial_is_never_probed() {
        let (_d, path) = node_toml("\n[vocoder]\ntrusted_serials = [\"TESTSER1\"]\n");
        let before = std::fs::read_to_string(&path).unwrap();
        let out = add(&path, "TESTSER1", |_| panic!("must not probe")).unwrap();
        assert_eq!(out, AddOutcome::AlreadyTrusted);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn every_refusal_writes_nothing_and_says_why() {
        let refusals = [
            VocoderEnrolment::Unsupported {
                serial: "TESTSER3".into(),
                port: "/dev/cu.usbserial-TESTSER3".into(),
                prodid: "AMBE3003".into(),
                model: astar_station::MODEL_AMBE3003.into(),
            },
            VocoderEnrolment::WrongDevice {
                serial: "TESTSER3".into(),
                port: "/dev/cu.usbserial-TESTSER3".into(),
                prodid: "XYZ".into(),
            },
            VocoderEnrolment::NoAnswer {
                serial: "TESTSER3".into(),
                port: "/dev/cu.usbserial-TESTSER3".into(),
            },
            VocoderEnrolment::OpenFailed {
                serial: "TESTSER3".into(),
                port: "/dev/cu.usbserial-TESTSER3".into(),
                reason: "busy".into(),
            },
            VocoderEnrolment::NotFound {
                serial: "TESTSER3".into(),
            },
            VocoderEnrolment::NotCompiled,
        ];
        for r in refusals {
            let (_d, path) = node_toml("");
            let before = std::fs::read_to_string(&path).unwrap();
            let want = r.message();
            let err = add(&path, "TESTSER3", |_| r).expect_err("refused");
            assert_eq!(err, want);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[test]
    fn a_bad_serial_or_a_broken_file_is_refused_before_any_probe() {
        let (_d, path) = node_toml("");
        let err = add(&path, "TEST SER", |_| panic!("must not probe")).unwrap_err();
        assert!(err.starts_with("vocoder.trusted_serials: "), "{err}");

        let (dir2, broken) = node_toml("\n[vocoder]\nport = \"/dev/ttyUSB0\"\n");
        assert!(add(&broken, "TESTSER1", |_| panic!("must not probe")).is_err());

        let missing = dir2.path().join("absent.toml");
        let err = add(&missing, "TESTSER1", |_| panic!("must not probe")).unwrap_err();
        assert!(err.starts_with("cannot read config"), "{err}");
        assert!(!missing.exists(), "add never bootstraps a config");
    }

    #[test]
    fn the_list_marks_trusted_ports_and_names_absent_sticks() {
        let cands = vec![
            VocoderCandidate {
                serial: "TESTSER1".into(),
                port: "/dev/cu.usbserial-TESTSER1".into(),
            },
            VocoderCandidate {
                serial: "CATCABLE".into(),
                port: "/dev/cu.usbserial-CATCABLE".into(),
            },
        ];
        let trust = VocoderTrust::new(["TESTSER1", "TESTSER9"]);
        let out = list_report(&cands, &trust);
        let line = |s: &str| {
            out.lines()
                .find(|l| l.contains(s))
                .unwrap_or_else(|| panic!("{s} listed:\n{out}"))
                .to_string()
        };
        assert!(line("TESTSER1").trim_end().ends_with(" trusted"));
        assert!(line("CATCABLE").trim_end().ends_with("not trusted"));
        assert!(
            out.contains("Trusted but not attached:\n  TESTSER9\n"),
            "{out}"
        );
        assert!(out.contains("nothing was opened"), "{out}");
    }

    #[test]
    fn an_empty_list_says_the_node_opens_no_vocoder() {
        let out = list_report(&[], &VocoderTrust::none());
        assert!(out.contains("No vocoder-class USB devices"), "{out}");
        assert!(out.contains("this node opens none"), "{out}");
    }
}
