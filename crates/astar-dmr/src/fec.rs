// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The block codes DMR's signalling is wrapped in, written from their
//! definitions.
//!
//! Five codes and a checksum: BPTC(196,96) for the full LC and every
//! signalling burst, BPTC(128,77) for the embedded LC, Golay(20,8) for the
//! slot type, QR(16,7,6) for the EMB, RS(12,9) for the full LC's parity, and
//! the five-bit sum that guards the embedded LC. `docs/design/dmr-wire.md` §8
//! gives each one by its parameters — which parity equations, which generator
//! polynomial, which field, which interleave — and this module is those
//! parameters turned into arithmetic.
//!
//! **Derived, not transcribed.** The parameters were read out of
//! `g4klx/MMDVMHost` (GPL-2.0) as a specification of the wire: `Hamming.cpp`,
//! `BPTC19696.cpp`, `DMREmbeddedData.cpp`, `Golay2087.cpp`, `QR1676.cpp`,
//! `RS129.cpp` and `CRC.cpp`. Every function below names the file and function
//! its parameters came from. What is *not* here is any of that project's code
//! or any of its tables: `Golay2087.cpp` and `QR1676.cpp` are, between them,
//! 384 pre-computed codewords, and this module computes the same numbers by
//! long division from the generator polynomial instead. astar is
//! AGPL-3.0-only and the licences do not mix, which is the practical reason;
//! the better one is that a table says what the answer is and a polynomial
//! says why. The tests pin a handful of codewords that were computed a second
//! time, independently, before they were written down.
//!
//! Nothing here does I/O, allocates on a hot path, or knows what a burst is.
//! Where a burst layout is unavoidable — BPTC(196,96) writes into a burst's
//! two information halves and must not touch the sync field between them —
//! the offsets are stated with their citation and pinned by a test.
//!
//! # Refusing is a feature
//!
//! Every decoder here returns `None` rather than a best guess when the damage
//! is past what the code can repair. A wrong talker id or a wrong slot type
//! that is *presented as right* is worse than nothing at all: the operator
//! reads it, believes it, and answers a station that never called.

use std::sync::OnceLock;

// ── Hamming ─────────────────────────────────────────────────────────────────

/// The four parity equations shared by Hamming (15,11,3) and (16,11,4), as
/// index sets over the eleven data bits.
///
/// From `MMDVMHost/Hamming.cpp`: `CHamming::encode15113_2` — the second of
/// that file's two (15,11,3) variants, and the one `BPTC19696.cpp` uses for
/// its rows. Written as sets because that is what a parity equation is; the
/// reference writes the same four as unrolled XOR chains.
const EQ0: &[usize] = &[0, 1, 2, 3, 5, 7, 8];
const EQ1: &[usize] = &[1, 2, 3, 4, 6, 8, 9];
const EQ2: &[usize] = &[2, 3, 4, 5, 7, 9, 10];
const EQ3: &[usize] = &[0, 1, 2, 4, 6, 7, 10];
/// The fifth equation that turns (15,11,3) into (16,11,4) — one more parity
/// bit, and a minimum distance of four instead of three.
/// `MMDVMHost/Hamming.cpp`: `CHamming::encode16114`.
const EQ4: &[usize] = &[0, 2, 5, 6, 8, 9, 10];

const CHECKS_15113: [&[usize]; 4] = [EQ0, EQ1, EQ2, EQ3];
const CHECKS_16114: [&[usize]; 5] = [EQ0, EQ1, EQ2, EQ3, EQ4];

/// Hamming (13,9,3)'s four equations over its nine data bits.
/// `MMDVMHost/Hamming.cpp`: `CHamming::encode1393`.
const CHECKS_1393: [&[usize]; 4] = [
    &[0, 1, 3, 5, 6],
    &[0, 1, 2, 4, 6, 7],
    &[0, 1, 2, 3, 5, 7, 8],
    &[0, 2, 4, 5, 8],
];

/// What a decoder found when it looked at a row or a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fix {
    /// Already a codeword.
    Clean,
    /// One bit was wrong and has been put back.
    Fixed,
    /// The syndrome fits no single-bit error. Past what the code promises.
    Unrecoverable,
}

/// The XOR of the bits an equation names.
fn parity_of(bits: &[bool], equation: &[usize]) -> bool {
    equation.iter().fold(false, |acc, &i| acc ^ bits[i])
}

/// Writes each check bit as the parity of its equation. The check bits follow
/// the `data_len` data bits, in equation order.
fn hamming_encode(bits: &mut [bool], checks: &[&[usize]], data_len: usize) {
    for (j, equation) in checks.iter().enumerate() {
        bits[data_len + j] = parity_of(bits, equation);
    }
}

/// One bit per equation: set when the received check bit disagrees with the
/// parity of the data it covers. Zero means "a codeword".
fn hamming_syndrome(bits: &[bool], checks: &[&[usize]], data_len: usize) -> u32 {
    let mut syndrome = 0u32;
    for (j, equation) in checks.iter().enumerate() {
        if parity_of(bits, equation) != bits[data_len + j] {
            syndrome |= 1u32 << j;
        }
    }
    syndrome
}

/// The syndrome a single flipped bit at `pos` would produce, computed from the
/// equations rather than looked up.
///
/// This is what makes the correctors below derivations instead of
/// transcriptions: `MMDVMHost/Hamming.cpp` spells the same mapping out as a
/// `switch` over syndrome values, one case per bit position, and those cases
/// are exactly the columns of the parity-check matrix.
fn error_syndrome(pos: usize, checks: &[&[usize]], data_len: usize) -> u32 {
    let mut syndrome = 0u32;
    for (j, equation) in checks.iter().enumerate() {
        let touched = if pos < data_len {
            equation.contains(&pos)
        } else {
            pos == data_len + j
        };
        if touched {
            syndrome |= 1u32 << j;
        }
    }
    syndrome
}

/// Corrects a single-bit error in place, or reports that there is no
/// single-bit story that fits.
fn hamming_correct(bits: &mut [bool], checks: &[&[usize]], data_len: usize) -> Fix {
    let syndrome = hamming_syndrome(bits, checks, data_len);
    if syndrome == 0 {
        return Fix::Clean;
    }
    match (0..bits.len()).find(|&pos| error_syndrome(pos, checks, data_len) == syndrome) {
        Some(pos) => {
            bits[pos] = !bits[pos];
            Fix::Fixed
        }
        None => Fix::Unrecoverable,
    }
}

