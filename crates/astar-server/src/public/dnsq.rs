// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Just enough DNS to ask one server for one name's TXT records, over UDP.
//!
//! DNS-01 asks this of the zone's **authoritative** nameservers directly, so
//! "the record is visible" means what the CA's own lookup will see, not
//! what some cache happens to hold. No recursion is requested, and nothing
//! here follows a referral.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Build a query for `name`'s TXT records with id `id`, RD clear.
#[must_use]
pub fn build_query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x00, 0x00]); // standard query, no recursion
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // QD=1, AN=NS=AR=0
    for label in name.trim_end_matches('.').split('.') {
        let len = u8::try_from(label.len()).unwrap_or(63).min(63);
        q.push(len);
        q.extend_from_slice(&label.as_bytes()[..usize::from(len)]);
    }
    q.push(0);
    q.extend_from_slice(&[0, 16, 0, 1]); // TXT, IN
    q
}

/// Read a (possibly compressed) name at `i`: its labels lower-cased and
/// dot-joined, and the offset just past it in the message.
fn read_name(msg: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    // Every pointer must go backwards, so a loop of pointers cannot spin.
    let mut limit = i;
    loop {
        let len = *msg.get(i)?;
        if len & 0xC0 == 0xC0 {
            let to = usize::from(u16::from_be_bytes([len & 0x3F, *msg.get(i + 1)?]));
            if to >= limit {
                return None;
            }
            end.get_or_insert(i + 2);
            limit = to;
            i = to;
            continue;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(i + 1)));
        }
        let label = msg.get(i + 1..i + 1 + usize::from(len))?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        i += 1 + usize::from(len);
    }
}

/// `name` as [`read_name`] spells it: lower case, no final dot.
fn canonical(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// The TXT strings in a response to the query with `id` for `name`. Each
/// record's character-strings are joined, as DNS-01 values are. Only
/// answers whose owner is `name`, of type TXT and class IN, count.
///
/// # Errors
/// A response that is not for `id` and `name` (`InvalidData`, which
/// [`query_txt`] treats as not its answer), is an error, or does not parse.
pub fn parse_txt_answers(id: u16, name: &str, msg: &[u8]) -> io::Result<Vec<String>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed DNS response");
    if msg.len() < 12 || msg[..2] != id.to_be_bytes() || msg[2] & 0x80 == 0 {
        return Err(bad());
    }
    let want = canonical(name);
    // The echoed question must be exactly the one asked.
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    if qd != 1 {
        return Err(bad());
    }
    let (qname, i) = read_name(msg, 12).ok_or_else(bad)?;
    if qname != want || msg.get(i..i + 4).ok_or_else(bad)? != [0, 16, 0, 1] {
        return Err(bad());
    }
    let mut i = i + 4;
    let rcode = msg[3] & 0x0F;
    if rcode == 3 {
        return Ok(Vec::new()); // NXDOMAIN: nothing there yet
    }
    if rcode != 0 {
        return Err(io::Error::other(format!("DNS rcode {rcode}")));
    }
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut out = Vec::new();
    for _ in 0..an {
        let (owner, next) = read_name(msg, i).ok_or_else(bad)?;
        i = next;
        let head = msg.get(i..i + 10).ok_or_else(bad)?;
        let rtype = u16::from_be_bytes([head[0], head[1]]);
        let class = u16::from_be_bytes([head[2], head[3]]);
        let rdlen = usize::from(u16::from_be_bytes([head[8], head[9]]));
        i += 10;
        let rdata = msg.get(i..i + rdlen).ok_or_else(bad)?;
        i += rdlen;
        if rtype != 16 || class != 1 || owner != want {
            continue;
        }
        let mut s = Vec::new();
        let mut j = 0;
        while j < rdata.len() {
            let l = usize::from(rdata[j]);
            s.extend_from_slice(rdata.get(j + 1..j + 1 + l).ok_or_else(bad)?);
            j += 1 + l;
        }
        out.push(String::from_utf8_lossy(&s).into_owned());
    }
    Ok(out)
}

/// A query id from the system's secure random source, so an off-path
/// sender cannot guess it.
fn query_id() -> io::Result<u16> {
    use ring::rand::SecureRandom as _;
    let mut b = [0u8; 2];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| io::Error::other("no random source"))?;
    Ok(u16::from_be_bytes(b))
}

