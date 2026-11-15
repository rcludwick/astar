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
//! [`full_frames`] reads only full frames: mini voice frames carry no
//! subclass, sequence number or call number worth reporting, and leaving
//! them out keeps a minute of capture small. [`voice_frames`] is the
//! exception: it reads one direction's voice, mini frames included, and
//! decodes it, for the speech-echo checks.

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

// ---- voice on the wire ------------------------------------------------------

/// One voice frame (full or mini) between two ports, decoded to PCM.
#[derive(Debug, Clone, PartialEq)]
pub struct WireVoice {
    /// Pod clock, seconds since the epoch.
    pub t: f64,
    /// Samples per second of `samples` (8000, or 16000 for slin16).
    pub rate: u32,
    pub samples: Vec<i16>,
}

/// The shell command that dumps every UDP payload from `src` to `dst`
/// port (since `since`, pod clock), one `time|hex` line each. `src` `None`
/// means any sender but the ASL3 node's port (`IAX_PORTS[0]`).
#[must_use]
pub fn voice_script(capture_dir: &str, src: Option<u16>, dst: u16, since: Option<f64>) -> String {
    // With no `src`: any sender but the ASL3 node's port. Toward
    // astar-server that is the host-side probe (it arrives from a forwarded
    // port); toward the node it is astar-server from any port (a link it
    // dialled leaves from its own socket, not 4570).
    let mut filter = if let Some(src) = src {
        format!("udp.srcport == {src} && udp.dstport == {dst}")
    } else {
        format!("udp.dstport == {dst} && udp.srcport != {}", IAX_PORTS[0])
    };
    if let Some(t) = since {
        let _ = write!(filter, " && frame.time_epoch >= {t:.6}");
    }
    format!(
        "for f in $(ls {dir}/*.pcapng 2>/dev/null | sort); do \
         tshark -r \"$f\" -Y '{filter}' -T fields -E separator='|' -e frame.time_epoch -e udp.payload 2>/dev/null; \
         done; true",
        dir = capture_dir.trim_end_matches('/'),
    )
}

/// G.711 mu-law to linear.
fn ulaw(b: u8) -> i16 {
    let u = !b;
    let exp = (u >> 4) & 7;
    let mag = ((i32::from(u & 0x0f) << 3) + 0x84) << exp;
    let v = mag - 0x84;
    i16::try_from(if u & 0x80 == 0 { v } else { -v }).unwrap_or(0)
}

/// G.711 A-law to linear.
fn alaw(b: u8) -> i16 {
    let a = b ^ 0x55;
    let exp = (a >> 4) & 7;
    let mant = i32::from(a & 0x0f);
    let v = if exp == 0 {
        (mant << 4) + 8
    } else {
        ((mant << 4) + 0x108) << (exp - 1)
    };
    i16::try_from(if a & 0x80 == 0 { -v } else { v }).unwrap_or(0)
}

/// Decode one IAX2 datagram if it is a voice frame. `format` is the codec
/// of the last full voice frame seen on this stream (a mini frame carries
/// none), updated when this datagram is one; before any, the codec is read
/// off the payload size.
#[must_use]
pub fn decode_voice(datagram: &[u8], format: &mut Option<u32>) -> Option<(u32, Vec<i16>)> {
    let payload = if datagram.first()? & 0x80 != 0 {
        // Full frame: 12-byte header; type at 10, subclass at 11.
        if datagram.len() < 12 || datagram[10] != ftype::VOICE {
            return None;
        }
        let c = datagram[11];
        // The C bit: the subclass is a power of two.
        *format = Some(if c & 0x80 != 0 {
            1u32.checked_shl(u32::from(c & 0x7f))?
        } else {
            u32::from(c)
        });
        &datagram[12..]
    } else {
        // Mini frame: source call (non-zero) and a 16-bit timestamp. A zero
        // call number is a meta frame (video, trunk), not voice.
        if datagram.len() < 4 || (datagram[0] == 0 && datagram[1] == 0) {
            return None;
        }
        &datagram[4..]
    };
    let be = |p: &[u8]| -> Vec<i16> {
        p.as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_be_bytes(c))
            .collect()
    };
    let le = |p: &[u8]| -> Vec<i16> {
        p.as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c))
            .collect()
    };
    // No full voice frame seen yet in the window (a talk spurt later in a
    // call is all mini frames): read the codec off the payload size. 20 ms
    // is 160 bytes of G.711 (ulaw; the rig never offers alaw), 320 of slin,
    // 640 of slin16.
    let codec = (*format).or(match payload.len() {
        160 => Some(0x4),
        320 => Some(0x40),
        640 => Some(0x8000),
        _ => None,
    })?;
    match codec {
        0x4 => Some((8000, payload.iter().map(|&b| ulaw(b)).collect())),
        0x8 => Some((8000, payload.iter().map(|&b| alaw(b)).collect())),
        0x40 => Some((8000, be(payload))),
        // 8 kHz slin is big-endian on the wire (the ASL3 node sends it so);
        // slin16 goes little-endian (astar-server sends it so, and the node
        // plays it at the right level).
        0x8000 => Some((16000, le(payload))),
        _ => None,
    }
}

