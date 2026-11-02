// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The node logger (`[recording]`): every over the conference carries, as
//! one FLAC per over, listed in `index.jsonl`, kept under a size and an age
//! cap.

pub mod http;
pub mod index;
pub mod logger;
