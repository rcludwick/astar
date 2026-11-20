// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The decoder thread. Audio arrives on a bounded tap (the feeding side only
//! ever `try_send`s and counts drops) and a worker thread decodes it, because
//! one `Decoder::push` can take seconds. Each picture is reported to a sink
//! on the worker thread, and the picture in flight can be peeked at any time.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_record::tap::PcmChunk;

use crate::{Decoder, Event, Mode, Picture, SILENCE_SECS};

/// Capacity of the tap that feeds a [`Worker`]: about five seconds of 20 ms
/// chunks.
pub const SSTV_TAP_CAPACITY: usize = 256;

const POLL: Duration = Duration::from_millis(20);

/// What became of a picture. `seq` numbers pictures from 1 for the worker's
/// life; `rate` is the sample rate of the audio it came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Started {
        seq: u64,
        mode: Mode,
        width: u32,
        height: u32,
        at_sample: u64,
        rate: u32,
        /// How long the picture takes to air, so a view can show progress
        /// (slowrx gives the picture itself only at the end).
        expected_secs: f64,
    },
    Finished {
        seq: u64,
        picture: Picture,
        rate: u32,
    },
    Abandoned {
        seq: u64,
        picture: Picture,
        rate: u32,
    },
    Rejected {
        seq: u64,
    },
}

/// The picture in flight. Rows not yet heard are black.
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    pub seq: u64,
    pub mode: Mode,
    pub width: u32,
    pub height: u32,
    /// Display rows whose audio has arrived.
    pub lines: u32,
    /// `width * height * 3` bytes of RGB.
    pub rgb: Vec<u8>,
}

type Shared = Arc<Mutex<Option<Live>>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Option<Live>> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A running decoder thread.
pub struct Worker {
    live: Shared,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Start a thread that decodes what arrives on `rx` and reports each
    /// picture to `sink`, on that thread.
    pub fn spawn(
        rx: Receiver<PcmChunk>,
        sink: Box<dyn FnMut(Outcome) + Send>,
    ) -> io::Result<Worker> {
        let live: Shared = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let mut run = Run {
            rx,
            sink,
            live: Arc::clone(&live),
            stop: Arc::clone(&stop),
            decoder: None,
            seq: 0,
            last_chunk: Instant::now(),
        };
        let thread = thread::Builder::new()
            .name("astar-sstv".into())
            .spawn(move || run.go())?;
        Ok(Worker {
            live,
            stop,
            thread: Some(thread),
        })
    }

    /// A copy of the picture in flight, `None` between pictures.
    #[must_use]
    pub fn live(&self) -> Option<Live> {
        lock(&self.live).clone()
    }

    /// The picture in flight's sequence number and rows heard, without
    /// copying its pixels.
    #[must_use]
    pub fn live_lines(&self) -> Option<(u64, u32)> {
        lock(&self.live).as_ref().map(|l| (l.seq, l.lines))
    }

    /// Stop the thread and wait for it. A picture in flight is abandoned
    /// with the rows heard, and reaches the sink first.
    pub fn stop(mut self) {
        self.join();
    }

    fn join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.join();
    }
}

struct Run {
    rx: Receiver<PcmChunk>,
    sink: Box<dyn FnMut(Outcome) + Send>,
    live: Shared,
    stop: Arc<AtomicBool>,
    decoder: Option<Decoder>,
    seq: u64,
    last_chunk: Instant,
}

impl Run {
    fn go(&mut self) {
        loop {
            let stopping = self.stop.load(Ordering::Acquire);
            let mut gone = false;
            loop {
                match self.rx.try_recv() {
                    Ok(chunk) => self.chunk(&chunk),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
            if stopping || gone {
                self.finish();
                return;
            }
            if self.decoder.as_ref().is_some_and(Decoder::in_flight)
                && self.last_chunk.elapsed() >= Duration::from_secs(u64::from(SILENCE_SECS))
            {
                self.finish();
            }
            thread::sleep(POLL);
        }
    }

    fn chunk(&mut self, chunk: &PcmChunk) {
        self.last_chunk = Instant::now();
        if self
            .decoder
            .as_ref()
            .is_some_and(|d| d.rate() != chunk.rate)
        {
            self.finish();
            self.decoder = None;
        }
        if self.decoder.is_none() {
            match Decoder::new(chunk.rate) {
                Ok(d) => self.decoder = Some(d),
                Err(_) => return,
            }
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let events = decoder.push(&chunk.samples);
        // Read after the push: a picture that starts in this chunk is in flight now.
        let expected = decoder.expected_secs().unwrap_or(0.0);
        let heard = decoder.lines_heard();
        let rate = decoder.rate();
        for event in events {
            self.event(event, rate, expected);
        }
        if let Some(heard) = heard
            && let Some(live) = lock(&self.live).as_mut()
        {
            live.lines = live.lines.max(heard.min(live.height));
        }
    }

    /// End the picture in flight, if any.
    fn finish(&mut self) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let rate = decoder.rate();
        if let Some(event) = decoder.finish() {
            self.event(event, rate, 0.0);
        }
    }

    fn event(&mut self, event: Event, rate: u32, expected_secs: f64) {
        match event {
            Event::Started {
                mode,
                width,
                height,
                at_sample,
            } => {
                self.seq += 1;
                *lock(&self.live) = Some(Live {
                    seq: self.seq,
                    mode,
                    width,
                    height,
                    lines: 0,
                    rgb: vec![0; width as usize * height as usize * 3],
                });
                (self.sink)(Outcome::Started {
                    seq: self.seq,
                    mode,
                    width,
                    height,
                    at_sample,
                    rate,
                    expected_secs,
                });
            }
            Event::Line { row, rgb } => {
                if let Some(live) = lock(&self.live).as_mut() {
                    let stride = live.width as usize * 3;
                    let at = row as usize * stride;
                    if row < live.height && rgb.len() == stride {
                        live.rgb[at..at + stride].copy_from_slice(&rgb);
                        live.lines = live.lines.max(row + 1);
                    }
                }
            }
            Event::Finished(picture) => {
                *lock(&self.live) = None;
                (self.sink)(Outcome::Finished {
                    seq: self.seq,
                    picture,
                    rate,
                });
            }
            Event::Abandoned(picture) => {
                *lock(&self.live) = None;
                (self.sink)(Outcome::Abandoned {
                    seq: self.seq,
                    picture,
                    rate,
                });
            }
            Event::Rejected { .. } => {
                *lock(&self.live) = None;
                (self.sink)(Outcome::Rejected { seq: self.seq });
            }
        }
    }
}
