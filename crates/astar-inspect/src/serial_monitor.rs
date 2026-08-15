// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Read-only serial RX monitor for the harness (iax-b38e). Opens a serial port
//! in a background thread, reads incoming **data bytes** (not the CTS modem
//! line the PTT bridge watches), and buffers them as timestamped records with a
//! monotonic `seq`. The `/serial/monitor?since=N` endpoint polls the buffer,
//! mirroring the `/frames?since=N` shape.
//!
//! Port selection: `HARNESS_MONITOR_SERIAL` if set, else the harness's existing
//! PTT port, else the first USB serial port found — the goal is to report
//! whatever data appears on the port that's already there, not to require a
//! separate dedicated one. `serialport` opens are not exclusive on macOS, so the
//! monitor and the PTT bridge can both watch the same device (the bridge reads
//! the CTS modem line; the monitor reads data bytes). v1 is read-only (no TX)
//! and best-effort: if the port can't be opened the monitor is disabled
//! (logged).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// One buffered chunk of received serial data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialRecord {
    /// Monotonic, 1-based sequence number (the `since=N` cursor returns records
    /// with `seq > N`, matching `/frames`).
    pub seq: u64,
    /// Milliseconds since the monitor started (monotonic clock).
    pub at_ms: u64,
    /// Lowercase, space-separated hex of the bytes, e.g. `"48 69 0a"`.
    pub hex: String,
    /// Printable-ASCII rendering; non-printable bytes shown as `.`.
    pub ascii: String,
}