/// Every voice frame sent from port `src` (`None`: any port but the ASL3
/// node's) to port `dst` since `since` (pod
/// clock), decoded, in time order. Mini frames included: this is the audio
/// itself, as it crossed the link.
pub fn voice_frames(
    rig: &Rig,
    src: Option<u16>,
    dst: u16,
    since: Option<f64>,
) -> Result<Vec<WireVoice>, RigError> {
    let Some(cap) = &rig.cfg.capture_container else {
        return Ok(Vec::new());
    };
    let script = voice_script(&rig.cfg.capture_dir, src, dst, since);
    let out = rig.podman().exec(cap, &["sh", "-c", &script])?;
    let mut lines: Vec<(f64, Vec<u8>)> = out
        .lines()
        .filter_map(|l| {
            let (t, hex) = l.split_once('|')?;
            Some((t.trim().parse::<f64>().ok()?, unhex(hex)))
        })
        .collect();
    lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut format = None;
    Ok(lines
        .into_iter()
        .filter_map(|(t, d)| {
            decode_voice(&d, &mut format).map(|(rate, samples)| WireVoice { t, rate, samples })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_frames_decode_full_then_mini() {
        let mut fmt = None;
        // A mini frame before any full voice frame in the window (a talk
        // spurt later in a call): the codec is read off the payload size,
        // 320 bytes being 20 ms of slin.
        let mut mini = vec![0x00, 0x05, 0, 20];
        mini.extend(std::iter::repeat_n(0u8, 318));
        mini.extend_from_slice(&[0x01, 0x02]);
        let (rate, pcm) = decode_voice(&mini, &mut fmt).expect("slin by size");
        assert_eq!((rate, pcm.len(), pcm[159]), (8000, 160, 0x0102));
        assert_eq!(fmt, None, "a guess is not remembered");
        // 160 bytes is ulaw (the rig never offers alaw); 640 is slin16.
        let mut mini = vec![0x00, 0x05, 0, 20];
        mini.extend(std::iter::repeat_n(0xffu8, 160));
        assert_eq!(decode_voice(&mini, &mut fmt).map(|v| v.0), Some(8000));
        // slin16 travels little-endian (8 kHz slin is big-endian).
        let mut mini = vec![0x00, 0x05, 0, 20];
        mini.extend_from_slice(&[0x9a, 0x00]);
        mini.extend(std::iter::repeat_n(0u8, 638));
        let (rate, pcm) = decode_voice(&mini, &mut fmt).expect("slin16 by size");
        assert_eq!((rate, pcm[0]), (16000, 0x009a));
        // Any other size, with no codec known, is not voice we can read.
        assert_eq!(decode_voice(&[0x00, 0x05, 0, 20, 1, 2], &mut fmt), None);
        // Full voice frame, slin (0x40), one sample 0x0102 big-endian.
        let mut full = vec![0x80, 0x05, 0, 1, 0, 0, 0, 20, 0, 0, ftype::VOICE, 0x40];
        full.extend_from_slice(&[0x01, 0x02]);
        assert_eq!(decode_voice(&full, &mut fmt), Some((8000, vec![0x0102])));
        // A mini frame now decodes as slin too.
        assert_eq!(
            decode_voice(&[0x00, 0x05, 0, 40, 0xff, 0xfe], &mut fmt),
            Some((8000, vec![-2]))
        );
        // slin16 is subclass 1 << 15, sent with the C bit.
        full[11] = 0x80 | 15;
        assert_eq!(decode_voice(&full, &mut fmt).map(|v| v.0), Some(16000));
        // A non-voice full frame is skipped and leaves the codec alone.
        full[10] = ftype::CONTROL;
        assert_eq!(decode_voice(&full, &mut fmt), None);
        assert_eq!(fmt, Some(0x8000));
        // A meta frame (call 0) is not voice.
        assert_eq!(decode_voice(&[0, 0, 0, 1, 9, 9], &mut fmt), None);
    }

    #[test]
    fn g711_decodes_to_the_standard_values() {
        assert_eq!(ulaw(0xff), 0);
        assert_eq!(ulaw(0x7f), 0);
        assert_eq!(ulaw(0x80), 32124);
        assert_eq!(ulaw(0x00), -32124);
        assert_eq!(alaw(0xd5), 8);
        assert_eq!(alaw(0x55), -8);
        assert_eq!(alaw(0xaa), 32256);
    }

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
