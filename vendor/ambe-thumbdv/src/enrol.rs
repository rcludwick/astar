//! Enrolment: the one operation that opens a port before it is trusted.
//!
//! USB identity cannot tell a DVstick from an FT-X CAT/PTT cable (both are
//! FTDI `0403:6015`), so the engine only opens ports whose serial number the
//! user has enrolled. Enrolling is how a serial gets onto that list: the user
//! picks a port, confirms it is not a radio's cable, and [`probe_with`] asks
//! the chip what it is.
//!
//! The probe is deliberately small. It opens the port, drives DTR/RTS low
//! before writing anything, sends RESET and PRODID with the driver's bounded
//! 300 ms deadline per transaction, and closes the port again. It never runs
//! the rest of the init cookbook.
//!
//! Enrolment is always a user action. Nothing in this crate calls it on its
//! own, and tests drive it only through a mock transport factory.

use std::io;
use std::time::Duration;

use crate::device::{transact, DeviceError};
use crate::packet::{prodid_query, reset, Response};
use crate::transport::Transport;

/// The baud rates a probe tries, in order (§6 fallback).
pub const PROBE_BAUDS: [u32; 2] = [460_800, 230_400];

/// What a PRODID string says the chip is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipClass {
    /// `AMBE3000*`: the single-channel chip in a ThumbDV or a DVstick 30.
    /// The engine supports it.
    Ambe3000,
    /// `AMBE3003*`: the multi-channel chip in a DVstick 33. Recognised, not
    /// supported yet.
    Ambe3003,
    /// Anything else.
    Other,
}

impl ChipClass {
    /// Classify a PRODID string.
    #[must_use]
    pub fn of(prodid: &str) -> Self {
        if prodid.starts_with("AMBE3000") {
            ChipClass::Ambe3000
        } else if prodid.starts_with("AMBE3003") {
            ChipClass::Ambe3003
        } else {
            ChipClass::Other
        }
    }
}

/// The result of probing one port.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// The chip answered RESET and PRODID.
    Answered {
        /// The PRODID string, as reported.
        prodid: String,
        /// What that PRODID means.
        class: ChipClass,
        /// The baud rate it answered at.
        baud: u32,
    },
    /// The port opened but nothing answered as an AMBE chip at any baud:
    /// a timeout, or bytes that were not a valid reply.
    NoAnswer,
    /// The port could not be opened, or its control lines could not be
    /// driven low (in which case nothing was written).
    OpenFailed(io::Error),
}

