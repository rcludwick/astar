// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Reading the FLV stream `ffmpeg` writes on its stdout, one tag at a time.
//!
//! An FLV file is a 9-byte header, then a run of tags. Each tag is an 11-byte
//! header (kind, body length, timestamp, stream id), the body, and a 4-byte
//! "previous tag size" trailer. The body is what an RTMP audio or video
//! message carries, byte for byte, so the publisher forwards it unchanged.

use std::io::{self, Read};

use bytes::Bytes;

/// What a tag carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagKind {
    /// An audio frame (AAC here).
    Audio,
    /// A video frame (H.264 here).
    Video,
    /// Script data, such as `onMetaData`.
    Script,
}

/// One FLV tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlvTag {
    /// Audio, video or script.
    pub kind: TagKind,
    /// The tag's timestamp in milliseconds, from the start of the stream.
    pub ts_ms: u32,
    /// The tag body, as an RTMP message carries it.
    pub data: Bytes,
    /// A video key frame (an IDR picture a viewer can start from).
    pub keyframe: bool,
    /// An AVC or AAC sequence header: the decoder setup, not a frame.
    pub seq_header: bool,
}

const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;
const TAG_SCRIPT: u8 = 18;
/// `SoundFormat` 10 in the high nibble of an audio body's first byte.
const AAC: u8 = 10;
/// `CodecID` 7 in the low nibble of a video body's first byte.
const AVC: u8 = 7;

impl FlvTag {
    /// Build a tag from its kind, time and body, working out `keyframe` and
    /// `seq_header` from the body as FLV defines them.
    #[must_use]
    pub fn new(kind: TagKind, ts_ms: u32, data: Bytes) -> Self {
        let (keyframe, seq_header) = match kind {
            TagKind::Video => {
                let first = data.first().copied().unwrap_or(0);
                let key = first >> 4 == 1;
                let seq = first & 0x0f == AVC && data.get(1) == Some(&0);
                (key && !seq, seq)
            }
            TagKind::Audio => {
                let first = data.first().copied().unwrap_or(0);
                (false, first >> 4 == AAC && data.get(1) == Some(&0))
            }
            TagKind::Script => (false, false),
        };
        Self {
            kind,
            ts_ms,
            data,
            keyframe,
            seq_header,
        }
    }
}

/// Reads [`FlvTag`]s from an FLV byte stream.
pub struct FlvReader<R: Read> {
    inner: R,
    header_read: bool,
}