/// Hamming (16,11,4) over a row of 16 bits: 11 data, 5 check.
///
/// Parity equations from `MMDVMHost/Hamming.cpp`: `CHamming::encode16114`.
/// Derived from those equations, not transcribed. This is BPTC(128,77)'s row
/// code — the embedded LC's — and the extra fifth equation is what lets its
/// column-parity row detect what a row corrected wrongly.
pub fn hamming16114_encode(bits: &mut [bool; 16]) {
    hamming_encode(bits, &CHECKS_16114, 11);
}

/// True when the sixteen bits are a (16,11,4) codeword. Detection only — see
/// [`hamming16114_correct`] to repair one.
#[must_use]
pub fn hamming16114_check(bits: &[bool; 16]) -> bool {
    hamming_syndrome(bits, &CHECKS_16114, 11) == 0
}

/// Corrects the one bit (16,11,4) promises, or reports it cannot.
fn hamming16114_correct(bits: &mut [bool; 16]) -> Fix {
    hamming_correct(bits, &CHECKS_16114, 11)
}

/// Hamming (13,9,3) — BPTC(196,96)'s column code.
///
/// Parity equations from `MMDVMHost/Hamming.cpp`: `CHamming::encode1393`.
/// Derived, not transcribed.
pub fn hamming1393_encode(bits: &mut [bool; 13]) {
    hamming_encode(bits, &CHECKS_1393, 9);
}

/// True when the thirteen bits are a (13,9,3) codeword.
#[must_use]
pub fn hamming1393_check(bits: &[bool; 13]) -> bool {
    hamming_syndrome(bits, &CHECKS_1393, 9) == 0
}

/// Corrects the one bit (13,9,3) promises, or reports it cannot.
fn hamming1393_correct(bits: &mut [bool; 13]) -> Fix {
    hamming_correct(bits, &CHECKS_1393, 9)
}

/// Hamming (15,11,3) — BPTC(196,96)'s row code.
///
/// Parity equations from `MMDVMHost/Hamming.cpp`: `CHamming::encode15113_2`.
/// That file carries two different (15,11,3) codes; `BPTC19696.cpp` calls the
/// `_2` one, and the two are not interchangeable. Derived, not transcribed.
pub fn hamming15113_encode(bits: &mut [bool; 15]) {
    hamming_encode(bits, &CHECKS_15113, 11);
}

/// True when the fifteen bits are a (15,11,3) codeword.
#[must_use]
pub fn hamming15113_check(bits: &[bool; 15]) -> bool {
    hamming_syndrome(bits, &CHECKS_15113, 11) == 0
}

/// Corrects the one bit (15,11,3) promises, or reports it cannot.
fn hamming15113_correct(bits: &mut [bool; 15]) -> Fix {
    hamming_correct(bits, &CHECKS_15113, 11)
}

// ── Golay (20,8) ────────────────────────────────────────────────────────────

/// Generator polynomial of the cyclic (23,12) Golay code, low bit first:
/// `x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1`.
///
/// `MMDVMHost/Golay2087.cpp` states it as `GENPOL 0x00000c75`. It is the same
/// polynomial `astar-ysf`'s `golay` module divides by; DMR's slot type uses
/// the extended (24,12,8) code shortened by four, which is where the 20 and
/// the 8 come from.
const GOLAY2087_GENERATOR: u32 = 0xC75;

/// The twenty-bit codeword for one data byte: the byte shifted up eleven
/// places plus the remainder of that division, then an overall even-parity
/// bit.
fn golay2087_codeword(value: u8) -> u32 {
    let shifted = u32::from(value) << 11;
    let mut rem = shifted;
    for i in (11..19).rev() {
        if rem >> i & 1 == 1 {
            rem ^= GOLAY2087_GENERATOR << (i - 11);
        }
    }
    let word19 = shifted | (rem & 0x7FF);
    (word19 << 1) | (word19.count_ones() & 1)
}

/// Every Golay(20,8) codeword, indexed by its data byte. Built once, 1 KiB,
/// from [`golay2087_codeword`] — one source of truth, and it is the
/// polynomial.
fn golay2087_codewords() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (value, slot) in table.iter_mut().enumerate() {
            // The index cannot exceed 255, so this never truncates.
            *slot = golay2087_codeword(u8::try_from(value).unwrap_or(0));
        }
        table
    })
}

/// Golay (20,8), used for the slot type. Generator `0xC75` over GF(2).
///
/// Parameters from `MMDVMHost/Golay2087.cpp`: its `GENPOL`, and the byte
/// layout its `CGolay2087::encode`/`decode` pair implies — data byte in
/// `data[0]`, codeword bits 11..4 in `data[1]`, bits 3..0 in `data[2]`'s high
/// nibble. Derived from the polynomial, not transcribed from that file's
/// 256-entry `ENCODING_TABLE_2087`.
///
/// `data[2]`'s low nibble is written as zero, as the reference does; the slot
/// type has nothing there.
pub fn golay2087_encode(data: &mut [u8; 3]) {
    let word = golay2087_codeword(data[0]);
    data[1] = ((word >> 4) & 0xFF) as u8;
    data[2] = ((word & 0x0F) as u8) << 4;
}

/// Recovers the data byte, correcting up to three bit errors.
///
/// Returns `None` when the received word is further than three bits from every
/// codeword. The code's minimum distance is eight, so a word within three of
/// one codeword is within three of no other: the first hit is the only hit.
/// Four errors are detectable and not correctable, and guessing at that
/// distance is how a corrupted slot type becomes a plausible wrong one.
#[must_use]
pub fn golay2087_decode(data: &[u8; 3]) -> Option<u8> {
    let received =
        (u32::from(data[0]) << 12) | (u32::from(data[1]) << 4) | (u32::from(data[2]) >> 4);
    for (value, &codeword) in golay2087_codewords().iter().enumerate() {
        if (codeword ^ received).count_ones() <= 3 {
            return u8::try_from(value).ok();
        }
    }
    None
}

// ── Quadratic residue (16,7,6) ──────────────────────────────────────────────

/// Generator polynomial of the cyclic (15,7) code, low bit first:
/// `x^8 + x^5 + x^4 + x^3 + 1`. `MMDVMHost/QR1676.cpp` states it as
/// `GENPOL 0x00000139`.
const QR1676_GENERATOR: u32 = 0x139;

