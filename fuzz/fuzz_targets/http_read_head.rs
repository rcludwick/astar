// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![no_main]
//! Fuzz target: the astar-http request-head parser on arbitrary bytes. A
//! panic is a bug, and so is a result that breaks the parser's contract:
//! an error answers 400 or 431 (or closes), and a parsed head has no
//! control bytes in its target or values and only tchar header names.
use astar_http::parse::{HeadError, read_head};
use libfuzzer_sys::fuzz_target;

fn tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn control(s: &str) -> bool {
    s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
}

fuzz_target!(|data: &[u8]| {
    let mut r = std::io::BufReader::new(data);
    match read_head(&mut r, 8 * 1024) {
        Err(e) => assert!(
            matches!(e.status(), Some(400 | 431)) || matches!(e, HeadError::Io(_)),
            "unexpected status {:?}",
            e.status()
        ),
        Ok(None) => assert!(data.is_empty()),
        Ok(Some(h)) => {
            assert!(!control(&h.target));
            for (k, v) in &h.headers {
                assert!(!k.is_empty() && k.bytes().all(tchar), "name {k:?}");
                assert!(!control(v), "value {v:?}");
            }
        }
    }
});
