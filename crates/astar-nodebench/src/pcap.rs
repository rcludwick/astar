// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! IAX2 full frames out of the tshark sidecar's ring buffer.
//!
//! tshark runs *inside* the capture container (`podman exec`), over every
//! file in its capture directory, so this works the same whether the run
//! directory is a hostPath or not. Times are the pod's clock
//! (`frame.time_epoch`); use [`pod_now`] for a matching "since" mark, never
//! the host's clock (the podman VM's clock can drift from the Mac's).
//!
//! Only full frames are read: mini voice frames carry no subclass, sequence
//! number or call number worth reporting, and leaving them out keeps a
//! minute of capture small.

use std::fmt::Write as _;

use crate::rig::{Rig, RigError};

/// The IAX2 ports tshark should decode (Asterisk's 4569 is the default;
/// astar-server's 4570 is not).
pub const IAX_PORTS: [u16; 2] = [4569, 4570];

/// One IAX2 full frame.
#[derive(Debug, Clone, PartialEq)]
pub struct IaxFrame {
    /// Pod clock, seconds since the epoch.
    pub t: f64,
    pub src: String,
    pub dst: String,
    pub src_port: u16,
    pub dst_port: u16,
    pub src_call: u32,
    pub dst_call: u32,
    /// The frame's own timestamp, ms.
    pub ts: u32,
    pub oseq: u32,
    pub iseq: u32,
    /// Frame type (2 voice, 4 control, 6 IAX, 7 text, 1/12 DTMF...).
    pub ftype: u8,
    /// Subclass for IAX, control and DTMF frames (0 if none).
    pub sub: u32,
    /// TEXT payload, or the HANGUP/REJECT cause.
    pub text: String,
    /// Information elements of an IAX (type 6) frame, in order.
    pub ies: Vec<(u8, Vec<u8>)>,
}

/// IAX frame types the bench names.
pub mod ftype {
    pub const DTMF_END: u8 = 1;
    pub const VOICE: u8 = 2;
    pub const CONTROL: u8 = 4;
    pub const IAX: u8 = 6;
    pub const TEXT: u8 = 7;
    pub const DTMF_BEGIN: u8 = 12;
}

/// IAX (type 6) subclasses the bench names.
pub mod iax {
    pub const NEW: u32 = 1;
    pub const PING: u32 = 2;
    pub const PONG: u32 = 3;
    pub const ACK: u32 = 4;
    pub const HANGUP: u32 = 5;
    pub const REJECT: u32 = 6;
    pub const ACCEPT: u32 = 7;
    pub const INVAL: u32 = 10;
    pub const LAGRQ: u32 = 11;
    pub const LAGRP: u32 = 12;
    pub const REGREQ: u32 = 13;
    pub const REGACK: u32 = 15;
    pub const VNAK: u32 = 18;
    pub const CALLTOKEN: u32 = 40;
}

impl IaxFrame {
    /// `PING`, `ACCEPT`, `TEXT "..."`, `KEY` — a short label for reports.
    #[must_use]
    pub fn label(&self) -> String {
        match self.ftype {
            ftype::IAX => {
                iax_name(self.sub).map_or_else(|| format!("IAX({})", self.sub), str::to_string)
            }
            ftype::CONTROL => match self.sub {
                1 => "HANGUP(ctl)".into(),
                4 => "ANSWER".into(),
                12 => "KEY".into(),
                13 => "UNKEY".into(),
                s => format!("CONTROL({s})"),
            },
            ftype::TEXT => format!("TEXT {:?}", truncate(&self.text, 40)),
            ftype::VOICE => format!("VOICE({})", self.sub),
            ftype::DTMF_END | ftype::DTMF_BEGIN => {
                format!("DTMF {}", char::from_u32(self.sub).unwrap_or('?'))
            }
            t => format!("type{t}({})", self.sub),
        }
    }

    #[must_use]
    pub fn is_iax(&self, sub: u32) -> bool {
        self.ftype == ftype::IAX && self.sub == sub
    }

