// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Default nicknames for new WebAuthn credentials, from the authenticator's
//! AAGUID: so a `YubiKey` shows up as "`YubiKey` 5" rather than "Security key".
//!
//! The table is the curated list from adif's `aaguid_registry.py`, ported as
//! data. An unknown or all-zero AAGUID (many platforms send zeros under
//! `attestation: "none"`) gets the generic name.

/// AAGUID → model, lower-case hyphenated.
const REGISTRY: &[(&str, &str)] = &[
    // Apple platform authenticators.
    (
        "dd4ec289-e01d-41c9-bb89-70fa845d4bf2",
        "Apple iCloud Keychain",
    ),
    (
        "fbfc3007-154e-4ecc-8c0b-6e020557d7bd",
        "Apple iCloud Keychain",
    ),
    // Feitian.
    (
        "833b721a-ff5f-4d00-bb2e-bdda3ec01e29",
        "Feitian ePass FIDO2 NFC",
    ),
    (
        "d91c5288-0ef0-49b7-b8ae-21ca0aa6b3f3",
        "Feitian BioPass FIDO2",
    ),
    (
        "3e22415d-7fdf-4ea4-8a0c-dd60c4249b9d",
        "Feitian iePass FIDO",
    ),
    // Google.
    (
        "ea9b8d66-4d01-1d21-3ce4-b6b48cb575d4",
        "Google Password Manager",
    ),
    // Microsoft.
    ("08987058-cadc-4b81-b6e1-30de50dcbe96", "Windows Hello"),
    ("6028b017-b1d4-4c02-b4b3-afcdafc96bb2", "Windows Hello"),
    ("9ddd1817-af5a-4672-a2b9-3e3dd95000a9", "Windows Hello"),
    // SoloKeys.
    ("8876631b-d4a0-427f-5773-0ec71c9e0279", "SoloKey Tap"),
    ("8b6b0cd0-2f3b-4f8d-9e08-b3aa72db5a39", "SoloKey 2"),
    // Yubico.
    ("ee882879-721c-4913-9775-3dfcce97072a", "YubiKey 5"),
    ("2fc0579f-8113-47ea-b116-bb5a8db9202a", "YubiKey 5 NFC"),
    (
        "cb69481e-8ff7-4039-93ec-0a2729a154a8",
        "YubiKey 5 NFC (FIPS)",
    ),
    ("fa2b99dc-9e39-4257-8f92-4a30d23c4118", "YubiKey 5 NFC"),
    ("c5ef55ff-ad9a-4b9f-b580-adebafe026d0", "YubiKey 5Ci"),
    ("85203421-48f9-4355-9bc8-8a53846e5083", "YubiKey 5Ci (FIPS)"),
    ("73bb0cd4-e502-49b8-9c6f-b59445bf720b", "YubiKey 5 (FIPS)"),
    ("f8a011f3-8c0a-4d15-8006-17111f9edc7d", "YubiKey Bio (FIDO)"),
    (
        "d8522d9f-575b-4866-88a9-ba99fa02f35b",
        "YubiKey Bio (Multi-protocol)",
    ),
    (
        "34744913-4f57-4e6e-a527-e9ec3c4b94e6",
        "YubiKey 5 NFC (FIPS, with PIN)",
    ),
];

/// The model name for an AAGUID, or `None` for an unknown or zero one.
#[must_use]
pub fn label(aaguid: &str) -> Option<&'static str> {
    let norm = aaguid.trim().to_ascii_lowercase();
    REGISTRY.iter().find(|(k, _)| *k == norm).map(|(_, v)| *v)
}

/// What registration's authenticator data says about the authenticator.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthInfo {
    /// Hyphenated lower-case AAGUID, `None` when absent or all zeros.
    pub aaguid: Option<String>,
    /// The backup-eligible flag (BE): a synced passkey.
    pub backup_eligible: bool,
    /// The backed-up flag (BS).
    pub backup_state: bool,
}

/// Read the AAGUID and backup flags from a CBOR attestation object
/// (`{"fmt": …, "attStmt": …, "authData": bytes}`). Anything malformed is
/// "unknown", never an error: a nickname is a convenience.
#[must_use]
pub fn from_attestation_object(att: &[u8]) -> AuthInfo {
    auth_data(att).map(parse_auth_data).unwrap_or_default()
}

