// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-netrec sstv <file>`: find the SSTV pictures in a recording that is
//! already on disk and save each one as a PNG.
//!
//! Three kinds of file are read:
//!
//! - **WAV**: RIFF PCM, 16-bit, any sample rate. Only the first channel is
//!   decoded.
//! - **FLAC**: any rate and bit depth (scaled to 16 bits), first channel.
//! - **`.pcap`**: a picture's own capture or a packet log. The call's
//!   received audio is rebuilt from its voice packets in timestamp order,
//!   and frames known to be lost are handed to the decoder as holes rather
//!   than silence. The codec comes from `--codec`, else the header line of
//!   the `.jitter.jsonl` file beside the capture, else the first full voice
//!   frame. Our own address comes from `--local`, else that same header
//!   line, and every packet sent from it is left out, so a capture holding
//!   both directions never rebuilds our own transmitted audio.
//!
//! A recording that is cut short or damaged part way through still gives up
//! the pictures heard before the damage; the damage is reported alongside
//! them ([`Decoded::damage`]).
//!
//! Nothing here opens a socket: the command reads one file and writes
//! pictures next to it (or into `--out`).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use astar_iax::rebuild::{RebuildError, rebuild_received};
use astar_iax_core::VoiceFormat;
use astar_sstv::{Decoder, Event, Picture, SILENCE_SECS, SstvError, decode_rebuilt};

/// The formats named in every "cannot read this" error.
const FORMATS: &str = "a WAV (16-bit PCM), FLAC or .pcap recording";

/// Why a recording could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum SstvCmdError {
    /// The file's name does not say WAV, FLAC or pcap.
    #[error("{}: not {FORMATS}", .0.display())]
    Format(PathBuf),
    /// The file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    /// The file is named as one format but its contents are not that.
    #[error("{}: {message}", path.display())]
    Bad { path: PathBuf, message: String },
    /// The capture's audio could not be rebuilt.
    #[error("{}: {source}", path.display())]
    Rebuild { path: PathBuf, source: RebuildError },
    /// A packet log of mini frames only, with no companion naming the codec.
    #[error(
        "{}: the capture does not say which codec it carries; name it with --codec <ulaw|alaw|slin|slin16|gsm>",
        .0.display()
    )]
    NoCodec(PathBuf),
    /// The decoder refused the recording's sample rate.
    #[error("{}: {source}", path.display())]
    Rate { path: PathBuf, source: SstvError },
    /// A picture's file is already there, as after a second run into the
    /// same folder. An existing file is never replaced.
    #[error(
        "{} already exists, and a picture is never written over; remove it, or choose another folder with --out <folder>",
        .0.display()
    )]
    Exists(PathBuf),
    /// A picture could not be saved.
    #[error("cannot save {}: {source}", path.display())]
    Save { path: PathBuf, source: io::Error },
    /// The recording was damaged before any picture was heard.
    #[error("{}: {damage}; no SSTV picture was found before it", path.display())]
    Damaged { path: PathBuf, damage: String },
}

/// What the caller may say about a `.pcap`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// The codec of its voice frames, which overrides the companion's.
    pub codec: Option<VoiceFormat>,
    /// Our own address as the capture shows it, which overrides the
    /// companion's. Packets sent from it are left out.
    pub local: Option<SocketAddr>,
}

/// The pictures saved from a recording, and the damage that cut it short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// In the order the pictures were heard.
    pub saved: Vec<Saved>,
    /// Why the recording could not be read to its end, when it could not.
    /// The pictures heard before the damage are still in `saved`.
    pub damage: Option<String>,
}

/// A picture written to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    /// The mode's display name, such as `"Robot 36"`.
    pub mode: &'static str,
    /// Display rows heard.
    pub lines: u32,
    /// True when the picture was cut short.
    pub partial: bool,
}

impl Saved {
    /// The line printed for this picture:
    /// `saved <path> (PD120, 496 lines)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let partial = if self.partial { ", partial" } else { "" };
        format!(
            "saved {} ({}, {} lines{partial})",
            self.path.display(),
            self.mode,
            self.lines
        )
    }
}

/// Parse a `--codec` name, as Asterisk spells it.
///
/// # Errors
///
/// A message naming the accepted codecs.
pub fn parse_codec(name: &str) -> Result<VoiceFormat, String> {
    codec_named(name).ok_or_else(|| {
        format!("unknown codec {name:?}: expected one of ulaw, alaw, slin, slin16, gsm")
    })
}

