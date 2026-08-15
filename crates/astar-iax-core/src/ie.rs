// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Typed Information Element (IE) container.
//!
//! RFC 5456 §8 defines an IE as a 1-byte type, 1-byte length, then payload.
//! [`Ies`] holds one `Option` per defined IE type so callers can construct
//! frames declaratively without juggling a TLV stream.
//!
//! Parse is zero-copy: string and raw-byte IEs reference the input slice
//! directly. Encoding appends to any `bytes::BufMut` so the caller controls
//! allocation. Unknown IEs are surfaced as [`ParseError::UnknownIe`] —
//! callers that wish to tolerate unknown IEs can match on that error path
//! and retry with a skip-list once that's a documented requirement.

use bytes::BufMut;

use crate::error::{EncodeError, ParseError};

// IE id constants. Cross-checked against Asterisk
// `channels/iax2/include/iax2.h` and the jaracil Go reference. Where the
// vendored astar header stops (IE 51) we follow upstream Asterisk.
//
// Documented in the dialect doc: CALLTOKEN = 54.

pub const IE_CALLED_NUMBER: u8 = 1;
pub const IE_CALLING_NUMBER: u8 = 2;
pub const IE_CALLING_ANI: u8 = 3;
pub const IE_CALLING_NAME: u8 = 4;
pub const IE_CALLED_CONTEXT: u8 = 5;
pub const IE_USERNAME: u8 = 6;
pub const IE_PASSWORD: u8 = 7;
pub const IE_CAPABILITY: u8 = 8;
pub const IE_FORMAT: u8 = 9;
pub const IE_LANGUAGE: u8 = 10;
pub const IE_VERSION: u8 = 11;
pub const IE_ADSICPE: u8 = 12;
pub const IE_DNID: u8 = 13;
pub const IE_AUTHMETHODS: u8 = 14;
pub const IE_CHALLENGE: u8 = 15;
pub const IE_MD5_RESULT: u8 = 16;
pub const IE_RSA_RESULT: u8 = 17;
pub const IE_APPARENT_ADDR: u8 = 18;
pub const IE_REFRESH: u8 = 19;
pub const IE_DPSTATUS: u8 = 20;
pub const IE_CALLNO: u8 = 21;
pub const IE_CAUSE: u8 = 22;
pub const IE_IAX_UNKNOWN: u8 = 23;
pub const IE_MSGCOUNT: u8 = 24;
pub const IE_AUTOANSWER: u8 = 25;
pub const IE_MUSICONHOLD: u8 = 26;
pub const IE_TRANSFERID: u8 = 27;
pub const IE_RDNIS: u8 = 28;
pub const IE_PROVISIONING: u8 = 29;
pub const IE_AESPROVISIONING: u8 = 30;
pub const IE_DATETIME: u8 = 31;
pub const IE_DEVICETYPE: u8 = 32;
pub const IE_SERVICEIDENT: u8 = 33;
pub const IE_FIRMWAREVER: u8 = 34;
pub const IE_FWBLOCKDESC: u8 = 35;
pub const IE_FWBLOCKDATA: u8 = 36;
pub const IE_PROVVER: u8 = 37;
pub const IE_CALLINGPRES: u8 = 38;
pub const IE_CALLINGTON: u8 = 39;
pub const IE_CALLINGTNS: u8 = 40;
pub const IE_SAMPLINGRATE: u8 = 41;
pub const IE_CAUSECODE: u8 = 42;
pub const IE_ENCRYPTION: u8 = 43;
pub const IE_ENCKEY: u8 = 44;
pub const IE_CODEC_PREFS: u8 = 45;
pub const IE_RR_JITTER: u8 = 46;
pub const IE_RR_LOSS: u8 = 47;
pub const IE_RR_PKTS: u8 = 48;
pub const IE_RR_DELAY: u8 = 49;
pub const IE_RR_DROPPED: u8 = 50;
pub const IE_RR_OOO: u8 = 51;
pub const IE_VARIABLE: u8 = 52;
pub const IE_OSPTOKEN: u8 = 53;
/// RFC 5456 §8.6 anti-spoof token. Empty payload from a client means
/// "I support call tokens, please send me one"; populated means "here's the
/// token you gave me." See `docs/spec/iax2-conformance.md` 2026-05-29.
pub const IE_CALLTOKEN: u8 = 54;

/// Defensive upper bound on CALLTOKEN payload size. Asterisk's anti-spoof
/// token (RFC 5456 §8.6) is an HMAC-MD5 emitted as ASCII hex (32 bytes);
/// real-world tokens are well under 64 bytes. We cap at 128 to leave
/// margin for vendor extensions while still rejecting absurd payloads
/// that are either bugs or `DoS` attempts. The cap matters because
/// CALLTOKEN is security-bearing: it gates the second NEW frame, and
/// parse failures on it must surface even in lenient mode (iax-181f).
pub const MAX_CALLTOKEN_LEN: usize = 128;
pub const IE_CAPABILITY2: u8 = 55;
pub const IE_FORMAT2: u8 = 56;

