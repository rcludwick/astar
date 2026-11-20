// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! SSTV picture decoding for astar.
//!
//! A [`Decoder`] takes 16-bit audio at the rate astar hears it and turns
//! slow-scan TV transmissions into [`Event`]s: a picture starts, its rows
//! arrive, and it finishes, is abandoned when the signal goes silent, or is
//! rejected as noise. A [`Picture`] saves itself as a PNG.
//!
//! The decoding itself is the `slowrx` crate (MIT, a port of Oona
//! Räisänen's ISC-licensed slowrx). This crate only ever decodes: it has no
//! encoder, and a picture starts only on a valid VIS header, never on line
//! sync alone.

mod decode;
mod mode;
mod png_out;
mod retime;
mod worker;

pub use decode::{
    Decoder, Event, Picture, Retime, SILENCE_LEVEL, SILENCE_SECS, SstvError, decode_rebuilt,
};
pub use mode::Mode;
pub use worker::{Live, Outcome, SSTV_TAP_CAPACITY, Worker};
