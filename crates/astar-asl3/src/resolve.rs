// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Node-number → IAX2 address resolution via the `AllStarLink` DNS directory.
//!
//! Primary: TXT lookup of `<node>.nodes.allstarlink.org`, whose record
//! carries the real `IP=`/`PT=` (nonstandard ports exist). Fallback: the A
//! record + `:4569` (the historical behaviour, and the only path for nodes
//! that are currently offline/unregistered and have no TXT record).

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use crate::Asl3Error;
use crate::dns::{build_txt_query, parse_txt_response};

const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const PUBLIC_RESOLVERS: [&str; 2] = ["8.8.8.8:53", "1.1.1.1:53"];

/// Extract `IP=`/`PT=` from the directory TXT strings. Pure; unit-tested.
fn addr_from_txt(strings: &[String]) -> Option<SocketAddr> {
    let ip: IpAddr = strings
        .iter()
        .find_map(|s| s.strip_prefix("IP="))?
        .parse()
        .ok()?;
    let port: u16 = strings
        .iter()
        .find_map(|s| s.strip_prefix("PT="))
        .and_then(|p| p.parse().ok())
        .unwrap_or(4569);
    Some(SocketAddr::new(ip, port))
}

/// System resolver from /etc/resolv.conf (works on macOS and Linux), then
/// public fallbacks. Pure parse; unit-tested via `parse_resolv_conf`.
fn parse_resolv_conf(text: &str) -> Option<SocketAddr> {
    text.lines().find_map(|l| {
        let l = l.trim();
        let ip = l.strip_prefix("nameserver")?.trim();
        ip.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 53))
    })
}

fn resolvers() -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(3);
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf")
        && let Some(a) = parse_resolv_conf(&text)
    {
        out.push(a);
    }
    for p in PUBLIC_RESOLVERS {
        if let Ok(a) = p.parse() {
            out.push(a);
        }
    }
    out
}

/// One TXT query against one resolver.
fn query_txt(resolver: SocketAddr, name: &str) -> Result<Vec<String>, Asl3Error> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).map_err(|e| Asl3Error::Dns(e.to_string()))?;
    sock.set_read_timeout(Some(DNS_TIMEOUT))
        .map_err(|e| Asl3Error::Dns(e.to_string()))?;
    #[allow(clippy::cast_possible_truncation)]
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x5353, |d| d.subsec_nanos() as u16);
    let q = build_txt_query(id, name);
    sock.send_to(&q, resolver)
        .map_err(|e| Asl3Error::Dns(e.to_string()))?;
    let mut buf = [0u8; 1500];
    let (n, _) = sock
        .recv_from(&mut buf)
        .map_err(|e| Asl3Error::Dns(e.to_string()))?;
    parse_txt_response(&buf[..n], id).ok_or_else(|| Asl3Error::Dns("malformed response".into()))
}

/// Resolve an `AllStarLink` node number to its IAX2 socket address.
///
/// # Errors
/// [`Asl3Error::NoRecords`] if the node has neither a TXT directory entry nor
/// an A record; [`Asl3Error::Dns`] only when every transport attempt failed.
pub fn resolve_node(node: &str) -> Result<SocketAddr, Asl3Error> {
    let name = format!("{node}.nodes.allstarlink.org");
    for resolver in resolvers() {
        // On Err, just try the next resolver.
        if let Ok(strings) = query_txt(resolver, &name) {
            if let Some(addr) = addr_from_txt(&strings) {
                return Ok(addr);
            }
            break; // resolver answered, no usable TXT → fall back to A
        }
    }
    // Fallback: A record + the standard port.
    let mut addrs = format!("{name}:4569")
        .to_socket_addrs()
        .map_err(|e| Asl3Error::Dns(e.to_string()))?;
    addrs.next().ok_or(Asl3Error::NoRecords {
        node: node.to_string(),
    })
}

/// Resolve an EXPLICIT dial address — `host:port` or a bare `host` (defaulting
/// to the IAX2 well-known port 4569) — to a [`SocketAddr`]. `host` may be an IP
/// literal or a DNS name (resolved via [`ToSocketAddrs`]).
///
/// Unlike [`resolve_node`], this performs **no** `AllStar` directory lookup: it is
/// the "dial exactly here" override (e.g. `127.0.0.1:4569`, or a LAN address
/// the registrar's public IP can't reach behind NAT — the hairpin case).
///
/// # Errors
/// [`Asl3Error::Dns`] if the address is empty, unparseable, or resolves to no
/// addresses.
pub fn resolve_addr(addr: &str) -> Result<SocketAddr, Asl3Error> {
    let addr = addr.trim();
    if addr.is_empty() {
        return Err(Asl3Error::Dns("empty dial address".to_string()));
    }
    // Bare host → append the IAX2 default port (mirrors the historical CLI
    // helper; a naive `:` check, so IPv6 literals must already carry a port).
    let with_port = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:4569")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| Asl3Error::Dns(format!("cannot resolve {addr:?}: {e}")))?
        .next()
        .ok_or_else(|| Asl3Error::Dns(format!("no address found for {addr:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_addr_host_port_passthrough() {
        let a = resolve_addr("127.0.0.1:4569").unwrap();
        assert_eq!(a.to_string(), "127.0.0.1:4569");
    }

    #[test]
    fn resolve_addr_bare_host_defaults_port_4569() {
        let a = resolve_addr("127.0.0.1").unwrap();
        assert!(a.ip().is_loopback());
        assert_eq!(a.port(), 4569);
    }

    #[test]
    fn resolve_addr_hostname_resolves_offline() {
        // `localhost` resolves via the system hosts file — no network needed.
        let a = resolve_addr("localhost:4569").unwrap();
        assert!(a.ip().is_loopback());
        assert_eq!(a.port(), 4569);
    }

    #[test]
    fn resolve_addr_rejects_empty_blank_and_bad_port() {
        // All fail fast, offline (no DNS round-trip).
        assert!(matches!(resolve_addr(""), Err(Asl3Error::Dns(_))));
        assert!(matches!(resolve_addr("   "), Err(Asl3Error::Dns(_))));
        assert!(matches!(
            resolve_addr("127.0.0.1:99999"),
            Err(Asl3Error::Dns(_))
        ));
    }

    #[test]
    fn txt_strings_yield_ip_and_port() {
        let s = vec![
            "NN=55553".to_string(),
            "IP=104.232.32.242".to_string(),
            "PT=4569".to_string(),
        ];
        assert_eq!(
            addr_from_txt(&s).expect("addr"),
            "104.232.32.242:4569".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn missing_port_defaults_to_4569_and_missing_ip_is_none() {
        let s = vec!["NN=1".to_string(), "IP=10.0.0.1".to_string()];
        assert_eq!(addr_from_txt(&s).unwrap().port(), 4569);
        assert!(addr_from_txt(&["NN=1".to_string()]).is_none());
    }

    #[test]
    fn resolv_conf_first_nameserver_wins() {
        let text = "# comment\nnameserver 192.168.1.201\nnameserver 8.8.4.4\n";
        assert_eq!(
            parse_resolv_conf(text).unwrap(),
            "192.168.1.201:53".parse().unwrap()
        );
        assert!(parse_resolv_conf("search lan\n").is_none());
    }
}