/// The sixteen-bit codeword for seven data bits: the systematic (15,7) word
/// plus an overall even-parity bit.
fn qr1676_codeword(value: u8) -> u32 {
    let shifted = u32::from(value & 0x7F) << 8;
    let mut rem = shifted;
    for i in (8..15).rev() {
        if rem >> i & 1 == 1 {
            rem ^= QR1676_GENERATOR << (i - 8);
        }
    }
    let word15 = shifted | (rem & 0xFF);
    (word15 << 1) | (word15.count_ones() & 1)
}

/// Every QR(16,7,6) codeword, indexed by its seven data bits. Built once from
/// the generator polynomial.
fn qr1676_codewords() -> &'static [u32; 128] {
    static TABLE: OnceLock<[u32; 128]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 128];
        for (value, slot) in table.iter_mut().enumerate() {
            // The index cannot exceed 127, so this never truncates.
            *slot = qr1676_codeword(u8::try_from(value).unwrap_or(0));
        }
        table
    })
}

/// Quadratic residue (16,7,6), used for the EMB.
///
/// Parameters from `MMDVMHost/QR1676.cpp`: its `GENPOL`, and the layout its
/// `CQR1676::encode` implies — the seven data bits are `(data[0] >> 1) & 0x7F`
/// and the sixteen-bit codeword replaces both bytes. Derived from the
/// polynomial, not transcribed from that file's `ENCODING_TABLE_1676`.
pub fn qr1676_encode(data: &mut [u8; 2]) {
    let word = qr1676_codeword((data[0] >> 1) & 0x7F);
    data[0] = ((word >> 8) & 0xFF) as u8;
    data[1] = (word & 0xFF) as u8;
}

/// Recovers the seven data bits, correcting up to two bit errors.
///
/// Returns `None` beyond that. The minimum distance is six, so nothing three
/// bits out is within two of any codeword and the answer is honestly no
/// answer — the EMB carries the colour code and the LCSS that says where an
/// embedded-LC fragment belongs, and a wrong one reassembles the wrong LC.
#[must_use]
pub fn qr1676_decode(data: &[u8; 2]) -> Option<u8> {
    let received = (u32::from(data[0]) << 8) | u32::from(data[1]);
    for (value, &codeword) in qr1676_codewords().iter().enumerate() {
        if (codeword ^ received).count_ones() <= 2 {
            return u8::try_from(value).ok();
        }
    }
    None
}

// ── Reed-Solomon (12,9) over GF(2^8) ────────────────────────────────────────

/// Multiplication in GF(2^8) modulo `x^8 + x^4 + x^3 + x^2 + 1`.
///
/// `MMDVMHost/RS129.cpp` does this through the 512-entry exponent table and
/// the 256-entry log table it ships; the field is the same one, which its
/// exponent table's `0x80 -> 0x1D` step confirms. Carry-less multiply with
/// the reduction folded in is the definition, needs no tables, and costs
/// eight iterations on twelve bytes once per LC.
const fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        b >>= 1;
        let overflow = a & 0x80 != 0;
        a <<= 1;
        if overflow {
            // 0x11D with the x^8 term dropped by the shift above.
            a ^= 0x1D;
        }
    }
    product
}

/// `a^n` for the field's primitive element `a = 2`.
const fn gf_pow(exponent: usize) -> u8 {
    let mut value = 1u8;
    let mut i = 0;
    while i < exponent {
        value = gf_mul(value, 2);
        i += 1;
    }
    value
}

/// The generator polynomial's coefficients, low order first:
/// `g(x) = (x + a)(x + a^2)(x + a^3)` with `a = 2`, whose `x^3` coefficient is
/// 1 and is not stored.
///
/// `MMDVMHost/RS129.cpp` states the same three numbers as its `POLY` constant.
/// They are multiplied out here rather than copied, and a test checks that the
/// product is what that file says.
const RS129_GENERATOR: [u8; 3] = rs129_generator();

const fn rs129_generator() -> [u8; 3] {
    let mut g = [1u8, 0, 0, 0];
    let mut degree = 0usize;
    let mut root = 1usize;
    while root <= 3 {
        let alpha = gf_pow(root);
        // Multiply g by (x + alpha), high coefficient first so the shift does
        // not clobber what it reads.
        let mut i = degree + 1;
        loop {
            let shifted = if i == 0 { 0 } else { g[i - 1] };
            g[i] = shifted ^ gf_mul(g[i], alpha);
            if i == 0 {
                break;
            }
            i -= 1;
        }
        degree += 1;
        root += 1;
    }
    [g[0], g[1], g[2]]
}

/// Reed-Solomon (12,9) over GF(2^8), used for the full LC's parity.
///
/// The shift-register form from `MMDVMHost/RS129.cpp`: `CRS129::encode(data,
/// 9, parity)`, with the generator computed here instead of stated. Derived,
/// not transcribed.
///
/// The full LC then writes `parity[2]`, `parity[1]`, `parity[0]` — **reversed**
/// — into `lc[9..12]`, under the voice-header or terminator mask; that is
/// `MMDVMHost/DMRFullLC.cpp`: `CDMRFullLC::encode` and `DroidStar dmr.cpp:
/// full_lc_encode`, and it is the easiest thing in this file to get backwards.
/// Reversing it is the caller's job, not this function's.
#[must_use]
pub fn rs129_parity(data: &[u8; 9]) -> [u8; 3] {
    let mut parity = [0u8; 3];
    for &byte in data {
        let feedback = byte ^ parity[2];
        parity = [
            gf_mul(RS129_GENERATOR[0], feedback),
            parity[0] ^ gf_mul(RS129_GENERATOR[1], feedback),
            parity[1] ^ gf_mul(RS129_GENERATOR[2], feedback),
        ];
    }
    parity
}