fn codec_named(name: &str) -> Option<VoiceFormat> {
    [
        VoiceFormat::G711U,
        VoiceFormat::G711A,
        VoiceFormat::Slin,
        VoiceFormat::Slin16,
        VoiceFormat::Gsm,
    ]
    .into_iter()
    .find(|f| f.wire_name().eq_ignore_ascii_case(name.trim()))
}

/// Decode every SSTV picture in `input` and save each one into `out` as
/// `<input stem>-NNN-<mode token>.png`. Returns the files written, in the
/// order their pictures were heard. A recording damaged part way through
/// gives up the pictures heard before the damage, which is not reported
/// here: use [`decode_file_with`] to hear about it.
///
/// # Errors
///
/// As [`decode_file_with`].
pub fn decode_file(input: &Path, out: &Path) -> Result<Vec<PathBuf>, SstvCmdError> {
    decode_file_with(input, out, &Options::default())
        .map(|d| d.saved.into_iter().map(|s| s.path).collect())
}

/// [`decode_file`], with what the caller knows about a `.pcap`, saying what
/// each saved picture is and what damage, if any, cut the recording short.
///
/// # Errors
///
/// [`SstvCmdError`] when the file is not a WAV, FLAC or pcap, cannot be
/// read or decoded, or a picture cannot be saved; and
/// [`SstvCmdError::Damaged`] when it was damaged before any picture was
/// heard. Pictures saved before a save error stay on disk.
pub fn decode_file_with(
    input: &Path,
    out: &Path,
    options: &Options,
) -> Result<Decoded, SstvCmdError> {
    let kind = Kind::of(input).ok_or_else(|| SstvCmdError::Format(input.to_path_buf()))?;
    let (events, damage) = match kind {
        Kind::Wav => {
            let wav = read_wav(input)?;
            let mut feed = Feed::new(input, wav.rate)?;
            wav.samples.iter().for_each(|&s| feed.push(s));
            (feed.end(wav.damage.is_none()), wav.damage)
        }
        Kind::Flac => decode_flac(input)?,
        Kind::Pcap => (decode_pcap(input, options)?, None),
    };
    let saved = save(input, out, &events)?;
    match damage {
        Some(damage) if saved.is_empty() => Err(SstvCmdError::Damaged {
            path: input.to_path_buf(),
            damage,
        }),
        damage => Ok(Decoded { saved, damage }),
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Wav,
    Flac,
    Pcap,
}

impl Kind {
    fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "wav" | "wave" => Some(Self::Wav),
            "flac" => Some(Self::Flac),
            "pcap" => Some(Self::Pcap),
            _ => None,
        }
    }
}

fn read_err(path: &Path) -> impl FnOnce(io::Error) -> SstvCmdError + '_ {
    move |source| SstvCmdError::Read {
        path: path.to_path_buf(),
        source,
    }
}

