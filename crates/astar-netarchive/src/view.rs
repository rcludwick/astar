// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The JSON the recordings page reads: the list of nets and one session.
//!
//! Callsigns come off the air, so they travel here as plain JSON strings and
//! the page sets them with `textContent`. Nothing in this module builds
//! markup. Times are formatted here, at the session's own UTC offset, so
//! every viewer sees the net's local time.

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::fmt::temporal::Pieces;
use jiff::tz::{Offset, TimeZone};
use serde::Serialize;

use crate::callsign::{base_call, split_call};
use crate::index::{Access, Archive, SessionEntry, is_public};
use crate::model::{LogKind, LogRow, SessionMeta};
use crate::summary::{carries_callsigns, overs_from_rows};

/// An over shorter than this is a key-up, drawn as a chip rather than a
/// bubble.
pub const SHORT_OVER_MS: u64 = 1_500;

/// What the serving side adds to a view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewOpts {
    /// Where an admin posts a delete; `None` when this server offers none.
    pub delete_url: Option<String>,
    /// Where a signed-out viewer can sign in; `None` when there is nowhere.
    pub sign_in_url: Option<String>,
    /// The `my_callsigns` to use for a session that recorded none.
    pub fallback_my_calls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListView {
    pub nets: Vec<NetView>,
    pub signed_in: bool,
    pub sign_in_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetView {
    pub name: String,
    pub slug: String,
    pub network: String,
    pub network_label: String,
    /// Newest first.
    pub sessions: Vec<SessionCard>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCard {
    pub url: String,
    pub date: String,
    pub clock: String,
    pub duration_ms: u64,
    pub checkins: u32,
    pub overs: u32,
    pub public: bool,
    pub recording: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioSrc {
    pub url: String,
    pub mime: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // The page's JSON: each flag is its own switch.
pub struct SessionView {
    pub net: String,
    pub network: String,
    pub network_label: String,
    pub target: String,
    pub date: String,
    pub clock: String,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub checkins: u32,
    pub my_side: bool,
    pub my_calls: Vec<String>,
    pub carries_callsigns: bool,
    pub public: bool,
    pub recording: bool,
    pub audio: AudioSrc,
    pub items: Vec<Item>,
    pub can_delete: bool,
    pub delete_url: Option<String>,
    pub net_slug: String,
    pub session_slug: String,
}

/// One line of a session's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Item {
    Over {
        /// 1-based, among the session's overs.
        seq: u32,
        /// The base callsign, `None` when nobody was named.
        call: Option<String>,
        suffix: Option<String>,
        /// Into `session.flac`.
        offset_ms: u64,
        duration_ms: u64,
        clock: String,
        short: bool,
        file_url: String,
        /// Empty when the over's file is missing.
        peaks: Vec<u8>,
    },
    Event {
        /// `joined`, `dropped`, `rejoined`, `left` or `error`.
        kind: String,
        clock: String,
        offset_ms: u64,
        /// When a `dropped` was followed by a `rejoined`: the rejoin's clock.
        back_clock: Option<String>,
        /// The drop reason or error message; signed-in viewers only.
        detail: Option<String>,
    },
}

/// The name a network goes by on the page.
fn network_label(network: &str) -> String {
    match network {
        "m17" => "M17".to_owned(),
        "dstar" => "D-Star".to_owned(),
        "allstar" => "AllStar".to_owned(),
        other => other.to_owned(),
    }
}

/// Percent-encode everything but RFC 3986's unreserved characters.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

pub(crate) fn session_url(e: &SessionEntry) -> String {
    format!("/nets/{}/{}/", encode(&e.net_slug), encode(&e.session_slug))
}

/// The UTC offset in `window_start`; UTC when it has none or does not parse.
fn offset_of(meta: &SessionMeta) -> Offset {
    Pieces::parse(&meta.window_start)
        .ok()
        .and_then(|p| p.offset())
        .map_or(Offset::UTC, |o| o.to_numeric_offset())
}

fn stamp(s: &str) -> Option<Timestamp> {
    s.parse().ok()
}

fn clock_at(t: Option<Timestamp>, off: Offset) -> String {
    t.map(|t| {
        t.to_zoned(TimeZone::fixed(off))
            .strftime("%H:%M:%S")
            .to_string()
    })
    .unwrap_or_default()
}

fn date_at(t: Option<Timestamp>, off: Offset) -> String {
    t.map(|t| {
        t.to_zoned(TimeZone::fixed(off))
            .strftime("%a %-d %b %Y")
            .to_string()
    })
    .unwrap_or_default()
}

/// Milliseconds from `from` to `to`, 0 when either is missing or `to` is
/// earlier.
fn ms_between(from: Option<Timestamp>, to: Option<Timestamp>) -> u64 {
    match (from, to) {
        (Some(f), Some(t)) => u64::try_from(t.duration_since(f).as_millis()).unwrap_or(0),
        _ => 0,
    }
}

/// `ended_at − started_at`; a session with no usable `ended_at` runs to its
/// last row.
fn duration_ms(meta: &SessionMeta, rows: impl FnOnce() -> Vec<LogRow>) -> u64 {
    let start = stamp(&meta.started_at);
    if let Some(end) = meta.ended_at.as_deref().and_then(stamp) {
        return ms_between(start, Some(end));
    }
    let last = rows().last().and_then(|r| stamp(&r.t));
    ms_between(start, last)
}

fn card(archive: &Archive, e: &SessionEntry) -> SessionCard {
    let m = &e.meta;
    let off = offset_of(m);
    let started = stamp(&m.started_at);
    let recording = m.ended.is_none();
    let overs = if recording {
        let rows = archive.rows(e);
        u32::try_from(overs_from_rows(&rows).len()).unwrap_or(u32::MAX)
    } else {
        m.overs
    };
    SessionCard {
        url: session_url(e),
        date: date_at(started, off),
        clock: clock_at(started, off),
        duration_ms: duration_ms(m, || archive.rows(e)),
        checkins: archive.checkins(e),
        overs,
        public: is_public(m),
        recording,
    }
}

/// Every net with at least one session `access` may see. Sessions are newest
/// first, and nets are in the order of their newest session.
#[must_use]
pub fn list_view(archive: &Archive, access: Access, opts: &ViewOpts) -> ListView {
    let mut entries: Vec<SessionEntry> = archive
        .scan()
        .into_iter()
        .filter(|e| access.may_see(&e.meta))
        .collect();
    entries.sort_by(|a, b| {
        stamp(&b.meta.started_at)
            .cmp(&stamp(&a.meta.started_at))
            .then_with(|| b.session_slug.cmp(&a.session_slug))
    });
    let mut nets: Vec<(std::path::PathBuf, NetView)> = Vec::new();
    for e in &entries {
        let net_dir = e.dir.parent().map(std::path::Path::to_path_buf);
        let Some(net_dir) = net_dir else { continue };
        let c = card(archive, e);
        if let Some((_, n)) = nets.iter_mut().find(|(d, _)| *d == net_dir) {
            n.sessions.push(c);
        } else {
            nets.push((
                net_dir,
                NetView {
                    name: e.meta.net.clone(),
                    slug: e.net_slug.clone(),
                    network: e.meta.network.clone(),
                    network_label: network_label(&e.meta.network),
                    sessions: vec![c],
                },
            ));
        }
    }
    let signed_in = access != Access::Anonymous;
    ListView {
        nets: nets.into_iter().map(|(_, n)| n).collect(),
        signed_in,
        sign_in_url: if signed_in {
            None
        } else {
            opts.sign_in_url.clone()
        },
    }
}

fn my_calls(meta: &SessionMeta, opts: &ViewOpts) -> Vec<String> {
    if !carries_callsigns(&meta.network) {
        return Vec::new();
    }
    let source = if meta.my_callsigns.is_empty() {
        &opts.fallback_my_calls
    } else {
        &meta.my_callsigns
    };
    let mut out: Vec<String> = Vec::new();
    for b in source.iter().filter_map(|c| base_call(c)) {
        if !out.contains(&b) {
            out.push(b);
        }
    }
    out
}

/// The timeline: overs and events, ordered by where they fall in the
/// session.
fn items(
    archive: &Archive,
    e: &SessionEntry,
    rows: &[LogRow],
    access: Access,
    off: Offset,
) -> Vec<Item> {
    let signed_in = access != Access::Anonymous;
    let start = stamp(&e.meta.started_at);
    let overs = overs_from_rows(rows);
    let peaks = archive.peaks(e, &overs);
    let base = session_url(e);
    let mut out = Vec::new();
    let mut seq = 0u32;
    let mut skip = vec![false; rows.len()];
    for (i, row) in rows.iter().enumerate() {
        if skip[i] {
            continue;
        }
        let t = stamp(&row.t);
        let event = |kind: &str, back_clock: Option<String>, detail: Option<String>| Item::Event {
            kind: kind.to_owned(),
            clock: clock_at(t, off),
            offset_ms: ms_between(start, t),
            back_clock,
            detail,
        };
        match &row.kind {
            LogKind::Over {
                start: over_start,
                duration_ms,
                offset_ms,
                talker,
                file,
                ..
            } => {
                seq = seq.saturating_add(1);
                let call = talker.as_deref().and_then(split_call);
                out.push(Item::Over {
                    seq,
                    call: call.as_ref().map(|c| c.base.clone()),
                    suffix: call.and_then(|c| c.suffix),
                    offset_ms: *offset_ms,
                    duration_ms: *duration_ms,
                    clock: clock_at(stamp(over_start), off),
                    short: *duration_ms < SHORT_OVER_MS,
                    file_url: format!("{base}overs/{}", encode(file)),
                    peaks: peaks.overs.get(file).cloned().unwrap_or_default(),
                });
            }
            LogKind::Joined => out.push(event("joined", None, None)),
            LogKind::Rejoined => out.push(event("rejoined", None, None)),
            LogKind::Left => out.push(event("left", None, None)),
            LogKind::Error { message } => {
                if signed_in {
                    out.push(event("error", None, Some(message.clone())));
                }
            }
            LogKind::Dropped { reason } => {
                // The next event that is neither an over nor an error: a
                // rejoin there closes this drop, and the two draw as one
                // divider. Errors are skipped because an anonymous viewer
                // never sees them, and would otherwise get a stray rejoin.
                let next =
                    rows.iter().enumerate().skip(i + 1).find(|(_, r)| {
                        !matches!(r.kind, LogKind::Over { .. } | LogKind::Error { .. })
                    });
                let back = match next {
                    Some((j, r)) if matches!(r.kind, LogKind::Rejoined) => {
                        skip[j] = true;
                        Some(clock_at(stamp(&r.t), off))
                    }
                    _ => None,
                };
                out.push(event("dropped", back, signed_in.then(|| reason.clone())));
            }
        }
    }
    out.sort_by_key(|item| match item {
        Item::Over { offset_ms, .. } | Item::Event { offset_ms, .. } => *offset_ms,
    });
    out
}

/// One session, as `access` may see it. The caller has already checked
/// [`Access::may_see`]: this function shapes the view, it does not gate it.
#[must_use]
pub fn session_view(
    archive: &Archive,
    e: &SessionEntry,
    access: Access,
    opts: &ViewOpts,
) -> SessionView {
    let m = &e.meta;
    let off = offset_of(m);
    let rows = archive.rows(e);
    let started = stamp(&m.started_at);
    let carries = carries_callsigns(&m.network);
    let is_admin = access == Access::Admin;
    let delete_url = if is_admin {
        opts.delete_url.clone()
    } else {
        None
    };
    SessionView {
        net: m.net.clone(),
        network: m.network.clone(),
        network_label: network_label(&m.network),
        target: m.target.clone(),
        date: date_at(started, off),
        clock: clock_at(started, off),
        duration_ms: duration_ms(m, || rows.clone()),
        sample_rate: m.sample_rate,
        checkins: archive.checkins(e),
        my_side: m.my_side && carries,
        my_calls: my_calls(m, opts),
        carries_callsigns: carries,
        public: is_public(m),
        recording: m.ended.is_none(),
        audio: AudioSrc {
            url: format!("{}session.flac", session_url(e)),
            mime: "audio/flac".to_owned(),
        },
        items: items(archive, e, &rows, access, off),
        can_delete: is_admin && delete_url.is_some() && m.ended.is_some(),
        delete_url,
        net_slug: e.net_slug.clone(),
        session_slug: e.session_slug.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Fixture, write_fixture};
    use crate::log::{LOG_FILE, META_FILE, append_row, read_meta, write_meta};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Setup {
        root: PathBuf,
        f: Fixture,
        a: Archive,
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn setup(tag: &str) -> Setup {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "netarchive-view-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let f = write_fixture(&root);
        let a = Archive::new(root.clone());
        Setup { root, f, a }
    }

    const THURSDAY: &str = "thursday-d-star-net";

    fn opts() -> ViewOpts {
        ViewOpts {
            delete_url: Some("/nets/delete".into()),
            sign_in_url: Some("/signin".into()),
            fallback_my_calls: Vec::new(),
        }
    }

    fn overs(v: &SessionView) -> Vec<&Item> {
        v.items
            .iter()
            .filter(|i| matches!(i, Item::Over { .. }))
            .collect()
    }

    fn events(v: &SessionView) -> Vec<(String, Option<String>, Option<String>)> {
        v.items
            .iter()
            .filter_map(|i| match i {
                Item::Event {
                    kind,
                    back_clock,
                    detail,
                    ..
                } => Some((kind.clone(), back_clock.clone(), detail.clone())),
                Item::Over { .. } => None,
            })
            .collect()
    }

    #[test]
    fn stage1_shaped_session_renders() {
        let s = setup("stage1");
        let e = s.a.find(THURSDAY, &s.f.private_session).unwrap();
        let v = session_view(&s.a, &e, Access::Admin, &opts());
        assert_eq!(v.checkins, 4);
        assert_eq!(v.network_label, "D-Star");
        assert_eq!(v.date, "Thu 5 Nov 2026");
        assert_eq!(v.clock, "19:00:00");
        assert_eq!(v.duration_ms, 40_000);
        assert!(!v.public && !v.recording);
        assert!(v.can_delete);
        assert_eq!(v.audio.mime, "audio/flac");
        assert_eq!(
            v.audio.url,
            format!("/nets/{THURSDAY}/{}/session.flac", s.f.private_session)
        );
        let o = overs(&v);
        assert_eq!(o.len(), 5);
        let Item::Over {
            seq,
            call,
            suffix,
            short,
            clock,
            file_url,
            peaks,
            offset_ms,
            ..
        } = o[1]
        else {
            unreachable!()
        };
        assert_eq!(*seq, 2);
        assert_eq!(call.as_deref(), Some("KX0AAA"));
        assert_eq!(suffix.as_deref(), Some("D"));
        assert!(*short);
        assert_eq!(clock, "19:00:10");
        assert_eq!(*offset_ms, 10_000);
        assert_eq!(
            file_url,
            &format!(
                "/nets/{THURSDAY}/{}/overs/002%2019-00-10%20KX0AAA.flac",
                s.f.private_session
            )
        );
        assert_eq!(peaks.len(), 48);
        for over in &o {
            let Item::Over {
                peaks,
                short,
                duration_ms,
                ..
            } = over
            else {
                unreachable!()
            };
            assert_eq!(peaks.len(), 48);
            assert_eq!(*short, *duration_ms < SHORT_OVER_MS);
        }
        let Item::Over { call, suffix, .. } = o[2] else {
            unreachable!()
        };
        assert_eq!(
            (call.as_deref(), suffix.as_deref()),
            (Some("KX0BBB"), Some("1"))
        );
        // Nothing was written into the archive.
        assert!(!e.dir.join(crate::peaks::PEAKS_FILE).exists());
        // Items are in session order.
        let offs: Vec<u64> = v
            .items
            .iter()
            .map(|i| match i {
                Item::Over { offset_ms, .. } | Item::Event { offset_ms, .. } => *offset_ms,
            })
            .collect();
        assert!(offs.windows(2).all(|w| w[0] <= w[1]), "{offs:?}");
    }

    #[test]
    fn drop_and_rejoin_make_one_divider() {
        let s = setup("drop");
        let e = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        let v = session_view(&s.a, &e, Access::Viewer, &opts());
        let ev = events(&v);
        let kinds: Vec<&str> = ev.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(kinds, ["joined", "dropped", "left"]);
        assert_eq!(ev[1].1.as_deref(), Some("18:55:43"));
        assert_eq!(
            ev[1].2.as_deref(),
            Some("keepalive lost from 203.0.113.7:17000")
        );
        let dropped = v
            .items
            .iter()
            .position(|i| matches!(i, Item::Event { kind, .. } if kind == "dropped"))
            .unwrap();
        let Item::Event {
            clock, offset_ms, ..
        } = &v.items[dropped]
        else {
            unreachable!()
        };
        assert_eq!((clock.as_str(), *offset_ms), ("18:55:41", 41_000));
        // Six overs before the drop, four after.
        assert_eq!(dropped, 7);
        assert!(!v.can_delete, "a viewer cannot delete");
        assert_eq!(v.delete_url, None);
        // A rejoin with no drop before it stands alone.
        append_row(
            &e.dir,
            &LogRow {
                t: "2026-11-04T01:56:29Z".into(),
                kind: LogKind::Rejoined,
            },
        )
        .unwrap();
        let v = session_view(&s.a, &e, Access::Viewer, &opts());
        let kinds: Vec<String> = events(&v).into_iter().map(|(k, _, _)| k).collect();
        assert_eq!(kinds, ["joined", "dropped", "left", "rejoined"]);
    }

    #[test]
    fn an_error_between_drop_and_rejoin_does_not_split_them() {
        let s = setup("droperr");
        let e = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        for (t, kind) in [
            (
                "2026-11-04T01:56:31Z",
                LogKind::Dropped {
                    reason: "keepalive lost".into(),
                },
            ),
            (
                "2026-11-04T01:56:33Z",
                LogKind::Error {
                    message: "reconnect refused".into(),
                },
            ),
            ("2026-11-04T01:56:35Z", LogKind::Rejoined),
        ] {
            append_row(&e.dir, &LogRow { t: t.into(), kind }).unwrap();
        }
        let anon = session_view(&s.a, &e, Access::Anonymous, &opts());
        let ev = events(&anon);
        let kinds: Vec<&str> = ev.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(kinds, ["joined", "dropped", "left", "dropped"]);
        assert_eq!(ev[3].1.as_deref(), Some("18:56:35"));
        let admin = session_view(&s.a, &e, Access::Admin, &opts());
        let kinds: Vec<String> = events(&admin).into_iter().map(|(k, _, _)| k).collect();
        assert_eq!(kinds, ["joined", "dropped", "left", "dropped", "error"]);
    }

    #[test]
    fn anonymous_sees_no_error_rows_or_reasons() {
        let s = setup("anon");
        let thu = s.a.find(THURSDAY, &s.f.private_session).unwrap();
        let admin = session_view(&s.a, &thu, Access::Admin, &opts());
        assert!(
            events(&admin)
                .iter()
                .any(|(k, _, d)| k == "error" && d.as_deref().is_some_and(|d| d.contains("/var")))
        );
        let anon = session_view(&s.a, &thu, Access::Anonymous, &opts());
        assert!(
            events(&anon)
                .iter()
                .all(|(k, _, d)| k != "error" && d.is_none())
        );
        let tue = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        let anon = session_view(&s.a, &tue, Access::Anonymous, &opts());
        let ev = events(&anon);
        assert!(ev.iter().any(|(k, b, _)| k == "dropped" && b.is_some()));
        assert!(ev.iter().all(|(_, _, d)| d.is_none()));
        assert!(!anon.can_delete);
        assert_eq!(anon.delete_url, None);
        let json = serde_json::to_string(&anon).unwrap();
        assert!(!json.contains("203.0.113.7"), "{json}");
    }

    #[test]
    fn list_hides_private_from_anonymous_and_sorts_newest_first() {
        let s = setup("list");
        let anon = list_view(&s.a, Access::Anonymous, &opts());
        assert!(!anon.signed_in);
        assert_eq!(anon.sign_in_url.as_deref(), Some("/signin"));
        assert_eq!(anon.nets.len(), 1);
        let n = &anon.nets[0];
        assert_eq!(
            (n.name.as_str(), n.slug.as_str(), n.network_label.as_str()),
            ("Tuesday M17 net", "tuesday-m17-net", "M17")
        );
        let c = &n.sessions[0];
        assert_eq!(c.url, "/nets/tuesday-m17-net/2026-11-03-1855/");
        assert_eq!(c.date, "Tue 3 Nov 2026");
        assert_eq!(c.clock, "18:55:00");
        assert_eq!(c.duration_ms, 90_000);
        assert_eq!((c.checkins, c.overs), (4, 10));
        assert!(c.public && !c.recording);
        let json = serde_json::to_string(&anon).unwrap();
        assert!(!json.contains("Thursday"), "{json}");

        let viewer = list_view(&s.a, Access::Viewer, &opts());
        assert!(viewer.signed_in);
        assert_eq!(viewer.sign_in_url, None);
        let names: Vec<&str> = viewer.nets.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Thursday D-Star net", "Tuesday M17 net"]);
        let thu: Vec<&str> = viewer.nets[0]
            .sessions
            .iter()
            .map(|c| c.url.as_str())
            .collect();
        assert_eq!(
            thu,
            [
                format!("/nets/{THURSDAY}/{}/", s.f.open_session),
                format!("/nets/{THURSDAY}/{}/", s.f.private_session),
            ]
        );
    }

    #[test]
    fn open_session_is_listed_as_recording() {
        let s = setup("open");
        let viewer = list_view(&s.a, Access::Admin, &opts());
        let c = &viewer.nets[0].sessions[0];
        assert!(c.recording);
        assert_eq!(c.overs, 2);
        assert_eq!(c.checkins, 2);
        // It runs to its last row: the second over ends at 16 s.
        assert_eq!(c.duration_ms, 16_000);
        let e = s.a.find(THURSDAY, &s.f.open_session).unwrap();
        let v = session_view(&s.a, &e, Access::Admin, &opts());
        assert!(v.recording);
        assert!(!v.can_delete, "an open session cannot be deleted");
        assert_eq!(overs(&v).len(), 2);
    }

    #[test]
    fn my_calls_fall_back_and_hide_without_callsigns() {
        let s = setup("mycalls");
        let o = ViewOpts {
            fallback_my_calls: vec!["kx0bbb-1".into(), "KX0BBB".into()],
            ..opts()
        };
        let tue = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        let v = session_view(&s.a, &tue, Access::Anonymous, &o);
        assert_eq!(v.my_calls, ["N0CALL"]);
        assert!(v.my_side && v.carries_callsigns);
        let thu = s.a.find(THURSDAY, &s.f.private_session).unwrap();
        let v = session_view(&s.a, &thu, Access::Admin, &o);
        assert_eq!(v.my_calls, ["KX0BBB"]);
        assert!(!v.my_side);
        // A network that names nobody has no "my side".
        let mut m = read_meta(&tue.dir).unwrap();
        m.network = "allstar".into();
        write_meta(&tue.dir, &m).unwrap();
        let tue = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        let v = session_view(&s.a, &tue, Access::Admin, &o);
        assert_eq!(v.my_calls, Vec::<String>::new());
        assert!(!v.my_side && !v.carries_callsigns);
        assert_eq!(v.network_label, "AllStar");
        m.network = "ysf".into();
        write_meta(&tue.dir, &m).unwrap();
        let tue = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        assert_eq!(
            session_view(&s.a, &tue, Access::Admin, &o).network_label,
            "ysf"
        );
    }

    #[test]
    fn hostile_talker_survives_as_plain_text() {
        let s = setup("hostile");
        let tue = s.a.find(&s.f.public_net, &s.f.public_session).unwrap();
        let raw = format!("\"<img src=x onerror=alert(1)>\u{202e}{}", "A".repeat(40));
        append_row(
            &tue.dir,
            &LogRow {
                t: "2026-11-04T01:56:29Z".into(),
                kind: LogKind::Over {
                    start: "2026-11-04T01:56:28Z".into(),
                    end: "2026-11-04T01:56:29Z".into(),
                    duration_ms: 1_000,
                    offset_ms: 88_000,
                    talker: Some(raw.clone()),
                    file: "011 18-56-28 unknown.flac".into(),
                },
            },
        )
        .unwrap();
        let v = session_view(&s.a, &tue, Access::Anonymous, &opts());
        let json = serde_json::to_string(&v).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        let hostile = back["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["seq"] == 11)
            .unwrap();
        let want = split_call(&raw).unwrap();
        assert_eq!(hostile["call"], serde_json::Value::String(want.base));
        match want.suffix {
            Some(sfx) => assert_eq!(hostile["suffix"], serde_json::Value::String(sfx)),
            None => assert!(hostile["suffix"].is_null()),
        }
        assert_eq!(hostile["type"], "over");
        assert_eq!(hostile["peaks"], serde_json::json!([]));
    }

    #[test]
    fn damaged_and_open_sessions_do_not_break_the_list() {
        let s = setup("damaged");
        let thu = s.a.find(THURSDAY, &s.f.private_session).unwrap();
        // A torn last line in the log.
        let mut log = std::fs::read(thu.dir.join(LOG_FILE)).unwrap();
        log.extend_from_slice(b"{\"t\":\"2026-11-06T02:00:39Z\",\"kind\":\"ov");
        std::fs::write(thu.dir.join(LOG_FILE), log).unwrap();
        // An over file deleted.
        std::fs::remove_file(thu.dir.join("overs").join("003 19-00-12 KX0BBB.flac")).unwrap();
        // A session.json that does not parse.
        let bad = s.root.join(THURSDAY).join("2026-11-06 1900");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(META_FILE), "{ not json").unwrap();
        // A session folder that is a symlink to one outside the archive.
        let outside =
            std::env::temp_dir().join(format!("netarchive-view-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        let elsewhere = write_fixture(&outside);
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.join("tuesday-m17-net").join("2026-11-03 1855"),
            s.root.join(THURSDAY).join("2026-11-09 1900"),
        )
        .unwrap();

        let entries = s.a.scan();
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert!(s.a.find(THURSDAY, "2026-11-06-1900").is_none());
        assert!(s.a.find(THURSDAY, "2026-11-09-1900").is_none());
        let list = list_view(&s.a, Access::Admin, &opts());
        let total: usize = list.nets.iter().map(|n| n.sessions.len()).sum();
        assert_eq!(total, 3);

        let v = session_view(&s.a, &thu, Access::Admin, &opts());
        let o = overs(&v);
        assert_eq!(o.len(), 5);
        let Item::Over { peaks, .. } = o[2] else {
            unreachable!()
        };
        assert_eq!(peaks, &Vec::<u8>::new());
        let Item::Over { peaks, .. } = o[0] else {
            unreachable!()
        };
        assert_eq!(peaks.len(), 48);
        let _ = elsewhere;
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn encode_leaves_only_unreserved_bytes() {
        assert_eq!(
            encode("001 19-00-12 N0CALL.flac"),
            "001%2019-00-12%20N0CALL.flac"
        );
        assert_eq!(encode("a/b?c#d%"), "a%2Fb%3Fc%23d%25");
        assert_eq!(encode("é"), "%C3%A9");
    }
}
