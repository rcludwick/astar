// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Properties of the head parser over arbitrary and well-formed input.
use astar_http::parse::{HeadError, read_head};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;
use std::fmt::Write as _;
use std::io::{BufReader, Read};

/// Hands out its data in short reads and counts what was pulled.
struct Counting<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.data.len() - self.pos).min(7);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn has_control(s: &str) -> bool {
    s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
}

const BASE: &[u8] =
    b"GET /abc?q=1 HTTP/1.1\r\nHost: example.org\r\nX-Pad: padding\r\nAccept: */*\r\n\r\n";

/// The contract every input must meet. Returns whether the head parsed.
fn check(data: &[u8], max: usize) -> Result<bool, TestCaseError> {
    let mut src = Counting { data, pos: 0 };
    let mut r = BufReader::with_capacity(64, &mut src);
    let out = read_head(&mut r, max);
    drop(r);
    // At most one byte past the cap, plus what the 64-byte buffer prefetched.
    prop_assert!(
        src.pos <= max + 1 + 64,
        "pulled {} with max {}",
        src.pos,
        max
    );
    match out {
        Err(e) => {
            prop_assert!(!matches!(e, HeadError::Io(_)), "in-memory source gave Io");
            prop_assert!(matches!(e.status(), Some(400 | 431)));
            Ok(false)
        }
        Ok(None) => {
            prop_assert!(data.is_empty());
            Ok(false)
        }
        Ok(Some(h)) => {
            prop_assert!(!has_control(&h.target));
            prop_assert!(h.target.starts_with('/') || h.target == "*");
            prop_assert!(!h.method.is_empty() && h.method.bytes().all(|b| b.is_ascii_uppercase()));
            prop_assert!(h.version == "HTTP/1.1" || h.version == "HTTP/1.0");
            for (k, v) in &h.headers {
                prop_assert!(!k.is_empty() && k.bytes().all(is_tchar), "name {k:?}");
                prop_assert!(!has_control(v), "value {v:?}");
            }
            for single in ["host", "content-length", "transfer-encoding"] {
                let n = h
                    .headers
                    .iter()
                    .filter(|(k, _)| k.eq_ignore_ascii_case(single))
                    .count();
                prop_assert!(n <= 1, "{single} repeated");
            }
            Ok(true)
        }
    }
}

/// Hostile input that still reaches the header phase: a valid request line
/// then header lines that are sometimes well-formed, sometimes byte soup.
fn hostile_headers() -> impl Strategy<Value = Vec<u8>> {
    let line = prop_oneof![
        3 => "[A-Za-z-]{1,8}: [ -~]{0,20}",
        1 => "[A-Za-z-]{1,8}:?[ -~\t\r]{0,20}",
        1 => "(Host|host|CONTENT-LENGTH|Transfer-Encoding): [0-9a-z]{0,5}",
    ];
    proptest::collection::vec(line, 0..12)
        .prop_map(|ls| format!("GET /p HTTP/1.1\r\n{}\r\n\r\n", ls.join("\r\n")).into_bytes())
}

/// A valid head with random byte flips, insertions and deletions.
fn mutated_head() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec((any::<usize>(), 0u8..3, any::<u8>()), 0..4).prop_map(|edits| {
        let mut d = BASE.to_vec();
        for (at, op, b) in edits {
            let i = at % (d.len() + 1);
            match op {
                0 if i < d.len() => d[i] = b,
                1 => d.insert(i, b),
                _ if i < d.len() => {
                    d.remove(i);
                }
                _ => {}
            }
        }
        d
    })
}

fn any_input() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => proptest::collection::vec(any::<u8>(), 0..20_000),
        3 => hostile_headers(),
        3 => mutated_head(),
    ]
}