impl<R: Read> FlvReader<R> {
    /// Read from `inner`, which starts at the FLV header.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            header_read: false,
        }
    }

    /// The next audio, video or script tag, or `None` at a clean end of the
    /// stream. Tags of any other kind are skipped.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidData`] when the stream is not FLV,
    /// [`io::ErrorKind::UnexpectedEof`] when it ends inside a tag, and any
    /// error from the reader.
    pub fn next_tag(&mut self) -> io::Result<Option<FlvTag>> {
        if !self.header_read {
            self.read_header()?;
            self.header_read = true;
        }
        loop {
            let mut head = [0u8; 11];
            if !self.fill_or_end(&mut head)? {
                return Ok(None);
            }
            let kind = head[0] & 0x1f;
            let len = u32::from_be_bytes([0, head[1], head[2], head[3]]) as usize;
            let ts_ms = u32::from_be_bytes([head[7], head[4], head[5], head[6]]);
            let mut body = vec![0u8; len];
            self.inner.read_exact(&mut body)?;
            let mut trailer = [0u8; 4];
            self.inner.read_exact(&mut trailer)?;
            let kind = match kind {
                TAG_AUDIO => TagKind::Audio,
                TAG_VIDEO => TagKind::Video,
                TAG_SCRIPT => TagKind::Script,
                _ => continue,
            };
            return Ok(Some(FlvTag::new(kind, ts_ms, Bytes::from(body))));
        }
    }

    /// The `FLV` signature, the header's own length (9 in every version so
    /// far, but skip whatever it says), and the zero "previous tag size".
    fn read_header(&mut self) -> io::Result<()> {
        let mut head = [0u8; 9];
        self.inner.read_exact(&mut head)?;
        if &head[..3] != b"FLV" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an FLV stream",
            ));
        }
        let size = u32::from_be_bytes([head[5], head[6], head[7], head[8]]);
        if !(9..=1024).contains(&size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FLV header size out of range",
            ));
        }
        let skip = u64::from(size - 9) + 4;
        let got = io::copy(&mut (&mut self.inner).take(skip), &mut io::sink())?;
        if got < skip {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }

    /// Fill `buf` entirely, or return `false` when the stream ends before its
    /// first byte. Ending part-way through is an error.
    fn fill_or_end(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        let mut at = 0;
        while at < buf.len() {
            match self.inner.read(&mut buf[at..]) {
                Ok(0) if at == 0 => return Ok(false),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => at += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tag as it sits in a file: header, body, previous-tag-size trailer.
    pub(crate) fn tag_bytes(kind: u8, ts: u32, body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(body.len()).unwrap();
        let mut v = vec![kind];
        v.extend_from_slice(&len.to_be_bytes()[1..]);
        v.extend_from_slice(&ts.to_be_bytes()[1..]);
        v.push(ts.to_be_bytes()[0]);
        v.extend_from_slice(&[0, 0, 0]);
        v.extend_from_slice(body);
        v.extend_from_slice(&(len + 11).to_be_bytes());
        v
    }

    pub(crate) fn header_bytes() -> Vec<u8> {
        vec![b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0]
    }

    #[test]
    fn flv_reader_reads_tags() {
        // A hand-built three-tag stream, always.
        let mut file = header_bytes();
        file.extend(tag_bytes(TAG_VIDEO, 0, &[0x17, 0, 0, 0, 0, 1, 2, 3]));
        file.extend(tag_bytes(TAG_AUDIO, 0, &[0xaf, 0, 0x12, 0x10]));
        // A timestamp past 24 bits, to read the extension byte.
        file.extend(tag_bytes(TAG_VIDEO, 0x0123_4567, &[0x17, 1, 0, 0, 0, 9]));
        let mut r = FlvReader::new(&file[..]);
        let a = r.next_tag().unwrap().unwrap();
        assert_eq!(
            (a.kind, a.ts_ms, a.seq_header, a.keyframe),
            (TagKind::Video, 0, true, false)
        );
        assert_eq!(&a.data[..], &[0x17, 0, 0, 0, 0, 1, 2, 3]);
        let b = r.next_tag().unwrap().unwrap();
        assert_eq!((b.kind, b.seq_header), (TagKind::Audio, true));
        let c = r.next_tag().unwrap().unwrap();
        assert_eq!(
            (c.kind, c.ts_ms, c.keyframe, c.seq_header),
            (TagKind::Video, 0x0123_4567, true, false)
        );
        assert!(r.next_tag().unwrap().is_none());

        // A stream cut inside a tag is an error, not a clean end.
        let cut = &file[..file.len() - 3];
        let mut r = FlvReader::new(cut);
        for _ in 0..2 {
            r.next_tag().unwrap().unwrap();
        }
        assert_eq!(
            r.next_tag().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Not FLV at all.
        let mut r = FlvReader::new(&b"RIFF0000WAVE"[..]);
        assert_eq!(r.next_tag().unwrap_err().kind(), io::ErrorKind::InvalidData);

        // And the real thing, when this machine has ffmpeg.
        if let Some(ff) = crate::ffmpeg::ffmpeg_for_tests() {
            let out = std::process::Command::new(&ff.path)
                .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
                .args(["-f", "lavfi", "-i", "testsrc=size=64x64:rate=10"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
                .args(["-t", "1", "-c:v", "libx264", "-g", "5", "-c:a", "aac"])
                .args(["-f", "flv", "pipe:1"])
                .output()
                .unwrap();
            assert!(out.status.success(), "ffmpeg failed");
            let mut r = FlvReader::new(&out.stdout[..]);
            let mut tags = Vec::new();
            while let Some(t) = r.next_tag().unwrap() {
                tags.push(t);
            }
            assert_eq!(tags[0].kind, TagKind::Script, "onMetaData comes first");
            let first_video = tags.iter().find(|t| t.kind == TagKind::Video).unwrap();
            assert!(first_video.seq_header);
            let first_audio = tags.iter().find(|t| t.kind == TagKind::Audio).unwrap();
            assert!(first_audio.seq_header);
            let keys = tags.iter().filter(|t| t.keyframe).count();
            assert!(keys >= 2, "a key frame every 5 frames: {keys}");
            let last = tags.iter().rfind(|t| t.kind == TagKind::Video).unwrap();
            assert!((800..=1000).contains(&last.ts_ms), "{}", last.ts_ms);
        }
    }
}