/// Recovers the nine data symbols from a twelve-symbol codeword, correcting
/// one wrong symbol.
///
/// The codeword is laid out as `CRS129::check` reads it: nine data bytes, then
/// `parity[2]`, `parity[1]`, `parity[0]` — the full LC's own order, with the
/// mask already removed. Three parity symbols give a minimum distance of four:
/// one symbol anywhere in the twelve is recoverable and two are not.
///
/// The reference only re-encodes and compares. Correcting is a strict
/// improvement on that when — and only when — it is honest about its limit,
/// so a codeword that fits no single-symbol error returns `None` rather than
/// the nearest thing to a talker.
#[must_use]
pub fn rs129_decode(codeword: &[u8; 12]) -> Option<[u8; 9]> {
    // Syndromes: the codeword read as a polynomial with codeword[0] the x^11
    // coefficient, evaluated at a, a^2 and a^3 — the generator's three roots,
    // so a clean codeword gives three zeros.
    let mut syndrome = [0u8; 3];
    for (m, slot) in syndrome.iter_mut().enumerate() {
        let x = gf_pow(m + 1);
        *slot = codeword.iter().fold(0u8, |acc, &b| gf_mul(acc, x) ^ b);
    }

    let data = |word: &[u8; 12]| {
        let mut out = [0u8; 9];
        out.copy_from_slice(&word[..9]);
        out
    };

    if syndrome == [0, 0, 0] {
        return Some(data(codeword));
    }

    // One error of value e at polynomial position j gives syndrome[m] =
    // e * a^((m+1)*j). Twelve positions to try, and the third syndrome is the
    // check: two equations fix e, the third has to agree.
    for j in 0..12usize {
        let value = gf_mul(syndrome[0], gf_pow(255 - j));
        if value == 0 {
            continue;
        }
        let fits = gf_mul(value, gf_pow(2 * j)) == syndrome[1]
            && gf_mul(value, gf_pow(3 * j)) == syndrome[2];
        if fits {
            let mut fixed = *codeword;
            fixed[11 - j] ^= value;
            return Some(data(&fixed));
        }
    }
    None
}

// ── The embedded LC's five-bit check ────────────────────────────────────────

/// The embedded LC's five-bit check: the nine LC bytes summed and taken
/// mod 31.
///
/// `MMDVMHost/CRC.cpp`: `CCRC::encodeFiveBit` — it is a checksum, not a CRC,
/// and calling it one is how a polynomial gets invented for it.
/// `DroidStar CRCenc.cpp` carries the identical loop; two projects, the same
/// arithmetic, and neither is a polynomial.
#[must_use]
pub fn embedded_lc_checksum(lc: &[u8; 9]) -> u8 {
    let total: u32 = lc.iter().map(|&b| u32::from(b)).sum();
    (total % 31) as u8
}

// ── BPTC(196,96) ────────────────────────────────────────────────────────────

/// The interleave is arithmetic, not tabular: `raw[(a * 181) mod 196]` is
/// deinterleaved position `a`. `MMDVMHost/BPTC19696.cpp`:
/// `CBPTC19696::decodeDeInterleave`, and `encodeInterleave` is the same
/// permutation applied backwards.
const INTERLEAVE_STEP: usize = 181;

/// The deinterleaved indices the 96 payload bits occupy, in order.
///
/// Index 0 and row 0's first three data positions are the unused R(3) bits;
/// row 0 carries eight payload bits at its positions 3..10 and rows 1..8
/// carry eleven each, which is 8 + 88 = 96. `MMDVMHost/BPTC19696.cpp`:
/// `decodeExtractData`.
fn payload_indices() -> impl Iterator<Item = usize> {
    (4..=11).chain((1..9).flat_map(|r| (15 * r + 1)..=(15 * r + 11)))
}

fn pack_byte(bits: &[bool]) -> u8 {
    bits.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b))
}

fn unpack_byte(byte: u8, out: &mut [bool]) {
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = byte >> (7 - i) & 1 == 1;
    }
}

/// Reads a burst's 196 BPTC bits: burst bits 0..97, then the two that live in
/// byte 20's low bits, then burst bits 168..263.
///
/// The ten bits either side of the sync field that this skips are the slot
/// type, not payload. `MMDVMHost/BPTC19696.cpp`: `decodeExtractBinary`, whose
/// "handle the two bits" step is this asymmetry.
fn burst_to_raw(burst: &[u8; 33]) -> [bool; 196] {
    let mut raw = [false; 196];
    for (b, chunk) in raw[..96].chunks_mut(8).enumerate() {
        unpack_byte(burst[b], chunk);
    }
    raw[96] = burst[12] & 0x80 != 0;
    raw[97] = burst[12] & 0x40 != 0;
    raw[98] = burst[20] & 0x02 != 0;
    raw[99] = burst[20] & 0x01 != 0;
    for (i, chunk) in raw[100..].chunks_mut(8).enumerate() {
        unpack_byte(burst[21 + i], chunk);
    }
    raw
}

/// Writes those same 196 bits back, leaving every other bit of the burst as it
/// was — the sync field in bytes 13..19, the six slot-type bits in byte 12 and
/// byte 20. `MMDVMHost/BPTC19696.cpp`: `encodeExtractBinary`, whose masks
/// (`& 0x3F`, `& 0xFC`) say the same thing.
fn raw_to_burst(raw: &[bool; 196], burst: &mut [u8; 33]) {
    for (b, chunk) in raw[..96].chunks(8).enumerate() {
        burst[b] = pack_byte(chunk);
    }
    burst[12] = (burst[12] & 0x3F) | (u8::from(raw[96]) << 7) | (u8::from(raw[97]) << 6);
    burst[20] = (burst[20] & 0xFC) | (u8::from(raw[98]) << 1) | u8::from(raw[99]);
    for (i, chunk) in raw[100..].chunks(8).enumerate() {
        burst[21 + i] = pack_byte(chunk);
    }
}

/// The thirteen bits of deinterleaved column `c`: index `c + 1`, then every
/// fifteenth bit after it. The matrix is 13 x 15 starting at index 1, and the
/// unused R(3) bit at index 0 is not part of any column.
fn bptc19696_column(deinterleaved: &[bool; 196], c: usize) -> [bool; 13] {
    let mut column = [false; 13];
    for (a, slot) in column.iter_mut().enumerate() {
        *slot = deinterleaved[c + 1 + 15 * a];
    }
    column
}

/// Puts a column back where [`bptc19696_column`] read it from.
fn bptc19696_put_column(deinterleaved: &mut [bool; 196], c: usize, column: &[bool; 13]) {
    for (a, &bit) in column.iter().enumerate() {
        deinterleaved[c + 1 + 15 * a] = bit;
    }
}