/// Typed container of decoded IEs. All fields are independently optional.
///
/// String fields borrow as `&str` and validate UTF-8 at parse time
/// (Asterisk treats them as opaque C strings, but in 2026 every well-formed
/// payload we care about is ASCII; if a peer sends non-UTF-8 we reject with
/// [`ParseError::MalformedIe`] rather than silently dropping the IE).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ies<'a> {
    pub called_number: Option<&'a str>,
    pub calling_number: Option<&'a str>,
    pub calling_ani: Option<&'a str>,
    pub calling_name: Option<&'a str>,
    pub called_context: Option<&'a str>,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
    pub capability: Option<u32>,
    pub format: Option<u32>,
    pub language: Option<&'a str>,
    pub version: Option<u16>,
    pub adsicpe: Option<u16>,
    pub dnid: Option<&'a str>,
    pub authmethods: Option<u16>,
    pub challenge: Option<&'a str>,
    pub md5_result: Option<&'a str>,
    pub rsa_result: Option<&'a str>,
    /// `struct sockaddr_in` payload — kept raw for now (`TODO: typed accessor`).
    pub apparent_addr: Option<&'a [u8]>,
    pub refresh: Option<u16>,
    pub dpstatus: Option<u16>,
    pub callno: Option<u16>,
    pub cause: Option<&'a str>,
    pub iax_unknown: Option<u8>,
    pub msgcount: Option<u16>,
    /// `AUTOANSWER` IE carries a zero-length payload — its presence is the signal.
    pub autoanswer: bool,
    /// `MUSICONHOLD` carries either a zero-length payload (presence-only) or
    /// a moh-class string. We preserve the bytes as-is (`TODO: typed accessor`).
    pub musiconhold: Option<&'a [u8]>,
    pub transferid: Option<u32>,
    pub rdnis: Option<&'a str>,
    /// `TODO: typed accessor` — provisioning blob, opaque per Asterisk source.
    pub provisioning: Option<&'a [u8]>,
    /// `TODO: typed accessor` — AES provisioning bytes.
    pub aesprovisioning: Option<&'a [u8]>,
    pub datetime: Option<u32>,
    pub devicetype: Option<&'a str>,
    pub serviceident: Option<&'a str>,
    pub firmwarever: Option<u16>,
    pub fwblockdesc: Option<u32>,
    /// Firmware block of data, opaque bytes.
    pub fwblockdata: Option<&'a [u8]>,
    pub provver: Option<u32>,
    pub callingpres: Option<u8>,
    pub callington: Option<u8>,
    pub callingtns: Option<u16>,
    pub samplingrate: Option<u16>,
    pub causecode: Option<u8>,
    pub encryption: Option<u16>,
    /// Encryption key blob.
    pub enckey: Option<&'a [u8]>,
    /// Codec preference string — Asterisk encodes the prefs list as ASCII.
    pub codec_prefs: Option<&'a str>,
    pub rr_jitter: Option<u32>,
    pub rr_loss: Option<u32>,
    pub rr_pkts: Option<u32>,
    pub rr_delay: Option<u16>,
    pub rr_dropped: Option<u32>,
    pub rr_ooo: Option<u32>,
    /// `TODO: typed accessor` — variable=value pairs for dialplan globals.
    pub variable: Option<&'a [u8]>,
    /// `TODO: typed accessor` — OSP authentication token.
    pub osptoken: Option<&'a [u8]>,
    /// Empty bytes mean "I support call tokens"; populated bytes carry the
    /// opaque server-issued token. RFC 5456 §8.6.
    pub calltoken: Option<&'a [u8]>,
    pub capability2: Option<u64>,
    pub format2: Option<u64>,
}

