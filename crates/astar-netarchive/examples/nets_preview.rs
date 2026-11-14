// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Serve the recordings pages over the synthetic fixture, for screenshots.
//!
//! `nets_preview <dir> <port>` writes the fixture into `<dir>` and serves
//! [`Pages`] on `127.0.0.1:<port>` (port 0 picks a free one) until killed.
//! `NETS_PREVIEW_ACCESS` is `anonymous`, `viewer` or `admin` (the default).
//!
//! It prints, one per line, `url <base>`, `session <path>` (the public
//! session) and `now_seq <n>` (the `seq` of the over playing at 40 s,
//! taken from the session view: the one whose offset range contains 40 s).

use astar_http::{Limits, Server};
use astar_netarchive::{
    Access, Item, NetsRequest, Pages, ViewOpts, fixture::write_fixture, session_view, slug,
};
use std::io::Write;
use std::path::PathBuf;

const NOW_MS: u64 = 40_000;

fn opts_for_view() -> ViewOpts {
    ViewOpts {
        delete_url: Some("/nets/delete".into()),
        sign_in_url: Some("/signin".into()),
        fallback_my_calls: Vec::new(),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: nets_preview <dir> <port>"));
    let port: u16 = args
        .next()
        .expect("usage: nets_preview <dir> <port>")
        .parse()
        .expect("a port number");
    let access = match std::env::var("NETS_PREVIEW_ACCESS").as_deref() {
        Ok("anonymous") => Access::Anonymous,
        Ok("viewer") => Access::Viewer,
        _ => Access::Admin,
    };
    let fixture = write_fixture(&dir);
    let pages = std::sync::Arc::new(Pages::new(dir, opts_for_view()));
    let entry = pages
        .archive()
        .find(&fixture.public_net, &fixture.public_session)
        .expect("the public session");
    // The page's `data-seq` and `#t=` are the view's own `Item::Over` seq and
    // offsets, so ask the view rather than counting log rows.
    let view = session_view(pages.archive(), &entry, Access::Admin, &opts_for_view());
    let now_seq = view
        .items
        .iter()
        .find_map(|i| match i {
            Item::Over {
                seq,
                offset_ms,
                duration_ms,
                ..
            } if (*offset_ms..*offset_ms + *duration_ms).contains(&NOW_MS) => Some(*seq),
            _ => None,
        })
        .expect("an over playing at 40 s in the fixture");

    let handler_pages = std::sync::Arc::clone(&pages);
    let server = Server::bind(
        &format!("127.0.0.1:{port}"),
        Limits::loopback(),
        move |req| {
            let range = req.header("range").map(str::to_owned);
            handler_pages
                .handle(&NetsRequest {
                    method: req.method(),
                    path: req.path(),
                    access,
                    offer_sign_in: true,
                })
                .into_reply(range)
        },
    )
    .expect("bind 127.0.0.1");
    let addr = server.local_addr();
    println!("url http://{addr}/nets/");
    println!(
        "session /nets/{}/{}/",
        slug(&fixture.public_net),
        slug(&fixture.public_session)
    );
    println!("now_seq {now_seq}");
    std::io::stdout().flush().expect("flush");
    loop {
        std::thread::park();
    }
}
