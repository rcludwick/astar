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

/// slin (16-bit signed linear PCM) wire framing (iax-31f7).
pub mod slin;

pub mod jitter;