fn bad(path: &Path, message: impl Into<String>) -> SstvCmdError {
    SstvCmdError::Bad {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

/// One decoder fed a recording's samples in 20 ms pieces.
struct Feed {
    decoder: Decoder,
    rate: u32,
    piece: Vec<i16>,
    step: usize,
    events: Vec<Event>,
}

impl Feed {
    fn new(input: &Path, rate: u32) -> Result<Self, SstvCmdError> {
        let decoder = Decoder::new(rate).map_err(|source| SstvCmdError::Rate {
            path: input.to_path_buf(),
            source,
        })?;
        let step = (rate as usize / 50).max(1);
        Ok(Self {
            decoder,
            rate,
            piece: Vec::with_capacity(step),
            step,
            events: Vec::new(),
        })
    }

    fn push(&mut self, sample: i16) {
        self.piece.push(sample);
        if self.piece.len() == self.step {
            self.events.extend(self.decoder.push(&self.piece));
            self.piece.clear();
        }
    }

    /// The recording is over. When it ended cleanly, enough silence follows
    /// for a picture that ran to its end to finish as the decoder would on
    /// air. When it was damaged, the silence would only be made up: the
    /// picture in flight is closed at once with the rows heard, so it is
    /// saved as partial if enough of it was heard.
    fn end(mut self, clean: bool) -> Vec<Event> {
        if clean {
            let tail = (SILENCE_SECS as usize + 1) * self.rate as usize;
            for _ in 0..tail {
                self.push(0);
            }
        }
        self.events.extend(self.decoder.push(&self.piece));
        self.events.extend(self.decoder.finish());
        self.events
    }
}

/// A WAV's rate and first channel, and why its data stops short, if it
/// does.
#[derive(Debug, PartialEq, Eq)]
struct Wav {
    rate: u32,
    samples: Vec<i16>,
    damage: Option<String>,
}

/// The highest sample rate the decoder takes.
const MAX_RATE: u32 = 192_000;

/// A RIFF PCM 16-bit WAV.
fn read_wav(path: &Path) -> Result<Wav, SstvCmdError> {
    let bytes = std::fs::read(path).map_err(read_err(path))?;
    parse_wav(&bytes).map_err(|m| bad(path, m))
}

fn parse_wav(bytes: &[u8]) -> Result<Wav, String> {
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF WAVE file".to_string());
    }
    let mut at = 12;
    let mut format: Option<(u16, u32)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32_at(bytes, at + 4) as usize;
        let body = at + 8;
        // A truncated last chunk (or a streaming writer's 0xFFFFFFFF) runs to
        // the end of the file.
        let end = body.saturating_add(len).min(bytes.len());
        match id {
            b"fmt " => {
                if end - body < 16 {
                    return Err("its fmt chunk is too short".to_string());
                }
                let tag = u16_at(bytes, body);
                let channels = u16_at(bytes, body + 2);
                let rate = u32_at(bytes, body + 4);
                let bits = u16_at(bytes, body + 14);
                // 0xFFFE is WAVE_FORMAT_EXTENSIBLE, whose PCM subformat reads
                // the same.
                let pcm = tag == 1
                    || (tag == 0xFFFE && end - body >= 26 && u16_at(bytes, body + 24) == 1);
                if !pcm || bits != 16 {
                    return Err(format!(
                        "only 16-bit PCM WAV is read, and this is format {tag} at {bits} bits"
                    ));
                }
                if channels == 0 {
                    return Err("its fmt chunk names no channels".to_string());
                }
                if rate == 0 || rate > MAX_RATE {
                    return Err(format!(
                        "its sample rate of {rate} Hz is not one SSTV is read at (1 to {MAX_RATE} Hz)"
                    ));
                }
                format = Some((channels, rate));
            }
            b"data" => {
                let (channels, rate) =
                    format.ok_or_else(|| "its data comes before its fmt chunk".to_string())?;
                let frame = usize::from(channels) * 2;
                let samples = bytes[body..end]
                    .chunks_exact(frame)
                    .map(|f| i16::from_le_bytes([f[0], f[1]]))
                    .collect();
                // A streaming writer that never came back to fill in the
                // length leaves 0xFFFFFFFF; any other length past the end
                // of the file means the file was cut short.
                let streamed = len == 0xFFFF_FFFF;
                let damage = (!streamed && body.saturating_add(len) > bytes.len()).then(|| {
                    format!(
                        "the recording is cut short: its data chunk declares {len} bytes but the file holds {}",
                        end - body
                    )
                });
                return Ok(Wav {
                    rate,
                    samples,
                    damage,
                });
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = body.saturating_add(len).saturating_add(len & 1);
    }
    Err("it has no data chunk".to_string())
}

/// A FLAC file's first channel, scaled to 16 bits, decoded as it is read,
/// and why it could not be read to its end, if it could not.
fn decode_flac(path: &Path) -> Result<(Vec<Event>, Option<String>), SstvCmdError> {
    let mut reader = claxon::FlacReader::open(path).map_err(|e| match e {
        claxon::Error::IoError(source) => SstvCmdError::Read {
            path: path.to_path_buf(),
            source,
        },
        other => bad(path, format!("not a FLAC file: {other}")),
    })?;
    let info = reader.streaminfo();
    let channels = (info.channels as usize).max(1);
    let bits = info.bits_per_sample;
    let mut feed = Feed::new(path, info.sample_rate)?;
    let mut damage = None;
    // Every sample is looked at, not only the first channel's: an error
    // can land on any of them.
    for (i, sample) in reader.samples().enumerate() {
        match sample {
            Ok(s) if i % channels == 0 => feed.push(to_i16(s, bits)),
            Ok(_) => {}
            Err(e) => {
                damage = Some(format!("the FLAC stream is damaged: {e}"));
                break;
            }
        }
    }
    Ok((feed.end(damage.is_none()), damage))
}

#[allow(clippy::cast_possible_truncation)] // shifted into 16 bits first
fn to_i16(sample: i32, bits: u32) -> i16 {
    if bits >= 16 {
        (sample >> (bits - 16)) as i16
    } else {
        (sample << (16 - bits)) as i16
    }
}

/// Rebuild a capture's received audio from its voice packets and decode it,
/// honouring the frames known to be lost.
fn decode_pcap(path: &Path, options: &Options) -> Result<Vec<Event>, SstvCmdError> {
    let bytes = std::fs::read(path).map_err(read_err(path))?;
    let packets = astar_record::pcap::read_pcap(&bytes)
        .map_err(|m| bad(path, format!("not a pcap file: {m}")))?;
    let companion = companion(path);
    let codec = options.codec.or(companion.codec);
    let local = options.local.or(companion.local);
    let rebuilt = rebuild_received(&packets, codec, local).map_err(|source| match source {
        RebuildError::NoCodec => SstvCmdError::NoCodec(path.to_path_buf()),
        source => SstvCmdError::Rebuild {
            path: path.to_path_buf(),
            source,
        },
    })?;
    decode_rebuilt(rebuilt.rate, &rebuilt.samples, &rebuilt.holes).map_err(|source| {
        SstvCmdError::Rate {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// What the header line of a packet log's companion says.
#[derive(Debug, Default, PartialEq, Eq)]
struct Companion {
    codec: Option<VoiceFormat>,
    /// Our own address on the call.
    local: Option<SocketAddr>,
}

/// The header line of the packet log's companion, `<name>.jitter.jsonl`
/// beside `<name>.pcap`; empty when there is none or it cannot be read.
fn companion(pcap: &Path) -> Companion {
    companion_header(pcap)
        .map(|header| Companion {
            codec: header
                .get("codec")
                .and_then(serde_json::Value::as_str)
                .and_then(codec_named),
            local: header
                .get("local")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse().ok()),
        })
        .unwrap_or_default()
}

fn companion_header(pcap: &Path) -> Option<serde_json::Value> {
    let stem = pcap.file_stem()?.to_str()?;
    let companion = pcap.with_file_name(format!("{stem}.jitter.jsonl"));
    let mut first = String::new();
    // Only the header line is wanted, and it is short: never read a huge or
    // endless file whole.
    BufReader::new(File::open(companion).ok()?)
        .take(64 * 1024)
        .read_line(&mut first)
        .ok()?;
    serde_json::from_str(&first).ok()
}

/// Save every finished picture and every abandoned one worth keeping.
fn save(input: &Path, out: &Path, events: &[Event]) -> Result<Vec<Saved>, SstvCmdError> {
    let stem = input
        .file_stem()
        .map_or_else(|| "sstv".into(), |s| s.to_string_lossy());
    let kept = events.iter().filter_map(|e| match e {
        Event::Finished(p) => Some(p),
        Event::Abandoned(p) if p.worth_keeping() => Some(p),
        _ => None,
    });
    let mut saved = Vec::new();
    for (n, picture) in kept.enumerate() {
        if saved.is_empty() {
            std::fs::create_dir_all(out).map_err(|source| SstvCmdError::Save {
                path: out.to_path_buf(),
                source,
            })?;
        }
        let path = out.join(format!("{stem}-{:03}-{}.png", n + 1, picture.mode.token()));
        write(picture, &path)?;
        saved.push(Saved {
            path,
            mode: picture.mode.name(),
            lines: picture.lines,
            partial: picture.partial,
        });
    }
    Ok(saved)
}

fn write(picture: &Picture, path: &Path) -> Result<(), SstvCmdError> {
    picture.write_png(path).map_err(|source| {
        if source.kind() == io::ErrorKind::AlreadyExists {
            SstvCmdError::Exists(path.to_path_buf())
        } else {
            SstvCmdError::Save {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_follow_asterisk() {
        assert_eq!(parse_codec("ulaw"), Ok(VoiceFormat::G711U));
        assert_eq!(parse_codec("SLIN16"), Ok(VoiceFormat::Slin16));
        assert!(parse_codec("g729").unwrap_err().contains("ulaw"));
    }

    #[test]
    fn wav_takes_the_first_channel_and_skips_odd_chunks() {
        let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
        // An odd-length chunk before fmt, padded to even.
        b.extend_from_slice(b"LIST");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&[1, 2, 3, 0]);
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 2] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&11_025u32.to_le_bytes());
        b.extend_from_slice(&44_100u32.to_le_bytes());
        for v in [4u16, 16] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&8u32.to_le_bytes());
        for v in [5i16, -1, -7, -1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            parse_wav(&b),
            Ok(Wav {
                rate: 11_025,
                samples: vec![5, -7],
                damage: None
            })
        );
    }

    #[test]
    fn wav_refuses_eight_bit() {
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&8_000u32.to_le_bytes());
        b.extend_from_slice(&8_000u32.to_le_bytes());
        for v in [1u16, 8] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert!(parse_wav(&b).unwrap_err().contains("16-bit"));
    }

    /// A RIFF header and the `chunks` given, each `(id, declared length,
    /// body)`, written as is.
    fn riff(chunks: &[(&[u8; 4], u32, Vec<u8>)]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
        for (id, len, body) in chunks {
            b.extend_from_slice(*id);
            b.extend_from_slice(&len.to_le_bytes());
            b.extend_from_slice(body);
        }
        b
    }

    /// A 16-bit PCM fmt body.
    fn fmt(channels: u16, rate: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b
    }

    fn refused(bytes: &[u8]) -> String {
        parse_wav(bytes).expect_err("a hostile WAV must be refused")
    }

    #[test]
    fn wav_refuses_a_short_fmt_chunk() {
        // Declared short, and declared whole but cut off by the file's end.
        let short = riff(&[(b"fmt ", 10, fmt(1, 8_000)[..10].to_vec())]);
        assert!(refused(&short).contains("fmt chunk is too short"));
        let cut = riff(&[(b"fmt ", 16, fmt(1, 8_000)[..10].to_vec())]);
        assert!(refused(&cut).contains("fmt chunk is too short"));
    }

    #[test]
    fn wav_refuses_data_before_fmt() {
        let b = riff(&[(b"data", 4, vec![0; 4]), (b"fmt ", 16, fmt(1, 8_000))]);
        assert!(refused(&b).contains("before its fmt"));
    }

    #[test]
    fn wav_refuses_zero_channels() {
        let b = riff(&[(b"fmt ", 16, fmt(0, 8_000)), (b"data", 4, vec![0; 4])]);
        assert!(refused(&b).contains("no channels"));
    }

    #[test]
    fn wav_refuses_a_rate_sstv_cannot_be_read_at() {
        for rate in [0, 192_001, u32::MAX / 2] {
            let b = riff(&[(b"fmt ", 16, fmt(1, rate)), (b"data", 4, vec![0; 4])]);
            assert!(refused(&b).contains("sample rate"), "{rate}");
        }
        let b = riff(&[(b"fmt ", 16, fmt(1, 192_000)), (b"data", 4, vec![0; 4])]);
        assert!(parse_wav(&b).is_ok());
    }

    #[test]
    fn wav_data_longer_than_the_file_is_read_and_reported() {
        let b = riff(&[
            (b"fmt ", 16, fmt(1, 8_000)),
            (b"data", 1_000, vec![1, 0, 2, 0]),
        ]);
        let wav = parse_wav(&b).unwrap();
        assert_eq!(wav.samples, [1, 2]);
        let damage = wav.damage.expect("the cut is reported");
        assert!(damage.contains("1000 bytes"), "{damage}");
        assert!(damage.contains("holds 4"), "{damage}");
        // A streaming writer's unfilled length is not damage.
        let b = riff(&[
            (b"fmt ", 16, fmt(1, 8_000)),
            (b"data", u32::MAX, vec![1, 0]),
        ]);
        let wav = parse_wav(&b).unwrap();
        assert_eq!((wav.samples, wav.damage), (vec![1], None));
    }

    #[test]
    fn wav_refuses_a_file_cut_inside_a_chunk_header() {
        let mut b = riff(&[(b"fmt ", 16, fmt(1, 8_000))]);
        b.extend_from_slice(b"dat");
        assert!(refused(&b).contains("no data chunk"));
        let mut b = riff(&[(b"fmt ", 16, fmt(1, 8_000))]);
        b.extend_from_slice(b"data\x10\0");
        assert!(refused(&b).contains("no data chunk"));
        // Shorter than the RIFF header itself.
        assert!(refused(b"RIFF\0\0").contains("not a RIFF"));
        assert!(refused(b"").contains("not a RIFF"));
    }

    #[test]
    fn wav_survives_a_huge_declared_chunk_before_fmt() {
        // A chunk claiming 4 GiB must not overflow the walk.
        let b = riff(&[(b"LIST", u32::MAX - 1, vec![0; 8])]);
        assert!(refused(&b).contains("no data chunk"));
    }

    #[test]
    fn samples_scale_to_sixteen_bits() {
        assert_eq!(to_i16(0x7F_FF00, 24), 0x7FFF);
        assert_eq!(to_i16(-128, 8), -32_768);
        assert_eq!(to_i16(1234, 16), 1234);
    }
}