impl<'a> Ies<'a> {
    /// Empty IE set — equivalent to `Ies::default()` but `const`-callable.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            called_number: None,
            calling_number: None,
            calling_ani: None,
            calling_name: None,
            called_context: None,
            username: None,
            password: None,
            capability: None,
            format: None,
            language: None,
            version: None,
            adsicpe: None,
            dnid: None,
            authmethods: None,
            challenge: None,
            md5_result: None,
            rsa_result: None,
            apparent_addr: None,
            refresh: None,
            dpstatus: None,
            callno: None,
            cause: None,
            iax_unknown: None,
            msgcount: None,
            autoanswer: false,
            musiconhold: None,
            transferid: None,
            rdnis: None,
            provisioning: None,
            aesprovisioning: None,
            datetime: None,
            devicetype: None,
            serviceident: None,
            firmwarever: None,
            fwblockdesc: None,
            fwblockdata: None,
            provver: None,
            callingpres: None,
            callington: None,
            callingtns: None,
            samplingrate: None,
            causecode: None,
            encryption: None,
            enckey: None,
            codec_prefs: None,
            rr_jitter: None,
            rr_loss: None,
            rr_pkts: None,
            rr_delay: None,
            rr_dropped: None,
            rr_ooo: None,
            variable: None,
            osptoken: None,
            calltoken: None,
            capability2: None,
            format2: None,
        }
    }

    /// Parse a TLV stream of IEs into a typed struct. Borrows from `input`.
    ///
    /// Per RFC 5456 §8 the wire layout is `<id:1><len:1><payload:len>`
    /// repeated. We surface a `ParseError::UnknownIe` for unrecognised ids
    /// so the caller can decide; tolerance is a state-machine policy, not a
    /// parser default.
    pub fn parse(mut input: &'a [u8]) -> Result<Self, ParseError> {
        let mut ies = Self::empty();
        while !input.is_empty() {
            if input.len() < 2 {
                return Err(ParseError::Truncated {
                    needed: 2,
                    available: input.len(),
                });
            }
            let id = input[0];
            let len = input[1] as usize;
            if input.len() < 2 + len {
                return Err(ParseError::Truncated {
                    needed: 2 + len,
                    available: input.len(),
                });
            }
            let payload = &input[2..2 + len];
            input = &input[2 + len..];
            apply_ie(&mut ies, id, payload)?;
        }
        Ok(ies)
    }

    /// Lenient variant: skip any IE whose id or payload we do not recognise
    /// instead of failing the whole frame. Useful when consuming traffic
    /// from a real peer that may carry vendor-extension IEs we don't model
    /// yet (e.g., Asterisk 20's variable-length `IE_FORMAT2`).
    ///
    /// `Truncated` errors are still hard errors — those mean the framing
    /// itself is broken, not a single IE.
    pub fn parse_lenient(mut input: &'a [u8]) -> Result<Self, ParseError> {
        let mut ies = Self::empty();
        while !input.is_empty() {
            if input.len() < 2 {
                return Err(ParseError::Truncated {
                    needed: 2,
                    available: input.len(),
                });
            }
            let id = input[0];
            let len = input[1] as usize;
            if input.len() < 2 + len {
                return Err(ParseError::Truncated {
                    needed: 2 + len,
                    available: input.len(),
                });
            }
            let payload = &input[2..2 + len];
            input = &input[2 + len..];
            // Swallow per-IE errors so a single unknown/malformed IE doesn't
            // tank the whole frame — EXCEPT for security-bearing IEs, where
            // silent skip masks both bugs and attacks. See iax-181f and
            // `is_strict_in_lenient_mode` for the rationale.
            if let Err(e) = apply_ie(&mut ies, id, payload)
                && is_strict_in_lenient_mode(id)
            {
                return Err(e);
            }
        }
        Ok(ies)
    }

    /// Encode this IE set into a `BufMut`. Order: ascending by IE id, which
    /// matches the canonical order Asterisk emits and lets equality tests
    /// against captured frames work byte-for-byte.
    ///
    /// Returns [`EncodeError::IeTooLong`] when any string- or byte-IE
    /// payload exceeds 255 bytes — the maximum representable by the
    /// 1-byte length prefix (RFC 5456 §8). Numeric IEs are fixed-width
    /// and cannot overflow. Per iax-545b we reject oversize payloads
    /// instead of silently truncating, which previously could split a
    /// multi-byte UTF-8 codepoint and produce wire bytes that the parser
    /// itself rejected. Callers that want truncation should shorten by
    /// codepoint (`str::char_indices`) before calling `encode`.
    ///
    /// On error the output buffer may already contain partially-written
    /// IEs (those that fit and came earlier in ascending-id order); the
    /// caller is expected to discard `out` on error.
    #[allow(clippy::too_many_lines)]
    pub fn encode(&self, out: &mut impl BufMut) -> Result<(), EncodeError> {
        write_str(out, IE_CALLED_NUMBER, self.called_number)?;
        write_str(out, IE_CALLING_NUMBER, self.calling_number)?;
        write_str(out, IE_CALLING_ANI, self.calling_ani)?;
        write_str(out, IE_CALLING_NAME, self.calling_name)?;
        write_str(out, IE_CALLED_CONTEXT, self.called_context)?;
        write_str(out, IE_USERNAME, self.username)?;
        write_str(out, IE_PASSWORD, self.password)?;
        write_u32(out, IE_CAPABILITY, self.capability);
        write_u32(out, IE_FORMAT, self.format);
        write_str(out, IE_LANGUAGE, self.language)?;
        write_u16(out, IE_VERSION, self.version);
        write_u16(out, IE_ADSICPE, self.adsicpe);
        write_str(out, IE_DNID, self.dnid)?;
        write_u16(out, IE_AUTHMETHODS, self.authmethods);
        write_str(out, IE_CHALLENGE, self.challenge)?;
        write_str(out, IE_MD5_RESULT, self.md5_result)?;
        write_str(out, IE_RSA_RESULT, self.rsa_result)?;
        write_bytes(out, IE_APPARENT_ADDR, self.apparent_addr)?;
        write_u16(out, IE_REFRESH, self.refresh);
        write_u16(out, IE_DPSTATUS, self.dpstatus);
        write_u16(out, IE_CALLNO, self.callno);
        write_str(out, IE_CAUSE, self.cause)?;
        write_u8(out, IE_IAX_UNKNOWN, self.iax_unknown);
        write_u16(out, IE_MSGCOUNT, self.msgcount);
        if self.autoanswer {
            out.put_u8(IE_AUTOANSWER);
            out.put_u8(0);
        }
        write_bytes(out, IE_MUSICONHOLD, self.musiconhold)?;
        write_u32(out, IE_TRANSFERID, self.transferid);
        write_str(out, IE_RDNIS, self.rdnis)?;
        write_bytes(out, IE_PROVISIONING, self.provisioning)?;
        write_bytes(out, IE_AESPROVISIONING, self.aesprovisioning)?;
        write_u32(out, IE_DATETIME, self.datetime);
        write_str(out, IE_DEVICETYPE, self.devicetype)?;
        write_str(out, IE_SERVICEIDENT, self.serviceident)?;
        write_u16(out, IE_FIRMWAREVER, self.firmwarever);
        write_u32(out, IE_FWBLOCKDESC, self.fwblockdesc);
        write_bytes(out, IE_FWBLOCKDATA, self.fwblockdata)?;
        write_u32(out, IE_PROVVER, self.provver);
        write_u8(out, IE_CALLINGPRES, self.callingpres);
        write_u8(out, IE_CALLINGTON, self.callington);
        write_u16(out, IE_CALLINGTNS, self.callingtns);
        write_u16(out, IE_SAMPLINGRATE, self.samplingrate);
        write_u8(out, IE_CAUSECODE, self.causecode);
        write_u16(out, IE_ENCRYPTION, self.encryption);
        write_bytes(out, IE_ENCKEY, self.enckey)?;
        write_str(out, IE_CODEC_PREFS, self.codec_prefs)?;
        write_u32(out, IE_RR_JITTER, self.rr_jitter);
        write_u32(out, IE_RR_LOSS, self.rr_loss);
        write_u32(out, IE_RR_PKTS, self.rr_pkts);
        write_u16(out, IE_RR_DELAY, self.rr_delay);
        write_u32(out, IE_RR_DROPPED, self.rr_dropped);
        write_u32(out, IE_RR_OOO, self.rr_ooo);
        write_bytes(out, IE_VARIABLE, self.variable)?;
        write_bytes(out, IE_OSPTOKEN, self.osptoken)?;
        write_bytes(out, IE_CALLTOKEN, self.calltoken)?;
        write_u64(out, IE_CAPABILITY2, self.capability2);
        write_u64(out, IE_FORMAT2, self.format2);
        Ok(())
    }
}