    /// The value of IE `id`, if present.
    #[must_use]
    pub fn ie(&self, id: u8) -> Option<&[u8]> {
        self.ies
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, v)| v.as_slice())
    }

    /// The FORMAT IE as a codec name (`ulaw`, `slin16`...).
    #[must_use]
    pub fn format(&self) -> Option<String> {
        let v = self.ie(ie::FORMAT)?;
        let b: [u8; 4] = v.try_into().ok()?;
        Some(codec_name(u32::from_be_bytes(b)))
    }

    /// An ACK, or anything else that only acknowledges.
    #[must_use]
    pub fn is_ack(&self) -> bool {
        self.is_iax(iax::ACK)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

#[must_use]
pub fn iax_name(sub: u32) -> Option<&'static str> {
    Some(match sub {
        1 => "NEW",
        2 => "PING",
        3 => "PONG",
        4 => "ACK",
        5 => "HANGUP",
        6 => "REJECT",
        7 => "ACCEPT",
        8 => "AUTHREQ",
        9 => "AUTHREP",
        10 => "INVAL",
        11 => "LAGRQ",
        12 => "LAGRP",
        13 => "REGREQ",
        14 => "REGAUTH",
        15 => "REGACK",
        16 => "REGREJ",
        17 => "REGREL",
        18 => "VNAK",
        30 => "POKE",
        40 => "CALLTOKEN",
        _ => return None,
    })
}

const FIELDS: &[&str] = &[
    "frame.time_epoch",
    "ip.src",
    "ip.dst",
    "udp.srcport",
    "udp.dstport",
    "iax2.src_call",
    "iax2.dst_call",
    "iax2.timestamp",
    "iax2.oseqno",
    "iax2.iseqno",
    "iax2.type",
    "iax2.iax.subclass",
    "iax2.control.subclass",
    "iax2.dtmf.subclass",
    "iax2.voice.subclass",
    "iax2.text.text",
    "iax2.iax.cause",
    "udp.payload",
];

/// IE ids the bench reads.
pub mod ie {
    pub const FORMAT: u8 = 0x09;
    pub const CALLTOKEN: u8 = 0x36;
}

/// Parse the IEs after a full frame's 12-byte header.
#[must_use]
pub fn parse_ies(payload: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut b = payload.get(12..).unwrap_or(&[]);
    while b.len() >= 2 {
        let (id, len) = (b[0], usize::from(b[1]));
        let Some(v) = b.get(2..2 + len) else { break };
        out.push((id, v.to_vec()));
        b = &b[2 + len..];
    }
    out
}

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim().replace(':', "");
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

/// An Asterisk codec bit as a name.
#[must_use]
pub fn codec_name(bits: u32) -> String {
    match bits {
        0x2 => "gsm".into(),
        0x4 => "ulaw".into(),
        0x8 => "alaw".into(),
        0x40 => "slin".into(),
        0x8000 => "slin16".into(),
        b => format!("0x{b:x}"),
    }
}

/// The shell command run in the capture container. Every ring-buffer file,
/// oldest first; a file still being written may end mid-packet, which tshark
/// reports on stderr and which is ignored.
#[must_use]
pub fn tshark_script(capture_dir: &str, since: Option<f64>) -> String {
    let mut decode = String::new();
    for p in IAX_PORTS {
        let _ = write!(decode, " -d udp.port=={p},iax2");
    }
    let mut filter = "iax2.packet_type == 1".to_string();
    if let Some(t) = since {
        let _ = write!(filter, " && frame.time_epoch >= {t:.6}");
    }
    let fields: String = FIELDS.iter().fold(String::new(), |mut s, f| {
        let _ = write!(s, " -e {f}");
        s
    });
    format!(
        "for f in $(ls {dir}/*.pcapng 2>/dev/null | sort); do \
         tshark -r \"$f\"{decode} -Y '{filter}' -T fields -E separator='|' -E occurrence=f{fields} 2>/dev/null; \
         done; true",
        dir = capture_dir.trim_end_matches('/'),
    )
}

/// Parse one `|`-separated tshark line.
#[must_use]
pub fn parse_line(line: &str) -> Option<IaxFrame> {
    let f: Vec<&str> = line.split('|').collect();
    if f.len() < FIELDS.len() {
        return None;
    }
    let num = |s: &str| s.trim().parse::<u32>().ok();
    let t = f[0].trim().parse::<f64>().ok()?;
    let ftype = u8::try_from(num(f[10])?).ok()?;
    let sub = match ftype {
        // tshark prints a DTMF subclass as the digit itself.
        ftype::DTMF_END | ftype::DTMF_BEGIN => f[13].trim().chars().next().map_or(0, u32::from),
        ftype::IAX => num(f[11]).unwrap_or(0),
        ftype::CONTROL => num(f[12]).unwrap_or(0),
        ftype::VOICE => num(f[14]).unwrap_or(0),
        _ => 0,
    };
    let text = if f[15].is_empty() { f[16] } else { f[15] };
    let ies = if ftype == ftype::IAX {
        f.get(17).map(|p| parse_ies(&unhex(p))).unwrap_or_default()
    } else {
        Vec::new()
    };
    Some(IaxFrame {
        t,
        src: f[1].to_string(),
        dst: f[2].to_string(),
        src_port: f[3].trim().parse().ok()?,
        dst_port: f[4].trim().parse().ok()?,
        src_call: num(f[5]).unwrap_or(0),
        dst_call: num(f[6]).unwrap_or(0),
        ts: num(f[7]).unwrap_or(0),
        oseq: num(f[8]).unwrap_or(0),
        iseq: num(f[9]).unwrap_or(0),
        ftype,
        sub,
        text: text.to_string(),
        ies,
    })
}