/// Probe one port through `open`, a transport factory taking `(port, baud)`.
///
/// For each of [`PROBE_BAUDS`]: open, deassert DTR/RTS (refusing the port if
/// that fails, before any write), drain briefly, RESET and await READY, then
/// PRODID. The transport is dropped, closing the port, before the next baud
/// is tried and before this returns.
pub fn probe_with<T, F>(port: &str, mut open: F) -> ProbeOutcome
where
    T: Transport,
    F: FnMut(&str, u32) -> io::Result<T>,
{
    let mut opened_any = false;
    let mut last_err = None;
    for baud in PROBE_BAUDS {
        let mut transport = match open(port, baud) {
            Ok(t) => t,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        if let Err(e) = transport.deassert_control_lines() {
            // Lines we cannot drive low are lines that may be keying a
            // radio. Write nothing; refuse.
            return ProbeOutcome::OpenFailed(e);
        }
        opened_any = true;
        match ask_prodid(&mut transport) {
            Ok(prodid) => {
                drop(transport);
                return ProbeOutcome::Answered {
                    class: ChipClass::of(&prodid),
                    prodid,
                    baud,
                };
            }
            Err(_) => drop(transport),
        }
    }
    if opened_any {
        ProbeOutcome::NoAnswer
    } else {
        ProbeOutcome::OpenFailed(
            last_err.unwrap_or_else(|| io::Error::other("port could not be opened")),
        )
    }
}

fn ask_prodid<T: Transport>(transport: &mut T) -> Result<String, DeviceError> {
    let mut drain = [0u8; 1024];
    let _ = transport.recv_some(&mut drain, Duration::from_millis(50));
    match transact(transport, &reset())? {
        Response::Ready => {}
        _ => return Err(DeviceError::Protocol("expected Ready after reset".into())),
    }
    match transact(transport, &prodid_query())? {
        Response::ProdId(s) => Ok(s),
        _ => Err(DeviceError::Protocol("expected ProdId".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hex;
    use std::sync::{Arc, Mutex};

    /// What a scripted port saw, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Open(u32),
        Deassert,
        Send(Vec<u8>),
        Close,
    }

    type Log = Arc<Mutex<Vec<Op>>>;

    /// A transport that answers each send with the next scripted reply and
    /// records every operation, including its own drop.
    struct Scripted {
        log: Log,
        replies: Vec<Vec<u8>>,
        pending: Option<Vec<u8>>,
        deassert_fails: bool,
    }

    impl Transport for Scripted {
        fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::Send(bytes.to_vec()));
            self.pending = if self.replies.is_empty() {
                None
            } else {
                Some(self.replies.remove(0))
            };
            Ok(())
        }
        fn recv_some(&mut self, buf: &mut [u8], _t: Duration) -> io::Result<usize> {
            match self.pending.take() {
                Some(r) => {
                    buf[..r.len()].copy_from_slice(&r);
                    Ok(r.len())
                }
                None => Ok(0),
            }
        }
        fn deassert_control_lines(&mut self) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::Deassert);
            if self.deassert_fails {
                Err(io::Error::other("cannot drive lines"))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            self.log.lock().unwrap().push(Op::Close);
        }
    }

    fn prodid_reply(s: &[u8]) -> Vec<u8> {
        // 61 <len> 00 30 <string>: len counts the field byte plus the string.
        let len = u16::try_from(1 + s.len()).unwrap();
        let mut v = vec![0x61];
        v.extend_from_slice(&len.to_be_bytes());
        v.push(0x00);
        v.push(0x30);
        v.extend_from_slice(s);
        v
    }

    fn ready() -> Vec<u8> {
        hex("61 00 01 00 39")
    }

    /// A factory whose every open yields a port scripted with `replies`.
    fn factory(
        log: &Log,
        replies: Vec<Vec<u8>>,
        deassert_fails: bool,
    ) -> impl FnMut(&str, u32) -> io::Result<Scripted> + '_ {
        move |_port, baud| {
            log.lock().unwrap().push(Op::Open(baud));
            Ok(Scripted {
                log: Arc::clone(log),
                replies: replies.clone(),
                pending: None,
                deassert_fails,
            })
        }
    }

    #[test]
    fn prodid_classification() {
        assert_eq!(ChipClass::of("AMBE3000R"), ChipClass::Ambe3000);
        assert_eq!(ChipClass::of("AMBE3000F"), ChipClass::Ambe3000);
        assert_eq!(ChipClass::of("AMBE3003F"), ChipClass::Ambe3003);
        assert_eq!(ChipClass::of("AMBE2000"), ChipClass::Other);
        assert_eq!(ChipClass::of(""), ChipClass::Other);
    }

    #[test]
    fn an_ambe3000_reply_is_classified_ambe3000() {
        let log = Log::default();
        let out = probe_with(
            "/dev/cu.usbserial-TESTSER1",
            factory(&log, vec![ready(), prodid_reply(b"AMBE3000R\0")], false),
        );
        match out {
            ProbeOutcome::Answered {
                prodid,
                class,
                baud,
            } => {
                assert_eq!(prodid, "AMBE3000R");
                assert_eq!(class, ChipClass::Ambe3000);
                assert_eq!(baud, 460_800);
            }
            other => panic!("expected Answered, got {other:?}"),
        }
        assert_eq!(log.lock().unwrap().last(), Some(&Op::Close), "port closed");
    }

    #[test]
    fn an_ambe3003_reply_is_classified_ambe3003() {
        let log = Log::default();
        let out = probe_with(
            "/dev/cu.usbserial-TESTSER3",
            factory(&log, vec![ready(), prodid_reply(b"AMBE3003F\0")], false),
        );
        assert!(
            matches!(
                out,
                ProbeOutcome::Answered {
                    class: ChipClass::Ambe3003,
                    ..
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn a_silent_port_is_no_answer_and_closed_at_both_bauds() {
        let log = Log::default();
        let out = probe_with("/dev/cu.usbserial-SILENT", factory(&log, vec![], false));
        assert!(matches!(out, ProbeOutcome::NoAnswer), "got {out:?}");
        let ops = log.lock().unwrap().clone();
        let opens = ops.iter().filter(|o| matches!(o, Op::Open(_))).count();
        let closes = ops.iter().filter(|o| **o == Op::Close).count();
        assert_eq!(opens, 2, "both bauds tried: {ops:?}");
        assert_eq!(closes, 2, "every open is closed again: {ops:?}");
    }

    #[test]
    fn a_garbage_reply_is_no_answer_and_closed() {
        let log = Log::default();
        // A CAT radio's reply: not a DVSI packet at all.
        let out = probe_with(
            "/dev/cu.usbserial-CATCABLE",
            factory(&log, vec![b"?;".to_vec(), b"?;".to_vec()], false),
        );
        assert!(matches!(out, ProbeOutcome::NoAnswer), "got {out:?}");
        assert_eq!(log.lock().unwrap().last(), Some(&Op::Close));
    }

    #[test]
    fn control_lines_are_deasserted_before_the_first_write() {
        let log = Log::default();
        let _ = probe_with(
            "/dev/cu.usbserial-TESTSER1",
            factory(&log, vec![ready(), prodid_reply(b"AMBE3000R\0")], false),
        );
        let ops = log.lock().unwrap().clone();
        let deassert = ops.iter().position(|o| *o == Op::Deassert);
        let first_send = ops.iter().position(|o| matches!(o, Op::Send(_)));
        assert!(
            matches!((deassert, first_send), (Some(d), Some(s)) if d < s),
            "DTR/RTS must go low before any byte is written: {ops:?}"
        );
        assert_eq!(ops[0], Op::Open(460_800));
        assert_eq!(
            ops[1],
            Op::Deassert,
            "deassert is the first thing after open"
        );
    }

    #[test]
    fn a_port_whose_lines_cannot_be_driven_is_refused_without_a_write() {
        let log = Log::default();
        let out = probe_with(
            "/dev/cu.usbserial-TESTSER1",
            factory(&log, vec![ready(), prodid_reply(b"AMBE3000R\0")], true),
        );
        assert!(matches!(out, ProbeOutcome::OpenFailed(_)), "got {out:?}");
        let ops = log.lock().unwrap().clone();
        assert!(
            !ops.iter().any(|o| matches!(o, Op::Send(_))),
            "nothing may be written: {ops:?}"
        );
        assert_eq!(ops.last(), Some(&Op::Close));
    }

    #[test]
    fn a_port_that_will_not_open_is_open_failed() {
        let out = probe_with::<Scripted, _>("/dev/cu.usbserial-GONE", |_, _| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        match out {
            ProbeOutcome::OpenFailed(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            other => panic!("expected OpenFailed, got {other:?}"),
        }
    }
}