fn parse_auth_data(ad: &[u8]) -> AuthInfo {
    // rpIdHash (32) · flags (1) · signCount (4) · [attestedCredentialData]
    let Some(&flags) = ad.get(32) else {
        return AuthInfo::default();
    };
    let backup_eligible = flags & 0x08 != 0;
    let backup_state = flags & 0x10 != 0;
    let aaguid = (flags & 0x40 != 0)
        .then(|| ad.get(37..53))
        .flatten()
        .filter(|g| g.iter().any(|b| *b != 0))
        .map(|g| {
            let h = super::session::hex(g);
            format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            )
        });
    AuthInfo {
        aaguid,
        backup_eligible,
        backup_state,
    }
}

/// The `authData` byte string of a top-level CBOR map.
fn auth_data(att: &[u8]) -> Option<&[u8]> {
    let mut r = Cbor { b: att, i: 0 };
    let (major, n) = r.head()?;
    if major != 5 {
        return None;
    }
    for _ in 0..n {
        let (km, kn) = r.head()?;
        let key = if km == 3 { r.take(kn)? } else { &[][..] };
        if km != 3 {
            r.skip_body(km, kn, 0)?;
        }
        if key == b"authData" {
            let (vm, vn) = r.head()?;
            return if vm == 2 { r.take(vn) } else { None };
        }
        r.skip(0)?;
    }
    None
}

/// A tiny definite-length CBOR reader: enough to skip any item.
struct Cbor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cbor<'a> {
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }

    fn take(&mut self, n: u64) -> Option<&'a [u8]> {
        let n = usize::try_from(n).ok()?;
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }

    /// Major type and argument.
    fn head(&mut self) -> Option<(u8, u64)> {
        let ib = self.byte()?;
        let major = ib >> 5;
        let info = ib & 0x1f;
        let arg = match info {
            0..=23 => u64::from(info),
            24 => u64::from(self.byte()?),
            25 => u64::from(u16::from_be_bytes(self.take(2)?.try_into().ok()?)),
            26 => u64::from(u32::from_be_bytes(self.take(4)?.try_into().ok()?)),
            27 => u64::from_be_bytes(self.take(8)?.try_into().ok()?),
            _ => return None, // indefinite lengths are not used by authenticators
        };
        Some((major, arg))
    }

    fn skip(&mut self, depth: u8) -> Option<()> {
        let (m, n) = self.head()?;
        self.skip_body(m, n, depth)
    }

    fn skip_body(&mut self, major: u8, n: u64, depth: u8) -> Option<()> {
        if depth > 16 {
            return None;
        }
        match major {
            0 | 1 | 7 => Some(()),
            2 | 3 => self.take(n).map(|_| ()),
            4 => (0..n).try_for_each(|_| self.skip(depth + 1)),
            5 => (0..n).try_for_each(|_| {
                self.skip(depth + 1)?;
                self.skip(depth + 1)
            }),
            6 => self.skip(depth + 1),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(flags: u8, aaguid: [u8; 16]) -> Vec<u8> {
        let mut ad = vec![0xAA; 32];
        ad.push(flags);
        ad.extend_from_slice(&[0, 0, 0, 1]);
        ad.extend_from_slice(&aaguid);
        ad.extend_from_slice(&[0, 2, 9, 9]);
        // {"fmt": "none", "attStmt": {}, "authData": h'…'}
        let mut v = vec![0xA3];
        v.push(0x63);
        v.extend_from_slice(b"fmt");
        v.push(0x64);
        v.extend_from_slice(b"none");
        v.push(0x67);
        v.extend_from_slice(b"attStmt");
        v.push(0xA0);
        v.push(0x68);
        v.extend_from_slice(b"authData");
        v.push(0x58);
        v.push(u8::try_from(ad.len()).unwrap());
        v.extend_from_slice(&ad);
        v
    }

    #[test]
    fn reads_the_aaguid_and_the_backup_flags() {
        let g = [
            0xee, 0x88, 0x28, 0x79, 0x72, 0x1c, 0x49, 0x13, 0x97, 0x75, 0x3d, 0xfc, 0xce, 0x97,
            0x07, 0x2a,
        ];
        let info = from_attestation_object(&att(0x45 | 0x08 | 0x10, g));
        assert_eq!(
            info.aaguid.as_deref(),
            Some("ee882879-721c-4913-9775-3dfcce97072a")
        );
        assert!(info.backup_eligible && info.backup_state);
        assert_eq!(label(info.aaguid.as_deref().unwrap()), Some("YubiKey 5"));
    }

    #[test]
    fn zeros_and_junk_are_unknown() {
        assert_eq!(from_attestation_object(&att(0x45, [0; 16])).aaguid, None);
        assert_eq!(
            from_attestation_object(b"\xff\x00junk"),
            AuthInfo::default()
        );
        assert_eq!(from_attestation_object(&[]), AuthInfo::default());
        assert_eq!(label("00000000-0000-0000-0000-000000000000"), None);
    }
}