/// Every IAX2 full frame in the capture (since `since`, pod clock), in time
/// order.
pub fn full_frames(rig: &Rig, since: Option<f64>) -> Result<Vec<IaxFrame>, RigError> {
    let Some(cap) = &rig.cfg.capture_container else {
        return Ok(Vec::new());
    };
    let script = tshark_script(&rig.cfg.capture_dir, since);
    let out = rig.podman().exec(cap, &["sh", "-c", &script])?;
    let mut v: Vec<IaxFrame> = out.lines().filter_map(parse_line).collect();
    v.sort_by(|a, b| a.t.total_cmp(&b.t));
    Ok(v)
}

/// The pod's clock, seconds since the epoch (from the capture container).
pub fn pod_now(rig: &Rig) -> Result<f64, RigError> {
    let c = rig
        .cfg
        .capture_container
        .as_deref()
        .unwrap_or(&rig.cfg.node_container);
    let out = rig.podman().exec(c, &["date", "+%s.%N"])?;
    out.trim().parse::<f64>().map_err(|e| RigError::NotReady {
        what: "pod clock".into(),
        last: format!("{e}: {out:?}"),
    })
}

/// A plain-text table of frames, relative to `t0`, naming each port.
#[must_use]
pub fn render(frames: &[IaxFrame], t0: f64, name: impl Fn(u16) -> String) -> String {
    let mut s = String::from(
        "     t(s)  from            to              calls        ts      oseq iseq  frame\n",
    );
    for f in frames {
        let _ = writeln!(
            s,
            "{:>9.3}  {:<15} {:<15} {:>5}>{:<5}  {:>7}  {:>4} {:>4}  {}{}",
            f.t - t0,
            name(f.src_port),
            name(f.dst_port),
            f.src_call,
            f.dst_call,
            f.ts,
            f.oseq,
            f.iseq,
            f.label(),
            if f.is_iax(iax::HANGUP) || f.is_iax(iax::REJECT) {
                format!(" cause={:?}", f.text)
            } else {
                String::new()
            }
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iax_control_line() {
        let l = "1791752016.219334402|127.0.0.1|127.0.0.1|4569|4570|13185|1|29901|39|35|6|2|||||||";
        let f = parse_line(l).unwrap();
        assert_eq!(f.src_port, 4569);
        assert_eq!(f.dst_port, 4570);
        assert_eq!(f.src_call, 13185);
        assert!(f.is_iax(iax::PING));
        assert_eq!(f.label(), "PING");
    }

    #[test]
    fn parses_text_and_hangup_cause() {
        let t = parse_line("1.0|a|b|4569|4570|1|2|3|4|5|7||||| !NEWKEY! |||").unwrap();
        assert_eq!(t.ftype, ftype::TEXT);
        assert!(t.label().starts_with("TEXT"));
        let h = parse_line("2.0|a|b|4569|4570|1|2|3|4|5|6|5|||||Caller hung up|").unwrap();
        assert!(h.is_iax(iax::HANGUP));
        assert_eq!(h.text, "Caller hung up");
    }

    #[test]
    fn ies_from_payload() {
        // ACCEPT with FORMAT = ulaw, then a 3-byte CALLTOKEN with a NUL.
        let hdr = "80010000000000000000060709040000000436030061 00".replace(' ', "");
        let line = format!("3.0|a|b|4570|4569|1|2|0|0|0|6|7||||||{hdr}");
        let f = parse_line(&line).unwrap();
        assert_eq!(f.format().as_deref(), Some("ulaw"));
        assert_eq!(f.ie(ie::CALLTOKEN), Some(&[0x00, 0x61, 0x00][..]));
    }

    #[test]
    fn short_or_garbage_lines_are_skipped() {
        assert!(parse_line("").is_none());
        assert!(parse_line("x|y").is_none());
    }

    #[test]
    fn script_decodes_both_ports_and_filters_time() {
        let s = tshark_script("/captures/", Some(12.5));
        assert!(s.contains("udp.port==4570,iax2"));
        assert!(s.contains("frame.time_epoch >= 12.500000"));
        assert!(s.contains("ls /captures/*.pcapng"));
    }
}