/// Format a byte slice as lowercase, space-separated hex (`"48 69"`).
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Render a byte slice as printable ASCII; bytes outside `0x20..=0x7e` become
/// `.` (classic hexdump convention).
#[must_use]
pub fn to_ascii(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Thread-safe ring of recent serial records with a monotonic `seq` cursor.
/// Pure (no I/O): the reader thread calls [`MonitorBuffer::push`]; the HTTP
/// handler calls [`MonitorBuffer::since`]. Capacity-bounded so a chatty port
/// can't grow memory without bound — old records drop, but `seq` keeps counting.
pub struct MonitorBuffer {
    inner: Mutex<Inner>,
    cap: usize,
}

struct Inner {
    records: std::collections::VecDeque<SerialRecord>,
    next_seq: u64,
}

impl MonitorBuffer {
    /// New buffer holding up to `cap` most-recent records.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                records: std::collections::VecDeque::new(),
                next_seq: 1,
            }),
            cap: cap.max(1),
        }
    }

    /// Append a chunk of received bytes, stamped at `at_ms`. No-op for an empty
    /// slice. Returns the assigned `seq`.
    pub fn push(&self, bytes: &[u8], at_ms: u64) -> u64 {
        let mut g = self.lock();
        let seq = g.next_seq;
        g.next_seq += 1;
        g.records.push_back(SerialRecord {
            seq,
            at_ms,
            hex: to_hex(bytes),
            ascii: to_ascii(bytes),
        });
        while g.records.len() > self.cap {
            g.records.pop_front();
        }
        seq
    }

    /// Records with `seq > since`, oldest first (poll cursor, like `/frames`).
    #[must_use]
    pub fn since(&self, since: u64) -> Vec<SerialRecord> {
        self.lock()
            .records
            .iter()
            .filter(|r| r.seq > since)
            .cloned()
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared monitor state held in `ServerState`: the buffer plus the port
/// description (name + baud) shown in the UI. Present even when no reader runs.
pub struct SerialMonitor {
    /// Timestamped RX records.
    pub buffer: MonitorBuffer,
    /// Human-readable port label (`None` until a reader opens one).
    pub port: Mutex<Option<String>>,
    /// Active baud rate (for display); set when a reader opens the port.
    pub baud: AtomicU32,
    /// Stop flag for the reader thread.
    pub stop: Arc<AtomicBool>,
    /// Wall-clock start, used to compute `at_ms`.
    start: std::time::Instant,
}

impl SerialMonitor {
    /// New monitor with the given record capacity and initial display baud.
    #[must_use]
    pub fn new(cap: usize, baud: u32) -> Self {
        Self {
            buffer: MonitorBuffer::new(cap),
            port: Mutex::new(None),
            baud: AtomicU32::new(baud),
            stop: Arc::new(AtomicBool::new(false)),
            start: std::time::Instant::now(),
        }
    }

    /// Active baud rate for display.
    #[must_use]
    pub fn baud(&self) -> u32 {
        self.baud.load(Ordering::Relaxed)
    }

    /// Milliseconds since the monitor was constructed.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn set_port(&self, label: Option<String>) {
        *self
            .port
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = label;
    }

    /// Current port label for display.
    #[must_use]
    pub fn port_label(&self) -> Option<String> {
        self.port
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A source of incoming serial bytes. Behind a trait so the buffering loop is
/// testable without real hardware. `read` fills `buf` and returns the count;
/// `0` means "no data this poll" (a timeout), not EOF.
pub trait ByteSource: Send {
    /// Read available bytes into `buf`. Returns the number read (may be 0 on a
    /// read timeout). An `Err` ends the loop.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
}

/// Drive `source` into `monitor.buffer` until `stop` is set or a read errors.
/// Pure of any port-opening concern — tests pass a scripted [`ByteSource`].
pub fn run_loop(monitor: &SerialMonitor, source: &mut dyn ByteSource, stop: &AtomicBool) {
    let mut buf = [0u8; 512];
    while !stop.load(Ordering::Relaxed) {
        match source.read(&mut buf) {
            Ok(0) => {} // timeout: re-check stop and poll again
            Ok(n) => {
                let at = monitor.now_ms();
                monitor.buffer.push(&buf[..n], at);
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
}

/// Adapter: a `serialport` port as a [`ByteSource`]. A read timeout surfaces as
/// `ErrorKind::TimedOut`, which `run_loop` treats as "no data".
struct PortSource(Box<dyn serialport::SerialPort>);
impl ByteSource for PortSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

/// Env-derived monitor configuration.
pub struct MonitorConfig {
    /// Explicit monitor port (`HARNESS_MONITOR_SERIAL`); `None` → autodetect a
    /// port that is NOT the PTT port.
    pub port: Option<String>,
    /// Baud rate (`HARNESS_MONITOR_BAUD`, default 9600 — the UCI150 rate).
    pub baud: u32,
    /// The PTT port path (if any). Used as the default monitor port when no
    /// explicit one is set — the goal is to report whatever data appears on the
    /// existing serial port, not to require a separate dedicated one.
    pub ptt_port: Option<String>,
}

impl MonitorConfig {
    /// Read from an env-like getter (testable).
    pub fn parse(get: impl Fn(&str) -> Option<String>, ptt_port: Option<String>) -> Self {
        let baud = get("HARNESS_MONITOR_BAUD")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(9600);
        Self {
            port: get("HARNESS_MONITOR_SERIAL")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            baud,
            ptt_port,
        }
    }

    /// Resolve the port to open: explicit `HARNESS_MONITOR_SERIAL` wins, else
    /// the existing PTT port, else the first USB-ish serial port we can see.
    /// Reports whatever data appears on the existing port rather than requiring
    /// a separate one.
    #[must_use]
    pub fn resolve_port(&self) -> Option<String> {
        if let Some(p) = &self.port {
            return Some(p.clone());
        }
        if let Some(p) = &self.ptt_port {
            return Some(p.clone());
        }
        serialport::available_ports()
            .ok()?
            .into_iter()
            .find_map(|p| {
                let name = p.port_name;
                let usbish = name.contains("usb") || name.contains("wch") || name.contains("UART");
                (name.contains("cu.") && usbish).then_some(name)
            })
    }
}

/// Spawn the monitor reader if a port is available. `None` (logged) when there
/// is no port or it can't be opened — the harness runs fine without it. The
/// thread stops when `monitor.stop` is set.
pub fn spawn(monitor: &Arc<SerialMonitor>, cfg: &MonitorConfig) -> Option<JoinHandle<()>> {
    let Some(path) = cfg.resolve_port() else {
        tracing::info!(target: "astar_inspect::serial_monitor", "no serial monitor port; monitor disabled");
        return None;
    };
    let baud = cfg.baud;
    let port = match serialport::new(&path, baud)
        .timeout(Duration::from_millis(200))
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "astar_inspect::serial_monitor", "cannot open monitor {path}: {e}; monitor disabled");
            return None;
        }
    };
    tracing::info!(target: "astar_inspect::serial_monitor", "serial monitor on {path} @ {baud}");
    monitor.baud.store(baud, Ordering::Relaxed);
    monitor.set_port(Some(path));
    let mon = Arc::clone(monitor);
    let stop = Arc::clone(&monitor.stop);
    let mut src = PortSource(port);
    Some(std::thread::spawn(move || {
        run_loop(&mon, &mut src, &stop);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn hex_and_ascii_formatting() {
        assert_eq!(to_hex(&[0x48, 0x69, 0x0a]), "48 69 0a");
        assert_eq!(to_hex(&[]), "");
        // 0x48='H' 0x69='i' 0x0a=newline (non-printable) 0x7e='~' 0x7f=del
        assert_eq!(to_ascii(&[0x48, 0x69, 0x0a, 0x7e, 0x7f]), "Hi.~.");
    }

    #[test]
    fn buffer_assigns_monotonic_seq_and_filters_by_since() {
        let b = MonitorBuffer::new(64);
        assert!(b.since(0).is_empty());
        let s1 = b.push(b"AB", 10);
        let s2 = b.push(b"CD", 20);
        assert_eq!((s1, s2), (1, 2));

        let all = b.since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[0].hex, "41 42");
        assert_eq!(all[0].ascii, "AB");
        assert_eq!(all[0].at_ms, 10);

        // since=1 → only records with seq > 1
        let tail = b.since(1);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 2);
        assert_eq!(tail[0].ascii, "CD");

        // since at/after the head → empty
        assert!(b.since(2).is_empty());
    }

    #[test]
    fn empty_push_still_records_a_seq() {
        // A 0-byte read shouldn't reach push (run_loop guards Ok(0)), but the
        // buffer itself records whatever it's given — keep it simple/total.
        let b = MonitorBuffer::new(8);
        let s = b.push(b"", 5);
        assert_eq!(s, 1);
        assert_eq!(b.since(0)[0].hex, "");
    }

    #[test]
    fn buffer_caps_records_but_seq_keeps_climbing() {
        let b = MonitorBuffer::new(2);
        b.push(b"A", 1);
        b.push(b"B", 2);
        b.push(b"C", 3); // evicts seq 1
        let recs = b.since(0);
        assert_eq!(recs.len(), 2, "only cap records retained");
        assert_eq!(recs[0].seq, 2, "oldest retained is seq 2");
        assert_eq!(recs[1].seq, 3);
        // A poller that saw seq 2 still gets seq 3 only.
        assert_eq!(b.since(2).len(), 1);
    }

    /// Scripted source: yields each scripted chunk once, then stops the loop.
    struct ScriptSource {
        chunks: std::collections::VecDeque<Vec<u8>>,
        stop: Arc<AtomicBool>,
    }
    impl ByteSource for ScriptSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(c) = self.chunks.pop_front() {
                let n = c.len().min(buf.len());
                buf[..n].copy_from_slice(&c[..n]);
                Ok(n)
            } else {
                // nothing left: end the loop
                self.stop.store(true, Ordering::Relaxed);
                Ok(0)
            }
        }
    }

    #[test]
    fn run_loop_buffers_each_chunk() {
        let monitor = SerialMonitor::new(64, 9600);
        let stop = Arc::new(AtomicBool::new(false));
        let mut src = ScriptSource {
            chunks: [b"He".to_vec(), b"llo".to_vec()].into_iter().collect(),
            stop: Arc::clone(&stop),
        };
        run_loop(&monitor, &mut src, &stop);
        let recs = monitor.buffer.since(0);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].ascii, "He");
        assert_eq!(recs[1].ascii, "llo");
    }

    #[test]
    fn run_loop_stops_on_read_error() {
        struct ErrSource;
        impl ByteSource for ErrSource {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("device gone"))
            }
        }
        let monitor = SerialMonitor::new(8, 9600);
        let stop = Arc::new(AtomicBool::new(false));
        let mut src = ErrSource;
        run_loop(&monitor, &mut src, &stop); // must return, not spin
        assert!(monitor.buffer.since(0).is_empty());
    }

    #[test]
    fn config_parse_defaults_and_overrides() {
        let empty = MonitorConfig::parse(|_| None, None);
        assert!(empty.port.is_none());
        assert_eq!(empty.baud, 9600);

        let mut m = HashMap::new();
        m.insert("HARNESS_MONITOR_SERIAL", "/dev/cu.mon");
        m.insert("HARNESS_MONITOR_BAUD", "115200");
        let cfg = MonitorConfig::parse(|k| m.get(k).map(ToString::to_string), None);
        assert_eq!(cfg.port.as_deref(), Some("/dev/cu.mon"));
        assert_eq!(cfg.baud, 115_200);
    }

    #[test]
    fn resolve_prefers_explicit_port() {
        let cfg = MonitorConfig {
            port: Some("/dev/cu.explicit".into()),
            baud: 9600,
            ptt_port: Some("/dev/cu.explicit".into()),
        };
        // Explicit config wins even if it equals the PTT port (operator chose it).
        assert_eq!(cfg.resolve_port().as_deref(), Some("/dev/cu.explicit"));
    }

    #[test]
    fn resolve_falls_back_to_the_ptt_port() {
        let cfg = MonitorConfig {
            port: None,
            baud: 9600,
            ptt_port: Some("/dev/cu.ptt".into()),
        };
        // No explicit monitor port → watch the existing PTT port (report what's
        // there rather than requiring a separate dedicated port).
        assert_eq!(cfg.resolve_port().as_deref(), Some("/dev/cu.ptt"));
    }
}
