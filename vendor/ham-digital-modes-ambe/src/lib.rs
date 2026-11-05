// SPDX-License-Identifier: LGPL-3.0-or-later
//! The AMBE/IMBE vocoder tree of Bruce Perens' `ham_digital_modes` crate
//! (`BrucePerens/hams_open`, `daemons/ham_digital_modes`), vendored for astar's
//! software D-Star vocoder. Upstream's `src/ambe/` is kept as published, minus
//! its AMBE+2 directories; this file replaces upstream's `lib.rs`, which also
//! carried FT8, WSPR, PSK31, RTTY and Codec 2. See VENDORED.md.

pub mod ambe;
