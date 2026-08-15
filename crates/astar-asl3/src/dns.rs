// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Minimal RFC 1035 DNS codec for TXT queries — just enough for the
//! `AllStarLink` node directory. Pure functions; the UDP transport lives in
//! `resolve.rs`. No resolver crate dependency.

/// Build a single-question DNS query for `name` with QTYPE=TXT, QCLASS=IN,
/// recursion desired. `id` is caller-chosen (echoed by the server).
pub(crate) fn build_txt_query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // flags: RD
    q.extend_from_slice(&[0x00, 0x01]); // QDCOUNT = 1
    q.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // AN/NS/AR = 0
    for label in name.split('.').filter(|l| !l.is_empty()) {
        let bytes = label.as_bytes();
        debug_assert!(bytes.len() < 64);
        #[allow(clippy::cast_possible_truncation)] // guarded by the debug_assert above
        q.push(bytes.len() as u8);
        q.extend_from_slice(bytes);
    }
    q.push(0); // root label
    q.extend_from_slice(&[0x00, 0x10]); // QTYPE = TXT (16)
    q.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
    q
}

/// Skip over a (possibly compressed) NAME starting at `pos`; return the
/// position just after it. Compression pointers (0b11xxxxxx) end the name.
fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *buf.get(pos)?;
        if len & 0xC0 == 0xC0 {
            return Some(pos + 2); // pointer: two bytes, done
        }
        if len == 0 {
            return Some(pos + 1); // root label
        }
        pos += 1 + len as usize;
    }
}

/// Parse a DNS response to a TXT query: collect every character-string from
/// every TXT answer (answers may carry several strings each, e.g. the node
/// directory's `"NN=..." "IP=..." "PT=..."`). Returns `None` on malformed
/// input or a non-matching `id`.
pub(crate) fn parse_txt_response(buf: &[u8], id: u16) -> Option<Vec<String>> {
    if buf.len() < 12 || u16::from_be_bytes([buf[0], buf[1]]) != id {
        return None;
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut pos = 12;
    for _ in 0..qdcount {
        pos = skip_name(buf, pos)?;
        pos += 4; // QTYPE + QCLASS
    }
    let mut strings = Vec::new();
    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        let rtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
        let rdlength = u16::from_be_bytes([*buf.get(pos + 8)?, *buf.get(pos + 9)?]) as usize;
        pos += 10;
        let rdata = buf.get(pos..pos + rdlength)?;
        if rtype == 16 {
            // TXT RDATA: sequence of <len><bytes> character-strings.
            let mut r = 0;
            while r < rdata.len() {
                let n = rdata[r] as usize;
                let s = rdata.get(r + 1..r + 1 + n)?;
                strings.push(String::from_utf8_lossy(s).into_owned());
                r += 1 + n;
            }
        }
        pos += rdlength;
    }
    Some(strings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a canned response: echo the question, then `txt_answers` records
    /// (each a list of character-strings) with a compression pointer back to
    /// the question name (offset 12) — the shape real resolvers send.
    #[allow(clippy::cast_possible_truncation)] // test inputs are tiny by construction
    fn canned_response(id: u16, name: &str, txt_answers: &[&[&str]]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&id.to_be_bytes());
        r.extend_from_slice(&[0x81, 0x80]); // response, RD+RA
        r.extend_from_slice(&[0x00, 0x01]); // QD = 1
        r.extend_from_slice(&(txt_answers.len() as u16).to_be_bytes()); // AN
        r.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NS/AR
        for label in name.split('.') {
            r.push(label.len() as u8);
            r.extend_from_slice(label.as_bytes());
        }
        r.push(0);
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01]); // QTYPE/QCLASS
        for strings in txt_answers {
            r.extend_from_slice(&[0xC0, 0x0C]); // NAME: pointer to offset 12
            r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01]); // TYPE TXT, CLASS IN
            r.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // TTL 60
            let rdlen: usize = strings.iter().map(|s| 1 + s.len()).sum();
            r.extend_from_slice(&(rdlen as u16).to_be_bytes());
            for s in *strings {
                r.push(s.len() as u8);
                r.extend_from_slice(s.as_bytes());
            }
        }
        r
    }

    #[test]
    fn query_encodes_labels_and_txt_type() {
        let q = build_txt_query(0xBEEF, "55553.nodes.allstarlink.org");
        assert_eq!(&q[0..2], &[0xBE, 0xEF]);
        assert_eq!(q[12], 5); // "55553"
        assert_eq!(&q[13..18], b"55553");
        // ends with root + TXT + IN
        assert_eq!(&q[q.len() - 5..], &[0x00, 0x00, 0x10, 0x00, 0x01]);
    }

    #[test]
    fn parses_node_directory_txt_strings() {
        // Live-verified format (2026-06-11): three separate character-strings.
        let resp = canned_response(
            7,
            "55553.nodes.allstarlink.org",
            &[&["NN=55553", "IP=104.232.32.242", "PT=4569"]],
        );
        let strings = parse_txt_response(&resp, 7).expect("parses");
        assert_eq!(strings, vec!["NN=55553", "IP=104.232.32.242", "PT=4569"]);
    }

    #[test]
    fn wrong_id_and_truncation_are_rejected() {
        let resp = canned_response(7, "x.y", &[&["a"]]);
        assert!(parse_txt_response(&resp, 8).is_none(), "id mismatch");
        assert!(parse_txt_response(&resp[..10], 7).is_none(), "short buffer");
    }

    #[test]
    fn no_answers_yields_empty_list() {
        let resp = canned_response(7, "77777.nodes.allstarlink.org", &[]);
        assert_eq!(
            parse_txt_response(&resp, 7).expect("parses"),
            Vec::<String>::new()
        );
    }
}