/// Runs the nine data rows through Hamming (15,11,3) and all fifteen columns
/// through Hamming (13,9,3). Rows first: the columns then cover the row parity
/// bits too, which is what makes this a product code rather than two codes.
fn bptc19696_error_check(deinterleaved: &mut [bool; 196]) {
    for row in deinterleaved[1..].chunks_mut(15).take(9) {
        hamming_encode(row, &CHECKS_15113, 11);
    }
    for c in 0..15 {
        let mut column = bptc19696_column(deinterleaved, c);
        hamming1393_encode(&mut column);
        bptc19696_put_column(deinterleaved, c, &column);
    }
}

/// BPTC(196,96): 96 payload bits into the two 108-bit information halves of a
/// burst. `payload` is the 12-byte full LC.
///
/// Layout and interleave from `MMDVMHost/BPTC19696.cpp`; row and column codes
/// from `Hamming.cpp`. Derived from those parameters, not transcribed. The
/// burst's sync field and slot-type bits are read and written back unchanged,
/// which a test pins.
pub fn bptc19696_encode(payload: &[u8; 12], burst: &mut [u8; 33]) {
    let mut bits = [false; 96];
    for (b, chunk) in bits.chunks_mut(8).enumerate() {
        unpack_byte(payload[b], chunk);
    }

    let mut deinterleaved = [false; 196];
    for (index, bit) in payload_indices().zip(bits) {
        deinterleaved[index] = bit;
    }
    bptc19696_error_check(&mut deinterleaved);

    let mut raw = [false; 196];
    for (a, &bit) in deinterleaved.iter().enumerate() {
        raw[(a * INTERLEAVE_STEP) % 196] = bit;
    }
    raw_to_burst(&raw, burst);
}

/// Recovers the 12-byte payload from a burst, repairing what the product code
/// can repair.
///
/// Rows and columns are corrected in turn until nothing more changes — a
/// column fixes what a row could not see, which can leave that row fixable on
/// the next pass. Five passes, as `MMDVMHost/BPTC19696.cpp`'s
/// `decodeErrorCheck` bounds it. Then every row and column has to check out:
/// the reference extracts the payload regardless, and this returns `None`,
/// because a full LC that still does not satisfy its own parity is a talker id
/// nobody said.
#[must_use]
pub fn bptc19696_decode(burst: &[u8; 33]) -> Option<[u8; 12]> {
    let raw = burst_to_raw(burst);
    let mut deinterleaved = [false; 196];
    for (a, slot) in deinterleaved.iter_mut().enumerate() {
        *slot = raw[(a * INTERLEAVE_STEP) % 196];
    }

    for _ in 0..5 {
        let mut fixing = false;
        for c in 0..15 {
            let mut column = bptc19696_column(&deinterleaved, c);
            if hamming1393_correct(&mut column) == Fix::Fixed {
                bptc19696_put_column(&mut deinterleaved, c, &column);
                fixing = true;
            }
        }
        for row in deinterleaved[1..].chunks_mut(15).take(9) {
            let row: &mut [bool; 15] = row.try_into().expect("fifteen");
            if hamming15113_correct(row) == Fix::Fixed {
                fixing = true;
            }
        }
        if !fixing {
            break;
        }
    }

    let rows_ok = deinterleaved[1..].chunks(15).take(9).all(|row| {
        let row: &[bool; 15] = row.try_into().expect("fifteen");
        hamming15113_check(row)
    });
    if !rows_ok {
        return None;
    }
    if !(0..15).all(|c| hamming1393_check(&bptc19696_column(&deinterleaved, c))) {
        return None;
    }

    let mut bits = [false; 96];
    for (slot, index) in bits.iter_mut().zip(payload_indices()) {
        *slot = deinterleaved[index];
    }
    let mut payload = [0u8; 12];
    for (byte, chunk) in payload.iter_mut().zip(bits.chunks(8)) {
        *byte = pack_byte(chunk);
    }
    Some(payload)
}

// ── BPTC(128,77) ────────────────────────────────────────────────────────────

/// The matrix indices the checksum's five bits occupy, most significant
/// first. `MMDVMHost/DMREmbeddedData.cpp`: `encodeEmbeddedData`.
const CHECKSUM_INDICES: [usize; 5] = [42, 58, 74, 90, 106];

/// The matrix indices the 72 LC bits occupy, in order: eleven in each of rows
/// 0 and 1, ten in each of rows 2..6 — whose eleventh data position is a
/// checksum bit. `MMDVMHost/DMREmbeddedData.cpp`: `encodeEmbeddedData`.
fn embedded_lc_indices() -> impl Iterator<Item = usize> {
    (0..11)
        .chain(16..27)
        .chain((2..7).flat_map(|r| (16 * r)..(16 * r + 10)))
}

/// BPTC(128,77): the embedded-LC matrix carried four bits at a time across
/// bursts B-E. `lc` is the 9-byte LC; the five-bit checksum is computed here.
///
/// Sixteen wide, eight tall: rows 0..6 are Hamming (16,11,4) codewords, row 7
/// is the column parity of the seven above it, and the result is read out down
/// the columns. `MMDVMHost/DMREmbeddedData.cpp`: `encodeEmbeddedData`.
/// Derived from that layout, not transcribed.
#[must_use]
pub fn bptc12877_encode(lc: &[u8; 9]) -> [bool; 128] {
    let mut bits = [false; 72];
    for (b, chunk) in bits.chunks_mut(8).enumerate() {
        unpack_byte(lc[b], chunk);
    }

    let mut matrix = [false; 128];
    for (index, bit) in embedded_lc_indices().zip(bits) {
        matrix[index] = bit;
    }
    let checksum = embedded_lc_checksum(lc);
    for (shift, &index) in (0..5u32).rev().zip(&CHECKSUM_INDICES) {
        matrix[index] = checksum >> shift & 1 == 1;
    }

    for row in matrix.chunks_mut(16).take(7) {
        hamming_encode(row, &CHECKS_16114, 11);
    }
    let (rows, parity) = matrix.split_at_mut(112);
    for (c, slot) in parity.iter_mut().enumerate() {
        *slot = rows.chunks(16).fold(false, |acc, row| acc ^ row[c]);
    }

    // Read out down the columns: sixteen on, wrapping at 127, so index 127
    // maps to itself and the other 127 positions rotate.
    let mut raw = [false; 128];
    let mut index = 0usize;
    for slot in &mut raw {
        *slot = matrix[index];
        index += 16;
        if index > 127 {
            index -= 127;
        }
    }
    raw
}