/// Returns `true` if the given IE id is considered "security-bearing" and
/// therefore must NOT be silently skipped on a per-IE parse failure, even
/// in lenient mode (iax-181f).
///
/// The list gates the IAX2 auth and transport-security handshakes. A
/// silently-skipped malformed value on any of these masks both genuine peer
/// bugs and adversarial framing, and — worse — can cause a downgrade or
/// bypass because downstream consumers treat a missing IE as "absent /
/// default" rather than "rejected":
///
/// - `IE_CALLTOKEN` (RFC 5456 §8.6 anti-spoof token): dropping it silently
///   makes the FSM in `NewSent` reply with nothing and time out.
/// - `IE_CHALLENGE` (§8.7 auth nonce): the digest is computed over this
///   value. Outbound handlers fall back to `ies.challenge.unwrap_or("")`,
///   so a silently-dropped malformed `CHALLENGE` makes the client sign
///   `md5("" || password)` — a wrong, attacker-influenceable digest.
/// - `IE_MD5_RESULT` / `IE_RSA_RESULT` (§8.9/§8.10 auth responses): the
///   server validates the call against these. Skipping a malformed one
///   makes the response *vanish*, masking a failed or forged authentication.
/// - `IE_PASSWORD` (§8.6 plaintext credential): gates auth directly.
/// - `IE_ENCRYPTION` (§8.30 encryption method select): a silently-dropped
///   malformed value reads back as "no encryption requested" — a cleartext
///   downgrade.
///
/// `IE_ENCKEY` is *not* on this list: it is parsed as opaque raw bytes with
/// no malformed-payload error path, so there is nothing to propagate. Adding
/// it would be inert. The remaining application IEs stay lenient on purpose —
/// see `parse_lenient` — so we keep talking to quirky legacy peers.
fn is_strict_in_lenient_mode(id: u8) -> bool {
    matches!(
        id,
        IE_CALLTOKEN | IE_CHALLENGE | IE_MD5_RESULT | IE_RSA_RESULT | IE_PASSWORD | IE_ENCRYPTION
    )
}