/// Random per-letter casing of `name`.
fn recase(name: &str, flips: &[bool]) -> String {
    name.chars()
        .zip(flips.iter().cycle())
        .map(|(c, up)| {
            if *up {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

fn parse(raw: &[u8]) -> Result<Option<astar_http::parse::RequestHead>, HeadError> {
    read_head(&mut BufReader::new(raw), 8192)
}

/// Guards property 1: the `Some` branch must actually run, often.
#[test]
fn the_generators_reach_parsed_heads() {
    // A fixed seed: a counting test must not flake on the luck of the draw.
    let mut runner = TestRunner::deterministic();
    let (some, total) = (std::cell::Cell::new(0u32), std::cell::Cell::new(0u32));
    runner
        .run(&(any_input(), 64usize..10_000), |(data, max)| {
            total.set(total.get() + 1);
            if check(&data, max)? {
                some.set(some.get() + 1);
            }
            Ok(())
        })
        .unwrap();
    let (some, total) = (some.get(), total.get());
    assert!(
        some * 20 >= total * 3,
        "only {some} of {total} inputs parsed"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn arbitrary_input_obeys_the_head_contract(data in any_input(), max in 64usize..10_000) {
        check(&data, max)?;
    }

    #[test]
    fn well_formed_heads_round_trip(
        path in "/[a-z0-9/]{0,40}",
        query in proptest::option::of("[a-z0-9=&]{0,20}"),
        names in proptest::collection::vec("[A-Za-z][A-Za-z0-9-]{0,12}", 0..5),
        flips in proptest::collection::vec(any::<bool>(), 1..8),
        pad_l in "[ \t]{0,3}", pad_r in "[ \t]{0,3}",
        core in "[!-~]{0,20}",
    ) {
        let target = match &query {
            Some(q) => format!("{path}?{q}"),
            None => path.clone(),
        };
        let mut raw = format!("GET {target} HTTP/1.1\r\nHost: x\r\n");
        // The index prefix keeps generated names distinct from Host and friends.
        let sent: Vec<String> = names.iter().enumerate()
            .map(|(i, n)| recase(&format!("X{i}-{n}"), &flips)).collect();
        for n in &sent {
            let _ = write!(raw, "{n}:{pad_l}{core}{pad_r}\r\n");
        }
        raw.push_str("\r\n");
        let h = parse(raw.as_bytes()).unwrap().unwrap();
        prop_assert_eq!(h.method.as_str(), "GET");
        prop_assert_eq!(h.path(), path.as_str());
        prop_assert_eq!(h.query(), query.as_deref().unwrap_or(""));
        prop_assert_eq!(h.headers.len(), sent.len() + 1);
        for (i, n) in sent.iter().enumerate() {
            let (k, v) = &h.headers[i + 1];
            prop_assert_eq!(k, n);
            prop_assert_eq!(v.as_str(), core.as_str());
            prop_assert_eq!(h.header(&n.to_ascii_uppercase()), Some(core.as_str()));
        }
    }

    #[test]
    fn a_control_byte_anywhere_in_a_line_is_400(
        pos in 0usize..60,
        byte in prop_oneof![0u8..9, 11u8..32, Just(0x7fu8)],
    ) {
        let mut raw = BASE.to_vec();
        // Replace a byte that is not part of a line terminator; 13 makes a bare CR.
        let candidates: Vec<usize> = (0..raw.len()).filter(|&i| raw[i] != b'\r' && raw[i] != b'\n').collect();
        let i = candidates[pos % candidates.len()];
        raw[i] = byte;
        prop_assert_eq!(parse(&raw).unwrap_err().status(), Some(400));
    }

    #[test]
    fn a_bare_cr_anywhere_but_before_lf_is_400(at in 0usize..BASE.len()) {
        // Inserting a CR at any offset leaves one CR that is not the
        // terminator's single CR (at a CRLF it makes CR CR LF).
        let mut raw = BASE.to_vec();
        raw.insert(at, b'\r');
        prop_assert_eq!(parse(&raw).unwrap_err().status(), Some(400));
    }

    #[test]
    fn a_non_tchar_in_a_header_name_is_400(
        at in 0usize..4,
        bad in prop_oneof![Just(" "), Just("("), Just("/"), Just("@"), Just("\""), Just("\u{e9}")],
    ) {
        let mut name = String::from("X-Ab");
        name.insert_str(at + 1, bad); // never first: a leading space is folding
        let raw = format!("GET / HTTP/1.1\r\nHost: x\r\n{name}: v\r\n\r\n");
        prop_assert_eq!(parse(raw.as_bytes()).unwrap_err().status(), Some(400));
    }

    #[test]
    fn a_repeated_single_header_is_400(
        which in 0usize..3,
        f1 in proptest::collection::vec(any::<bool>(), 1..6),
        f2 in proptest::collection::vec(any::<bool>(), 1..6),
        v1 in "[0-9]{1,3}", v2 in "[0-9]{1,3}",
    ) {
        let name = ["Host", "Content-Length", "Transfer-Encoding"][which];
        let raw = format!(
            "POST / HTTP/1.1\r\n{}: {v1}\r\nX-Between: 1\r\n{}: {v2}\r\n\r\n",
            recase(name, &f1), recase(name, &f2),
        );
        prop_assert_eq!(parse(raw.as_bytes()).unwrap_err().status(), Some(400));
    }
}