/// Recovers the 9-byte LC, repairing one bit in each Hamming row.
///
/// The column-parity row is checked and never corrected — it is the only thing
/// that can tell a row's wrong "correction" from a right one — and the
/// five-bit checksum has to agree as well. Any of the three failing gives
/// `None`: a wrong source id shown as a talker is worse than no talker at all.
/// `MMDVMHost/DMREmbeddedData.cpp`: `decodeEmbeddedData`.
#[must_use]
pub fn bptc12877_decode(raw: &[bool; 128]) -> Option<[u8; 9]> {
    // The same column walk as the encoder, with the roles swapped.
    let mut matrix = [false; 128];
    let mut index = 0usize;
    for &bit in raw {
        matrix[index] = bit;
        index += 16;
        if index > 127 {
            index -= 127;
        }
    }

    for row in matrix.chunks_mut(16).take(7) {
        let row: &mut [bool; 16] = row.try_into().expect("sixteen");
        if hamming16114_correct(row) == Fix::Unrecoverable {
            return None;
        }
    }
    let (rows, parity) = matrix.split_at(112);
    for (c, &bit) in parity.iter().enumerate() {
        if rows.chunks(16).fold(bit, |acc, row| acc ^ row[c]) {
            return None;
        }
    }

    let mut bits = [false; 72];
    for (slot, index) in bits.iter_mut().zip(embedded_lc_indices()) {
        *slot = matrix[index];
    }
    let mut lc = [0u8; 9];
    for (byte, chunk) in lc.iter_mut().zip(bits.chunks(8)) {
        *byte = pack_byte(chunk);
    }

    let mut checksum = 0u8;
    for (shift, &index) in (0..5u32).rev().zip(&CHECKSUM_INDICES) {
        if matrix[index] {
            checksum |= 1 << shift;
        }
    }
    (checksum == embedded_lc_checksum(&lc)).then_some(lc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MMDVMHost/DMRDefines.h`. Task 6 gives these a permanent home in
    /// `frame.rs`; BPTC's contract is that it does not touch them, and that
    /// has to be testable before `frame.rs` exists.
    const SYNC_MASK: [u8; 7] = [0x0F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xF0];
    const MS_SOURCED_DATA_SYNC: [u8; 7] = [0x0D, 0x5D, 0x7F, 0x77, 0xFD, 0x75, 0x70];

    /// The nine-byte LC every vector in this file is computed over: a group
    /// call from 3021214 to 31118, the shape `docs/design/dmr-wire.md` §7
    /// gives the full LC.
    const LC9: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7];

    fn pack(bits: &[bool]) -> Vec<u8> {
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b)))
            .collect()
    }

    #[test]
    fn hamming16114_is_the_published_parity_set() {
        // MMDVMHost/Hamming.cpp: CHamming::encode16114 -- five check bits,
        // each an XOR over a fixed subset of the eleven data bits. Written
        // out from the equations; this asserts the generated form, and that
        // every single-bit error is detected, which a transcription of a
        // table would not prove.
        let mut bits = [false; 16];
        for (i, b) in bits.iter_mut().take(11).enumerate() {
            *b = i % 3 == 0;
        }
        hamming16114_encode(&mut bits);
        assert!(hamming16114_check(&bits));
        for flip in 0..16 {
            let mut broken = bits;
            broken[flip] = !broken[flip];
            assert!(
                !hamming16114_check(&broken),
                "bit {flip} flipped undetected"
            );
        }
    }

    #[test]
    fn the_two_bptc_row_and_column_codes_detect_every_single_bit_error() {
        let mut row = [false; 13];
        row[0] = true;
        row[4] = true;
        row[8] = true;
        hamming1393_encode(&mut row);
        assert!(hamming1393_check(&row));
        for flip in 0..13 {
            let mut broken = row;
            broken[flip] = !broken[flip];
            assert!(!hamming1393_check(&broken));
        }

        let mut col = [false; 15];
        col[1] = true;
        col[10] = true;
        hamming15113_encode(&mut col);
        assert!(hamming15113_check(&col));
        for flip in 0..15 {
            let mut broken = col;
            broken[flip] = !broken[flip];
            assert!(!hamming15113_check(&broken));
        }
    }

    #[test]
    fn every_hamming_single_bit_error_is_put_back_where_it_was() {
        // The promise each code makes: one flipped bit anywhere, corrected.
        // Nothing in BPTC works if this is only detection.
        let mut row = [false; 13];
        for (i, b) in row.iter_mut().take(9).enumerate() {
            *b = i % 2 == 0;
        }
        hamming1393_encode(&mut row);
        for flip in 0..13 {
            let mut broken = row;
            broken[flip] = !broken[flip];
            assert_eq!(hamming1393_correct(&mut broken), Fix::Fixed);
            assert_eq!(broken, row, "bit {flip} was not put back");
        }
        assert_eq!(hamming1393_correct(&mut row.clone()), Fix::Clean);

        let mut col = [false; 15];
        for (i, b) in col.iter_mut().take(11).enumerate() {
            *b = i % 3 != 0;
        }
        hamming15113_encode(&mut col);
        for flip in 0..15 {
            let mut broken = col;
            broken[flip] = !broken[flip];
            assert_eq!(hamming15113_correct(&mut broken), Fix::Fixed);
            assert_eq!(broken, col, "bit {flip} was not put back");
        }

        let mut wide = [false; 16];
        for (i, b) in wide.iter_mut().take(11).enumerate() {
            *b = i % 4 == 1;
        }
        hamming16114_encode(&mut wide);
        for flip in 0..16 {
            let mut broken = wide;
            broken[flip] = !broken[flip];
            assert_eq!(hamming16114_correct(&mut broken), Fix::Fixed);
            assert_eq!(broken, wide, "bit {flip} was not put back");
        }
        // (16,11,4) detects two errors and must not invent a correction for
        // them: distance four leaves no single-bit story that fits.
        let mut two = wide;
        two[0] = !two[0];
        two[7] = !two[7];
        assert_eq!(hamming16114_correct(&mut two), Fix::Unrecoverable);
    }

    #[test]
    fn golay2087_round_trips_and_corrects_a_single_error() {
        // Golay (20,8): eight data bits in the top byte, twelve check bits.
        // Generated from the polynomial, not read out of a table.
        for value in [0x00u8, 0x01, 0x13, 0x7F, 0x80, 0xFF] {
            let mut word = [value, 0x00, 0x00];
            golay2087_encode(&mut word);
            assert_eq!(golay2087_decode(&word), Some(value));
            let mut broken = word;
            broken[1] ^= 0x08;
            assert_eq!(
                golay2087_decode(&broken),
                Some(value),
                "one error is correctable"
            );
        }
    }

    #[test]
    fn golay2087_matches_independently_computed_codewords() {
        // Computed by an independent script (GF(2) long division by 0xC75
        // plus an overall parity bit) and cross-checked against every one of
        // the 256 entries of MMDVMHost/Golay2087.cpp's ENCODING_TABLE_2087,
        // which is not copied here: six values are pinned, the rest are
        // covered by the round trip above.
        let expected: [(u8, [u8; 3]); 6] = [
            (0x00, [0x00, 0x00, 0x00]),
            (0x01, [0x01, 0x8E, 0xB0]),
            (0x13, [0x13, 0x2B, 0x20]),
            (0x7F, [0x7F, 0xEB, 0x70]),
            (0x80, [0x80, 0x3D, 0xA0]),
            (0xFF, [0xFF, 0xD6, 0xD0]),
        ];
        for (value, want) in expected {
            let mut word = [value, 0x00, 0x00];
            golay2087_encode(&mut word);
            assert_eq!(word, want, "codeword for {value:#04X}");
        }
    }

    #[test]
    fn golay2087_corrects_three_errors_and_refuses_four() {
        // Minimum distance eight: three errors are always correctable and
        // four never are. A guess at four bits out is a wrong slot type read
        // as a real one.
        let mut word = [0x13u8, 0x00, 0x00];
        golay2087_encode(&mut word);

        let mut three = word;
        three[0] ^= 0x81;
        three[2] ^= 0x10;
        assert_eq!(golay2087_decode(&three), Some(0x13));

        let mut four = three;
        four[1] ^= 0x02;
        assert_eq!(golay2087_decode(&four), None);
    }

    #[test]
    fn qr1676_round_trips_the_seven_data_bits() {
        // MMDVMHost/QR1676.cpp: the seven data bits are `(data[0] >> 1) &
        // 0x7F`, the sixteen-bit codeword replaces both bytes.
        for value in [0x00u8, 0x01, 0x2A, 0x55, 0x7F] {
            let mut word = [value << 1, 0x00];
            qr1676_encode(&mut word);
            assert_eq!(qr1676_decode(&word), Some(value));
        }
    }

    #[test]
    fn qr1676_matches_independently_computed_codewords() {
        // Same method as Golay: long division by 0x139 plus an overall parity
        // bit, cross-checked against all 128 entries of
        // MMDVMHost/QR1676.cpp's ENCODING_TABLE_1676, none of which is copied.
        let expected: [(u8, [u8; 2]); 5] = [
            (0x00, [0x00, 0x00]),
            (0x01, [0x02, 0x73]),
            (0x2A, [0x54, 0x19]),
            (0x55, [0xAA, 0x42]),
            (0x7F, [0xFE, 0x5B]),
        ];
        for (value, want) in expected {
            let mut word = [value << 1, 0x00];
            qr1676_encode(&mut word);
            assert_eq!(word, want, "codeword for {value:#04X}");
        }
    }

    #[test]
    fn qr1676_corrects_two_errors_and_refuses_three() {
        // Minimum distance six. The EMB carries the colour code and the LCSS
        // that says where an embedded-LC fragment belongs; a wrong one
        // reassembles somebody else's LC.
        let mut word = [0x55u8 << 1, 0x00];
        qr1676_encode(&mut word);

        let mut two = word;
        two[0] ^= 0x40;
        two[1] ^= 0x04;
        assert_eq!(qr1676_decode(&two), Some(0x55));

        let mut three = two;
        three[1] ^= 0x20;
        assert_eq!(qr1676_decode(&three), None);
    }

    #[test]
    fn rs129_parity_is_three_bytes_over_the_nine_lc_bytes() {
        // MMDVMHost/RS129.cpp: CRS129::encode(data, 9, parity). The full LC
        // then writes parity[2], parity[1], parity[0] -- reversed -- into
        // lc[9..12], XORed with the header or terminator mask. That reversal
        // is DroidStar dmr.cpp: full_lc_encode, and it is the easiest thing
        // in this file to get backwards.
        let parity = rs129_parity(&LC9);
        assert_eq!(parity.len(), 3);
        // Computed independently twice before this line was written: once by
        // carry-less multiplication mod 0x11D, once through the exponent and
        // log tables MMDVMHost ships. Both give 98 69 88.
        assert_eq!(parity, [0x98, 0x69, 0x88]);
    }

    #[test]
    fn the_rs129_generator_is_the_product_of_its_three_roots() {
        // g(x) = (x + a)(x + a^2)(x + a^3) with a = 2 over GF(2^8)/0x11D.
        // MMDVMHost/RS129.cpp states the coefficients as its POLY constant;
        // this multiplies them out instead, and the two agree.
        assert_eq!(RS129_GENERATOR, [64, 56, 14]);
    }

    #[test]
    fn rs129_corrects_one_wrong_symbol_and_refuses_two() {
        // Three parity symbols, minimum distance four: one symbol anywhere in
        // the twelve is recoverable, two are not, and a full LC that is two
        // symbols out must be dropped rather than shown as a talker.
        let parity = rs129_parity(&LC9);
        let mut codeword = [0u8; 12];
        codeword[..9].copy_from_slice(&LC9);
        codeword[9] = parity[2];
        codeword[10] = parity[1];
        codeword[11] = parity[0];
        assert_eq!(rs129_decode(&codeword), Some(LC9));

        for pos in 0..12 {
            let mut broken = codeword;
            broken[pos] ^= 0x5A;
            assert_eq!(rs129_decode(&broken), Some(LC9), "symbol {pos}");
        }

        let mut two = codeword;
        two[2] ^= 0x11;
        two[8] ^= 0x22;
        assert_eq!(rs129_decode(&two), None);
    }

    #[test]
    fn the_embedded_lc_checksum_is_a_sum_mod_thirty_one() {
        // MMDVMHost/CRC.cpp: CCRC::encodeFiveBit sums the nine bytes and
        // takes the total mod 31. Not a polynomial, and never five bits of
        // CRC however it is named elsewhere.
        assert_eq!(embedded_lc_checksum(&[0; 9]), 0);
        assert_eq!(embedded_lc_checksum(&[1, 0, 0, 0, 0, 0, 0, 0, 0]), 1);
        assert_eq!(embedded_lc_checksum(&[31, 0, 0, 0, 0, 0, 0, 0, 0]), 0);
        assert_eq!(embedded_lc_checksum(&[0xFF; 9]), (255u32 * 9 % 31) as u8);
        assert!(embedded_lc_checksum(&[0xFF; 9]) < 31);
        // The test LC: 0x7A + 0x51 + 0x30 + 0x1E + 0xB7 = 494, 494 % 31 = 30.
        assert_eq!(embedded_lc_checksum(&LC9), 30);
    }

    #[test]
    fn bptc19696_round_trips_a_full_lc_through_a_burst() {
        // BPTC(196,96) lands in the burst's two 108-bit information halves
        // and leaves the 48-bit sync field alone -- the whole point of the
        // interleave. A round trip that also proves the sync field is
        // untouched is worth more than a borrowed vector.
        let lc: [u8; 12] = [
            0x00, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7, 0x96, 0x96, 0x96,
        ];
        let mut burst = [0u8; 33];
        burst[13..20].copy_from_slice(&MS_SOURCED_DATA_SYNC);
        let sync_before: [u8; 7] = burst[13..20].try_into().expect("seven");
        bptc19696_encode(&lc, &mut burst);
        assert_eq!(bptc19696_decode(&burst), Some(lc));
        let sync_after: [u8; 7] = burst[13..20].try_into().expect("seven");
        for i in 0..7 {
            let mask = SYNC_MASK[i];
            assert_eq!(
                sync_after[i] & mask,
                sync_before[i] & mask,
                "BPTC wrote into the sync field at byte {i}"
            );
        }
    }

    #[test]
    fn bptc19696_matches_an_independently_computed_burst() {
        // The 33 bytes below came out of a throwaway script written from the
        // definitions in docs/design/dmr-wire.md §8 -- the (a * 181) mod 196
        // interleave, the row and column codes, and the two stray bits in
        // burst bytes 12 and 20 -- not from this implementation. A round trip
        // proves the two halves of one file agree; this proves the file is
        // right.
        let lc: [u8; 12] = [
            0x00, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7, 0x96, 0x96, 0x96,
        ];
        let mut burst = [0u8; 33];
        burst[13..20].copy_from_slice(&MS_SOURCED_DATA_SYNC);
        bptc19696_encode(&lc, &mut burst);
        assert_eq!(
            burst,
            [
                0x08, 0x27, 0x10, 0x40, 0x35, 0x40, 0x41, 0x70, 0x68, 0x40, 0x79, 0x61, 0x40, 0x0D,
                0x5D, 0x7F, 0x77, 0xFD, 0x75, 0x70, 0x02, 0xC8, 0x1E, 0x18, 0x77, 0x60, 0x15, 0xC0,
                0x3F, 0x83, 0x13, 0x80, 0xAD,
            ]
        );
    }

    #[test]
    fn bptc19696_repairs_scattered_single_bit_errors() {
        // One bit per row and one per column is exactly what the product code
        // is for: the rows fix what the columns cannot see and the other way
        // about.
        let lc: [u8; 12] = [
            0x03, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7, 0x11, 0x22, 0x33,
        ];
        let mut burst = [0u8; 33];
        bptc19696_encode(&lc, &mut burst);
        for byte in [0usize, 3, 7, 11, 22, 27, 31] {
            let mut broken = burst;
            broken[byte] ^= 0x20;
            assert_eq!(
                bptc19696_decode(&broken),
                Some(lc),
                "one bit in byte {byte}"
            );
        }
    }

    #[test]
    fn a_shredded_burst_is_refused_rather_than_half_decoded() {
        // Past what the product code can repair, the answer is no answer.
        let lc: [u8; 12] = [
            0x00, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7, 0x96, 0x96, 0x96,
        ];
        let mut burst = [0u8; 33];
        bptc19696_encode(&lc, &mut burst);
        for byte in &mut burst[0..12] {
            *byte ^= 0xFF;
        }
        assert_eq!(bptc19696_decode(&burst), None);
    }

    #[test]
    fn bptc12877_round_trips_an_embedded_lc() {
        let lc = [0x00u8, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7];
        let raw = bptc12877_encode(&lc);
        assert_eq!(bptc12877_decode(&raw), Some(lc));
    }

    #[test]
    fn bptc12877_matches_an_independently_computed_matrix() {
        // Sixteen bytes of the same independent script: the checksum bits at
        // matrix indices 42, 58, 74, 90 and 106, seven Hamming(16,11,4) rows,
        // a column-parity row, read out down the columns.
        let raw = bptc12877_encode(&LC9);
        assert_eq!(
            pack(&raw),
            vec![
                0x03, 0x18, 0x12, 0x11, 0x12, 0x0A, 0x11, 0x06, 0x0F, 0x1E, 0x3C, 0x1B, 0x09, 0x3F,
                0x30, 0x27,
            ]
        );
    }

    #[test]
    fn bptc12877_repairs_one_bit_in_each_hamming_row() {
        // The output is column-ordered, so these six offsets land in six
        // different Hamming rows (0, 5, 3, 0, 4 and 6) and none of them in the
        // column-parity row, which is checked and never corrected.
        let raw = bptc12877_encode(&LC9);
        for offset in [0usize, 5, 19, 40, 100, 110] {
            let mut broken = raw;
            broken[offset] = !broken[offset];
            assert_eq!(bptc12877_decode(&broken), Some(LC9), "bit {offset}");
        }
    }

    #[test]
    fn a_corrupted_embedded_lc_is_refused_rather_than_reported_as_a_talker() {
        // A wrong source id shown as a talker is worse than no talker at
        // all: the operator reads it, believes it, and answers the wrong
        // station.
        let lc = [0x00u8, 0x00, 0x00, 0x00, 0x7A, 0x51, 0x30, 0x1E, 0xB7];
        let mut raw = bptc12877_encode(&lc);
        for i in 0..8 {
            raw[i * 7 + 3] = !raw[i * 7 + 3];
        }
        assert_eq!(bptc12877_decode(&raw), None);
    }
}