#[allow(clippy::too_many_lines)]
fn apply_ie<'a>(ies: &mut Ies<'a>, id: u8, payload: &'a [u8]) -> Result<(), ParseError> {
    match id {
        IE_CALLED_NUMBER => ies.called_number = Some(parse_str(id, payload)?),
        IE_CALLING_NUMBER => ies.calling_number = Some(parse_str(id, payload)?),
        IE_CALLING_ANI => ies.calling_ani = Some(parse_str(id, payload)?),
        IE_CALLING_NAME => ies.calling_name = Some(parse_str(id, payload)?),
        IE_CALLED_CONTEXT => ies.called_context = Some(parse_str(id, payload)?),
        IE_USERNAME => ies.username = Some(parse_str(id, payload)?),
        IE_PASSWORD => ies.password = Some(parse_str(id, payload)?),
        IE_CAPABILITY => ies.capability = Some(parse_u32(id, payload)?),
        IE_FORMAT => ies.format = Some(parse_u32(id, payload)?),
        IE_LANGUAGE => ies.language = Some(parse_str(id, payload)?),
        IE_VERSION => ies.version = Some(parse_u16(id, payload)?),
        IE_ADSICPE => ies.adsicpe = Some(parse_u16(id, payload)?),
        IE_DNID => ies.dnid = Some(parse_str(id, payload)?),
        IE_AUTHMETHODS => ies.authmethods = Some(parse_u16(id, payload)?),
        IE_CHALLENGE => ies.challenge = Some(parse_str(id, payload)?),
        IE_MD5_RESULT => ies.md5_result = Some(parse_str(id, payload)?),
        IE_RSA_RESULT => ies.rsa_result = Some(parse_str(id, payload)?),
        IE_APPARENT_ADDR => ies.apparent_addr = Some(payload),
        IE_REFRESH => ies.refresh = Some(parse_u16(id, payload)?),
        IE_DPSTATUS => ies.dpstatus = Some(parse_u16(id, payload)?),
        IE_CALLNO => ies.callno = Some(parse_u16(id, payload)?),
        IE_CAUSE => ies.cause = Some(parse_str(id, payload)?),
        IE_IAX_UNKNOWN => ies.iax_unknown = Some(parse_u8(id, payload)?),
        IE_MSGCOUNT => ies.msgcount = Some(parse_u16(id, payload)?),
        IE_AUTOANSWER => {
            if !payload.is_empty() {
                return Err(ParseError::MalformedIe {
                    ie: id,
                    reason: "AUTOANSWER must be zero-length",
                });
            }
            ies.autoanswer = true;
        }
        IE_MUSICONHOLD => ies.musiconhold = Some(payload),
        IE_TRANSFERID => ies.transferid = Some(parse_u32(id, payload)?),
        IE_RDNIS => ies.rdnis = Some(parse_str(id, payload)?),
        IE_PROVISIONING => ies.provisioning = Some(payload),
        IE_AESPROVISIONING => ies.aesprovisioning = Some(payload),
        IE_DATETIME => ies.datetime = Some(parse_u32(id, payload)?),
        IE_DEVICETYPE => ies.devicetype = Some(parse_str(id, payload)?),
        IE_SERVICEIDENT => ies.serviceident = Some(parse_str(id, payload)?),
        IE_FIRMWAREVER => ies.firmwarever = Some(parse_u16(id, payload)?),
        IE_FWBLOCKDESC => ies.fwblockdesc = Some(parse_u32(id, payload)?),
        IE_FWBLOCKDATA => ies.fwblockdata = Some(payload),
        IE_PROVVER => ies.provver = Some(parse_u32(id, payload)?),
        IE_CALLINGPRES => ies.callingpres = Some(parse_u8(id, payload)?),
        IE_CALLINGTON => ies.callington = Some(parse_u8(id, payload)?),
        IE_CALLINGTNS => ies.callingtns = Some(parse_u16(id, payload)?),
        IE_SAMPLINGRATE => ies.samplingrate = Some(parse_u16(id, payload)?),
        IE_CAUSECODE => ies.causecode = Some(parse_u8(id, payload)?),
        IE_ENCRYPTION => ies.encryption = Some(parse_u16(id, payload)?),
        IE_ENCKEY => ies.enckey = Some(payload),
        IE_CODEC_PREFS => ies.codec_prefs = Some(parse_str(id, payload)?),
        IE_RR_JITTER => ies.rr_jitter = Some(parse_u32(id, payload)?),
        IE_RR_LOSS => ies.rr_loss = Some(parse_u32(id, payload)?),
        IE_RR_PKTS => ies.rr_pkts = Some(parse_u32(id, payload)?),
        IE_RR_DELAY => ies.rr_delay = Some(parse_u16(id, payload)?),
        IE_RR_DROPPED => ies.rr_dropped = Some(parse_u32(id, payload)?),
        IE_RR_OOO => ies.rr_ooo = Some(parse_u32(id, payload)?),
        IE_VARIABLE => ies.variable = Some(payload),
        IE_OSPTOKEN => ies.osptoken = Some(payload),
        IE_CALLTOKEN => {
            if payload.len() > MAX_CALLTOKEN_LEN {
                return Err(ParseError::MalformedIe {
                    ie: id,
                    reason: "CALLTOKEN payload exceeds MAX_CALLTOKEN_LEN",
                });
            }
            ies.calltoken = Some(payload);
        }
        IE_CAPABILITY2 => ies.capability2 = Some(parse_u64(id, payload)?),
        IE_FORMAT2 => ies.format2 = Some(parse_u64(id, payload)?),
        other => return Err(ParseError::UnknownIe(other)),
    }
    Ok(())
}

// --- Payload helpers ------------------------------------------------------

fn parse_str(id: u8, payload: &[u8]) -> Result<&str, ParseError> {
    std::str::from_utf8(payload).map_err(|_| ParseError::MalformedIe {
        ie: id,
        reason: "string IE payload is not valid UTF-8",
    })
}

fn parse_u8(id: u8, payload: &[u8]) -> Result<u8, ParseError> {
    match payload {
        [b] => Ok(*b),
        _ => Err(ParseError::MalformedIe {
            ie: id,
            reason: "u8 IE payload must be exactly 1 byte",
        }),
    }
}

fn parse_u16(id: u8, payload: &[u8]) -> Result<u16, ParseError> {
    let bytes: [u8; 2] = payload.try_into().map_err(|_| ParseError::MalformedIe {
        ie: id,
        reason: "u16 IE payload must be exactly 2 bytes",
    })?;
    Ok(u16::from_be_bytes(bytes))
}

fn parse_u32(id: u8, payload: &[u8]) -> Result<u32, ParseError> {
    let bytes: [u8; 4] = payload.try_into().map_err(|_| ParseError::MalformedIe {
        ie: id,
        reason: "u32 IE payload must be exactly 4 bytes",
    })?;
    Ok(u32::from_be_bytes(bytes))
}

fn parse_u64(id: u8, payload: &[u8]) -> Result<u64, ParseError> {
    let bytes: [u8; 8] = payload.try_into().map_err(|_| ParseError::MalformedIe {
        ie: id,
        reason: "u64 IE payload must be exactly 8 bytes",
    })?;
    Ok(u64::from_be_bytes(bytes))
}

