// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio codecs used over IAX2.

pub mod dtmf;

#[cfg(feature = "g711")]
pub mod g711;

/// Codec 2 speech vocoder — runtime `dlopen` loader + static linked backend
/// (iax-f2b8). Neither backend is in this crate's default features; see the
/// licensing note at the top of the module for why.
#[cfg(any(feature = "codec2-static", feature = "codec2-runtime"))]
pub mod codec2;

/// AMBE vocoder backend for D-Star (iax-a9d4): a ThumbDV hardware dongle
/// (`ambe-hw`). It is not in this crate's default features; see the
/// licensing/feature note at the top of the module for why.
#[cfg(feature = "ambe-hw")]
pub mod ambe;

/// System Fusion DN voice: the AMBE+2 half-rate frames inside a YSF
/// payload, and the AMBE-3000 words that decode them (`iax-e8a4` §1). Pure
/// bytes in and bytes out — no hardware, so it is not `ambe-hw`-gated.
pub mod ysf;

/// NXDN voice: the four AMBE+2 half-rate frames inside a 33-byte network
/// frame. `astar-nxdn` carries the two 14-byte blocks; this is the layer
/// that reads them. Same 49-bit frame as YSF DN, so it reuses
/// [`ysf::DnFrame`] and [`ysf::ratep_dn`] rather than duplicating them.
pub mod nxdn;

/// DMR voice: the AMBE+2 full-rate frames inside an MMDVM/homebrew burst,
/// and the AMBE-3000 rate word that configures a dongle to produce them.
/// Same 72-bit channel width as D-Star and a DIFFERENT rate word — see the
/// module doc. Pure bytes in and bytes out, so it is not `ambe-hw`-gated.
pub mod dmr;

/// slin (16-bit signed linear PCM) wire framing (iax-31f7).
pub mod slin;

pub mod jitter;
