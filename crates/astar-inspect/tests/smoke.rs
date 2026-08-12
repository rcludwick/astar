// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Smoke test: the `tiny_http` server binds, routes a real request, and stops
//! cleanly (iax-dd42 Phase 2). Uses a stub backend (no real audio devices).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_inspect::server::{ServerState, serve};

fn dev(name: &str, dir: Direction) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(name.to_string()),
        name: name.to_string(),
        direction: dir,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

struct StubBackend;
impl AudioBackend for StubBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev("Smoke Mic", Direction::Input),
            dev("Smoke Out", Direction::Output),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev("Smoke Mic", Direction::Input))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev("Smoke Out", Direction::Output))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _s: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        unreachable!()
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _s: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        unreachable!()
    }
}

#[test]
fn server_binds_and_serves_devices() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let state = ServerState::new(Box::new(|| Box::new(StubBackend) as Box<dyn AudioBackend>));
    // au-ef39: the stop flag lives in ServerState now; keep a handle to set it.
    let state_for_stop = Arc::clone(&state);

    // serve() picks the real bound addr; use a channel to learn it.
    let (tx, rx) = std::sync::mpsc::channel();
    let server_thread = thread::spawn(move || {
        serve(addr, &state, Some(tx)).expect("serve");
    });
    let bound: SocketAddr = rx.recv_timeout(Duration::from_secs(2)).expect("bound addr");

    // Raw HTTP GET /devices.
    let mut conn = TcpStream::connect(bound).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    conn.write_all(b"GET /devices HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    conn.read_to_string(&mut resp).unwrap();

    assert!(resp.starts_with("HTTP/1.1 200"), "200 OK: {resp}");
    assert!(resp.contains("Smoke Mic"), "device JSON present: {resp}");

    state_for_stop.stop.store(true, Ordering::Relaxed);
    server_thread.join().expect("server thread joined");
}