#[allow(clippy::cast_possible_truncation)]
fn write_str(out: &mut impl BufMut, id: u8, value: Option<&str>) -> Result<(), EncodeError> {
    if let Some(v) = value {
        // String IEs are length-prefixed by a u8 (RFC 5456 §8); the wire
        // format cannot represent payloads > 255 bytes. iax-545b: previously
        // we silently clamped to 255 here, which could split a multi-byte
        // UTF-8 codepoint mid-sequence and make the encoder emit a frame
        // that failed self-round-trip through `parse_str`. Now we surface
        // a typed error and let the caller shorten the value (by
        // codepoint, e.g. `s.char_indices()`) or propagate.
        let bytes = v.as_bytes();
        if bytes.len() > 255 {
            return Err(EncodeError::IeTooLong {
                ie: id,
                len: bytes.len(),
            });
        }
        out.put_u8(id);
        out.put_u8(bytes.len() as u8);
        out.put_slice(bytes);
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn write_bytes(out: &mut impl BufMut, id: u8, value: Option<&[u8]>) -> Result<(), EncodeError> {
    if let Some(v) = value {
        if v.len() > 255 {
            return Err(EncodeError::IeTooLong {
                ie: id,
                len: v.len(),
            });
        }
        out.put_u8(id);
        out.put_u8(v.len() as u8);
        out.put_slice(v);
    }
    Ok(())
}

fn write_u8(out: &mut impl BufMut, id: u8, value: Option<u8>) {
    if let Some(v) = value {
        out.put_u8(id);
        out.put_u8(1);
        out.put_u8(v);
    }
}

fn write_u16(out: &mut impl BufMut, id: u8, value: Option<u16>) {
    if let Some(v) = value {
        out.put_u8(id);
        out.put_u8(2);
        out.put_u16(v);
    }
}

fn write_u32(out: &mut impl BufMut, id: u8, value: Option<u32>) {
    if let Some(v) = value {
        out.put_u8(id);
        out.put_u8(4);
        out.put_u32(v);
    }
}

fn write_u64(out: &mut impl BufMut, id: u8, value: Option<u64>) {
    if let Some(v) = value {
        out.put_u8(id);
        out.put_u8(8);
        out.put_u64(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_username_and_format() {
        let ies = Ies {
            username: Some("rob"),
            format: Some(0x4),
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        ies.encode(&mut buf).unwrap();
        let parsed = Ies::parse(&buf).unwrap();
        assert_eq!(parsed.username, Some("rob"));
        assert_eq!(parsed.format, Some(4));
    }

    #[test]
    fn empty_calltoken_round_trips() {
        // Per the conformance doc: an empty CALLTOKEN IE is the client's
        // signal that it supports call tokens. The wire shape is id=54 len=0.
        let ies = Ies {
            calltoken: Some(&[]),
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        ies.encode(&mut buf).unwrap();
        assert_eq!(buf, vec![IE_CALLTOKEN, 0]);
        let parsed = Ies::parse(&buf).unwrap();
        assert_eq!(parsed.calltoken, Some(&[][..]));
    }

    #[test]
    fn unknown_ie_is_surfaced() {
        let buf = [99u8, 1, 0];
        let err = Ies::parse(&buf).unwrap_err();
        assert!(matches!(err, ParseError::UnknownIe(99)));
    }

    #[test]
    fn truncated_ie_is_surfaced() {
        let buf = [IE_USERNAME, 5, b'r', b'o', b'b'];
        let err = Ies::parse(&buf).unwrap_err();
        assert!(matches!(err, ParseError::Truncated { .. }));
    }

    #[test]
    fn autoanswer_is_presence_only() {
        let ies = Ies {
            autoanswer: true,
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        ies.encode(&mut buf).unwrap();
        assert_eq!(buf, vec![IE_AUTOANSWER, 0]);
        let parsed = Ies::parse(&buf).unwrap();
        assert!(parsed.autoanswer);
    }

    #[test]
    fn malformed_u16_rejected() {
        // VERSION (11) expects 2 bytes; supply 1.
        let buf = [IE_VERSION, 1, 0];
        let err = Ies::parse(&buf).unwrap_err();
        assert!(matches!(
            err,
            ParseError::MalformedIe { ie: IE_VERSION, .. }
        ));
    }

    /// iax-181f: lenient mode must NOT swallow per-IE errors for security-
    /// bearing IEs like CALLTOKEN. A malformed CALLTOKEN — here, an oversize
    /// payload that violates the max-size guard — must propagate as
    /// `MalformedIe` rather than being silently skipped. Otherwise the FSM
    /// in `NewSent` would see no `calltoken` IE and reply with nothing,
    /// then time out (or worse, accept an attacker-crafted handshake).
    #[test]
    fn malformed_calltoken_propagated_in_lenient_mode() {
        // CALLTOKEN payload of 129 bytes exceeds MAX_CALLTOKEN_LEN (128).
        let mut buf = vec![IE_CALLTOKEN, 129];
        buf.extend(std::iter::repeat_n(0xAAu8, 129));
        let err = Ies::parse_lenient(&buf)
            .expect_err("oversize CALLTOKEN must error in lenient mode (security-bearing IE)");
        assert!(
            matches!(
                err,
                ParseError::MalformedIe {
                    ie: IE_CALLTOKEN,
                    ..
                }
            ),
            "expected MalformedIe for IE_CALLTOKEN, got {err:?}",
        );
    }

    /// iax-181f: lenient mode's whole purpose is tolerating malformed
    /// *application* IEs from legacy peers. A malformed non-security IE
    /// (here, USERNAME with non-UTF-8 payload) must still be silently
    /// skipped so we keep talking to quirky peers. Pin both halves so a
    /// future "make everything strict" regression doesn't slip through.
    #[test]
    fn malformed_non_security_ie_skipped_in_lenient_mode() {
        // USERNAME with invalid UTF-8 (lone continuation byte 0x80).
        let buf = [IE_USERNAME, 1, 0x80];
        let ies = Ies::parse_lenient(&buf)
            .expect("malformed application IE must be skipped in lenient mode");
        assert_eq!(
            ies.username, None,
            "malformed USERNAME payload should be skipped, leaving field unset",
        );
        // Strict parse of the same input must still reject it — sanity
        // check that the test fixture is actually malformed.
        assert!(matches!(
            Ies::parse(&buf).unwrap_err(),
            ParseError::MalformedIe {
                ie: IE_USERNAME,
                ..
            }
        ));
    }

    // iax-1741: audit follow-up to iax-181f. The CHALLENGE/MD5_RESULT/
    // RSA_RESULT/PASSWORD/ENCRYPTION IEs all gate the auth or transport-
    // security handshake, so a malformed payload on any of them must
    // propagate even in lenient mode rather than being silently skipped
    // (which downstream consumers would read as "absent / default" and so
    // downgrade or bypass the handshake). Each IE gets the iax-181f
    // two-test shape: malformed propagates, well-formed still parses.

    /// iax-1741: a malformed `CHALLENGE` (non-UTF-8) must propagate in lenient
    /// mode. Otherwise outbound handlers fall back to `unwrap_or("")` and
    /// sign `md5("" || password)` — a wrong, attacker-influenceable digest.
    #[test]
    fn malformed_challenge_propagated_in_lenient_mode() {
        let buf = [IE_CHALLENGE, 1, 0x80]; // lone UTF-8 continuation byte
        let err = Ies::parse_lenient(&buf)
            .expect_err("malformed CHALLENGE must error in lenient mode (security-bearing IE)");
        assert!(
            matches!(
                err,
                ParseError::MalformedIe {
                    ie: IE_CHALLENGE,
                    ..
                }
            ),
            "expected MalformedIe for IE_CHALLENGE, got {err:?}",
        );
    }

    /// iax-1741: a well-formed `CHALLENGE` must still parse in lenient mode.
    #[test]
    fn well_formed_challenge_parses_in_lenient_mode() {
        let buf = [IE_CHALLENGE, 6, b'c', b'0', b'f', b'f', b'e', b'e'];
        let ies =
            Ies::parse_lenient(&buf).expect("well-formed CHALLENGE must parse in lenient mode");
        assert_eq!(ies.challenge, Some("c0ffee"));
    }

    /// iax-1741: a malformed `MD5_RESULT` (non-UTF-8) must propagate in lenient
    /// mode — skipping it makes a failed/forged auth response vanish.
    #[test]
    fn malformed_md5_result_propagated_in_lenient_mode() {
        let buf = [IE_MD5_RESULT, 1, 0x80];
        let err = Ies::parse_lenient(&buf)
            .expect_err("malformed MD5_RESULT must error in lenient mode (security-bearing IE)");
        assert!(
            matches!(
                err,
                ParseError::MalformedIe {
                    ie: IE_MD5_RESULT,
                    ..
                }
            ),
            "expected MalformedIe for IE_MD5_RESULT, got {err:?}",
        );
    }

    /// iax-1741: a well-formed `MD5_RESULT` must still parse in lenient mode.
    #[test]
    fn well_formed_md5_result_parses_in_lenient_mode() {
        let digest = "0123456789abcdef0123456789abcdef"; // 32 bytes
        let mut buf = vec![IE_MD5_RESULT, 32];
        buf.extend_from_slice(digest.as_bytes());
        let ies =
            Ies::parse_lenient(&buf).expect("well-formed MD5_RESULT must parse in lenient mode");
        assert_eq!(ies.md5_result, Some(digest));
    }

    /// iax-1741: a malformed `RSA_RESULT` (non-UTF-8) must propagate in lenient
    /// mode for the same reason as `MD5_RESULT`.
    #[test]
    fn malformed_rsa_result_propagated_in_lenient_mode() {
        let buf = [IE_RSA_RESULT, 1, 0x80];
        let err = Ies::parse_lenient(&buf)
            .expect_err("malformed RSA_RESULT must error in lenient mode (security-bearing IE)");
        assert!(
            matches!(
                err,
                ParseError::MalformedIe {
                    ie: IE_RSA_RESULT,
                    ..
                }
            ),
            "expected MalformedIe for IE_RSA_RESULT, got {err:?}",
        );
    }

    /// iax-1741: a well-formed `RSA_RESULT` must still parse in lenient mode.
    #[test]
    fn well_formed_rsa_result_parses_in_lenient_mode() {
        let buf = [IE_RSA_RESULT, 4, b'r', b's', b'a', b'1'];
        let ies =
            Ies::parse_lenient(&buf).expect("well-formed RSA_RESULT must parse in lenient mode");
        assert_eq!(ies.rsa_result, Some("rsa1"));
    }

    /// iax-1741: a malformed `PASSWORD` (non-UTF-8) must propagate in lenient
    /// mode — it gates auth directly.
    #[test]
    fn malformed_password_propagated_in_lenient_mode() {
        let buf = [IE_PASSWORD, 1, 0x80];
        let err = Ies::parse_lenient(&buf)
            .expect_err("malformed PASSWORD must error in lenient mode (security-bearing IE)");
        assert!(
            matches!(
                err,
                ParseError::MalformedIe {
                    ie: IE_PASSWORD,
                    ..
                }
            ),
            "expected MalformedIe for IE_PASSWORD, got {err:?}",
        );
    }

    /// iax-1741: a well-formed `PASSWORD` must still parse in lenient mode.
    #[test]
    fn well_formed_password_parses_in_lenient_mode() {
        let buf = [IE_PASSWORD, 6, b's', b'e', b'c', b'r', b'e', b't'];
        let ies =
            Ies::parse_lenient(&buf).expect("well-formed PASSWORD must parse in lenient mode");
        assert_eq!(ies.password, Some("secret"));
    }

    /// iax-1741: a malformed `ENCRYPTION` (wrong length — u16 IE given 1 byte)
    /// must propagate in lenient mode. Silently skipping it reads back as
    /// "no encryption requested" — a cleartext downgrade.
    #[test]
    fn malformed_encryption_propagated_in_lenient_mode() {
        let buf = [IE_ENCRYPTION, 1, 0x01]; // u16 IE needs exactly 2 bytes
        let err = Ies::parse_lenient(&buf)
            .expect_err("malformed ENCRYPTION must error in lenient mode (security-bearing IE)");
        assert!(
            matches!(
                err,
                ParseError::MalformedIe {
                    ie: IE_ENCRYPTION,
                    ..
                }
            ),
            "expected MalformedIe for IE_ENCRYPTION, got {err:?}",
        );
    }

    /// iax-1741: a well-formed `ENCRYPTION` must still parse in lenient mode.
    #[test]
    fn well_formed_encryption_parses_in_lenient_mode() {
        let buf = [IE_ENCRYPTION, 2, 0x00, 0x01]; // AES128 = 1
        let ies =
            Ies::parse_lenient(&buf).expect("well-formed ENCRYPTION must parse in lenient mode");
        assert_eq!(ies.encryption, Some(1));
    }

    // iax-545b: string IEs > 255 bytes used to be silently truncated by the
    // encoder. The truncation could split a multi-byte UTF-8 codepoint, so
    // `parse_str` would then reject the encoder's own output as
    // `MalformedIe`. After the fix, `encode` reports the oversize as a typed
    // `EncodeError` instead of producing an unparseable frame.
    #[test]
    fn encoding_oversized_string_returns_error() {
        let long = "a".repeat(256);
        let ies = Ies {
            username: Some(&long),
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        let err = ies.encode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::EncodeError::IeTooLong {
                ie: IE_USERNAME,
                len: 256
            }
        ));
    }

    #[test]
    fn encoding_oversized_bytes_returns_error() {
        let long = vec![0u8; 300];
        let ies = Ies {
            calltoken: Some(&long),
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        let err = ies.encode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::EncodeError::IeTooLong {
                ie: IE_CALLTOKEN,
                len: 300
            }
        ));
    }

    // The specific shape from the review: a multi-byte UTF-8 codepoint
    // straddles byte 255 of the payload. The old encoder would slice the
    // first 255 bytes — splitting the codepoint mid-sequence — and the
    // resulting buffer would fail self-round-trip via `parse_str`. Pin the
    // new contract: encode errors out rather than emitting unparseable
    // bytes.
    #[test]
    fn encoding_string_with_codepoint_straddling_255_returns_error() {
        // 254 ASCII bytes + one 4-byte codepoint = 258 bytes; bytes 255,
        // 256, 257 are continuation bytes of the codepoint. Old encoder
        // would emit [0..255), keeping the lead byte and dropping the
        // three continuations, producing invalid UTF-8 on the wire.
        let mut s = "a".repeat(254);
        s.push('\u{1F600}'); // 4 UTF-8 bytes
        assert_eq!(s.len(), 258);
        let ies = Ies {
            calling_name: Some(&s),
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        let err = ies.encode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::EncodeError::IeTooLong {
                ie: IE_CALLING_NAME,
                len: 258
            }
        ));
    }

    // Round-trip property: any value `encode()` accepts must `parse()` back
    // to the same value. Covers boundary length 255 (max permitted) plus a
    // mix of short fields.
    #[test]
    fn encode_then_parse_round_trip_at_boundary() {
        let max_str = "x".repeat(255);
        // CALLTOKEN has a tighter cap (MAX_CALLTOKEN_LEN = 128, iax-181f);
        // 128 still exercises a multi-byte length boundary.
        let max_bytes = vec![0xAB; 128];
        let ies = Ies {
            username: Some(&max_str),
            calltoken: Some(&max_bytes),
            capability: Some(0xDEAD_BEEF),
            version: Some(2),
            ..Ies::empty()
        };
        let mut buf = Vec::new();
        ies.encode(&mut buf)
            .expect("255-byte IE values must encode");
        let parsed = Ies::parse(&buf).expect("encoder output must round-trip");
        assert_eq!(parsed.username.unwrap(), max_str);
        assert_eq!(parsed.calltoken.unwrap(), max_bytes.as_slice());
        assert_eq!(parsed.capability, Some(0xDEAD_BEEF));
        assert_eq!(parsed.version, Some(2));
    }
}