/// Ask `server` for `name`'s TXT records.
///
/// The socket is connected to `server`, so datagrams from anywhere else
/// are dropped; a datagram from it that is not the answer to this query
/// (another id, another question, garbage) is skipped. `timeout` bounds
/// the whole exchange, however many datagrams arrive.
///
/// # Errors
/// No answer within `timeout`, an error response, or nothing that parses.
pub fn query_txt(server: SocketAddr, name: &str, timeout: Duration) -> io::Result<Vec<String>> {
    let deadline = Instant::now() + timeout;
    let bind = if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let sock = UdpSocket::bind(bind)?;
    sock.connect(server)?;
    let id = query_id()?;
    sock.send(&build_query(id, name))?;
    let mut buf = [0u8; 4096];
    let mut last = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(last.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "no DNS answer in time")
            }));
        }
        sock.set_read_timeout(Some(left))?;
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue; // the deadline check above ends it
            }
            Err(e) => return Err(e),
        };
        match parse_txt_answers(id, name, &buf[..n]) {
            Err(e) if e.kind() == io::ErrorKind::InvalidData => last = Some(e),
            other => return other,
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A one-zone TXT server on 127.0.0.1 for tests.
    use super::*;
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    pub struct FakeDns {
        pub addr: SocketAddr,
        pub records: Arc<Mutex<HashMap<String, Vec<String>>>>,
        stop: Arc<AtomicBool>,
    }

    impl Drop for FakeDns {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn qname(msg: &[u8]) -> Option<(String, usize)> {
        let mut i = 12;
        let mut labels = Vec::new();
        loop {
            let l = usize::from(*msg.get(i)?);
            if l == 0 {
                return Some((labels.join("."), i + 1));
            }
            labels.push(String::from_utf8_lossy(msg.get(i + 1..i + 1 + l)?).to_lowercase());
            i += 1 + l;
        }
    }

    impl FakeDns {
        pub fn start() -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let addr = sock.local_addr().unwrap();
            let records: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));
            let (r, s) = (Arc::clone(&records), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut buf = [0u8; 512];
                while !s.load(Ordering::Relaxed) {
                    let Ok((n, from)) = sock.recv_from(&mut buf) else {
                        continue;
                    };
                    let q = &buf[..n];
                    let Some((name, end)) = qname(q) else {
                        continue;
                    };
                    let answers = r.lock().unwrap().get(&name).cloned().unwrap_or_default();
                    let mut resp = Vec::new();
                    resp.extend_from_slice(&q[..2]);
                    resp.extend_from_slice(&[0x84, 0x00, 0, 1]);
                    resp.extend_from_slice(&u16::try_from(answers.len()).unwrap().to_be_bytes());
                    resp.extend_from_slice(&[0, 0, 0, 0]);
                    resp.extend_from_slice(&q[12..end + 4]);
                    for a in &answers {
                        resp.extend_from_slice(&[0xC0, 12, 0, 16, 0, 1, 0, 0, 0, 60]);
                        let rd = u16::try_from(a.len() + 1).unwrap();
                        resp.extend_from_slice(&rd.to_be_bytes());
                        resp.push(u8::try_from(a.len()).unwrap());
                        resp.extend_from_slice(a.as_bytes());
                    }
                    let _ = sock.send_to(&resp, from);
                }
            });
            Self {
                addr,
                records,
                stop,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_txt_query_round_trips_against_the_fake_server() {
        let dns = fake::FakeDns::start();
        dns.records.lock().unwrap().insert(
            "_acme-challenge.69586.example.net".into(),
            vec!["abc123".into(), "second".into()],
        );
        let got = query_txt(
            dns.addr,
            "_acme-challenge.69586.EXAMPLE.net.",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(got, ["abc123", "second"]);
        let none = query_txt(
            dns.addr,
            "_acme-challenge.other.example.net",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(none, [] as [std::string::String; 0]);
    }

    #[test]
    fn queries_ask_for_txt_without_recursion() {
        let q = build_query(0x1234, "a.example.net");
        assert_eq!(&q[..4], &[0x12, 0x34, 0, 0]);
        assert_eq!(&q[q.len() - 4..], &[0, 16, 0, 1]);
        assert!(
            parse_txt_answers(0x9999, "a.example.net", &q).is_err(),
            "a query is not an answer"
        );
    }

    fn encode(name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for l in name.split('.') {
            v.push(u8::try_from(l.len()).unwrap());
            v.extend_from_slice(l.as_bytes());
        }
        v.push(0);
        v
    }

    /// A response with id `id` to the question `qname`/`qtype`, carrying
    /// `answers` of (owner, type, class, text). An owner of `"@"` is a
    /// pointer to the question name.
    fn response(id: u16, qname: &str, qtype: u16, answers: &[(&str, u16, u16, &str)]) -> Vec<u8> {
        let mut m = id.to_be_bytes().to_vec();
        m.extend_from_slice(&[0x84, 0x00, 0, 1]);
        m.extend_from_slice(&u16::try_from(answers.len()).unwrap().to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0]);
        m.extend_from_slice(&encode(qname));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        for (owner, t, class, text) in answers {
            if *owner == "@" {
                m.extend_from_slice(&[0xC0, 12]);
            } else {
                m.extend_from_slice(&encode(owner));
            }
            m.extend_from_slice(&t.to_be_bytes());
            m.extend_from_slice(&class.to_be_bytes());
            m.extend_from_slice(&[0, 0, 0, 60]);
            m.extend_from_slice(&u16::try_from(text.len() + 1).unwrap().to_be_bytes());
            m.push(u8::try_from(text.len()).unwrap());
            m.extend_from_slice(text.as_bytes());
        }
        m
    }

    #[test]
    fn only_answers_to_the_question_asked_are_taken() {
        let name = "_acme-challenge.a.example.net";
        let ok = response(7, name, 16, &[("@", 16, 1, "v")]);
        assert_eq!(parse_txt_answers(7, name, &ok).unwrap(), ["v"]);
        // The question is matched without regard to case or a final dot.
        let upper = response(7, "_ACME-challenge.A.example.net", 16, &[("@", 16, 1, "v")]);
        assert_eq!(
            parse_txt_answers(7, &format!("{name}."), &upper).unwrap(),
            ["v"]
        );

        // A response to some other question is not this one's answer.
        let other_q = response(7, "_acme-challenge.b.example.net", 16, &[("@", 16, 1, "v")]);
        assert!(parse_txt_answers(7, name, &other_q).is_err());
        let other_type = response(7, name, 1, &[("@", 16, 1, "v")]);
        assert!(parse_txt_answers(7, name, &other_type).is_err());

        // Records for another owner, another class or another type are
        // skipped; only the queried name's IN TXT counts.
        let mixed = response(
            7,
            name,
            16,
            &[
                ("_acme-challenge.b.example.net", 16, 1, "elsewhere"),
                ("@", 16, 3, "chaos"),
                ("@", 1, 1, "an-a-record"),
                ("_ACME-CHALLENGE.a.example.net", 16, 1, "mine"),
            ],
        );
        assert_eq!(parse_txt_answers(7, name, &mixed).unwrap(), ["mine"]);
    }

    /// A UDP server on 127.0.0.1 that, if `answer`, sends the query's sender
    /// a datagram with the wrong id and then the real answer; then floods
    /// the sender from a second socket for `flood`.
    fn hostile(answer: bool, flood: Duration) -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                return;
            };
            let id = u16::from_be_bytes([buf[0], buf[1]]);
            let name = "a.example.net";
            assert_eq!(&buf[12..n - 4], encode(name).as_slice());
            let wrong = response(id ^ 0xFFFF, name, 16, &[("@", 16, 1, "forged")]);
            if answer {
                let _ = sock.send_to(&wrong, from);
                let _ = sock.send_to(&response(id, name, 16, &[("@", 16, 1, "real")]), from);
            }
            let other = UdpSocket::bind("127.0.0.1:0").unwrap();
            let end = std::time::Instant::now() + flood;
            while std::time::Instant::now() < end {
                let _ = other.send_to(&wrong, from);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        addr
    }

    #[test]
    fn a_forged_id_is_ignored_and_the_real_answer_taken() {
        let server = hostile(true, Duration::ZERO);
        let got = query_txt(server, "a.example.net", Duration::from_secs(2)).unwrap();
        assert_eq!(got, ["real"]);
    }

    #[test]
    fn a_flood_of_stray_datagrams_cannot_stretch_the_timeout() {
        let server = hostile(false, Duration::from_secs(5));
        let started = std::time::Instant::now();
        let e = query_txt(server, "a.example.net", Duration::from_millis(400)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "took {:?}: {e}",
            started.elapsed()
        );
    }
}
