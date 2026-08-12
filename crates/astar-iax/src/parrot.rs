// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Headless record-then-playback parrot over a raw-dial call (iax-64b6):
//! buffers the far end's audio while it's keyed, waits a beat after unkey, then
//! plays it back. Drives any peer that bridges a call to a handset (the
//! node-as-handset test path). Shared by the `echo` example and the inspect
//! harness's in-process parrot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{CallEvent, RawDial};

/// PCM frames are 20 ms of audio at the station rate; play them back at that rate.
const FRAME_PERIOD: Duration = Duration::from_millis(20);

/// Head-start buffer on playback: the first ~5 frames go out immediately so the
/// receiver's output always has a cushion to absorb arrival jitter (the node's
/// mixer pads with silence when starved, which sounds choppy).
const PLAYOUT_CUSHION: Duration = Duration::from_millis(100);

/// Tunables for [`run_parrot`].
#[derive(Clone, Copy, Debug)]
pub struct ParrotConfig {
    /// Pause between the caller unkeying and the replay ("a few seconds").
    pub playback_delay: Duration,
    /// Frame-gap that counts as end-of-transmission (covers a dropped PTT-off).
    pub silence_gap: Duration,
}

impl Default for ParrotConfig {
    fn default() -> Self {
        Self {
            playback_delay: Duration::from_secs(3),
            silence_gap: Duration::from_millis(800),
        }
    }
}

/// Parrot state machine.
enum State {
    /// Waiting for the far end to key / send audio.
    Idle,
    /// Buffering the far end's audio; `last` is when the most recent frame came.
    Recording { last: Instant },
    /// Recorded; replay once `Instant` is reached.
    Pending(Instant),
}

/// Run the parrot loop until `stop` is set or the call hangs up. Consumes the
/// dial. Logs state transitions with `log_line` (pass a closure; the example
/// prints to stdout, the harness can drop them or log them).
pub fn run_parrot(
    raw: RawDial,
    cfg: &ParrotConfig,
    stop: &AtomicBool,
    mut log_line: impl FnMut(&str),
) {
    let RawDial {
        call,
        events,
        tx_frames,
        rx_frames,
    } = raw;

    let mut state = State::Idle;
    let mut buffer: Vec<Vec<i16>> = Vec::new();

    loop {
        // Stop requested: hang up and return.
        if stop.load(Ordering::Relaxed) {
            let _ = call.hangup(None);
            return;
        }

        // Lifecycle events. RemotePtt(true/false) is the node operator keying /
        // unkeying their handset (manager.key/unkey signals it on the wire).
        while let Ok(ev) = events.try_recv() {
            match ev {
                CallEvent::Answered { .. } => {
                    log_line("connected — go ahead and key the handset");
                }
                CallEvent::RemotePtt(false) => {
                    // Unkeyed: end recording, schedule playback.
                    if let State::Recording { .. } = state {
                        state = finish_recording(&buffer, cfg, &mut log_line);
                    }
                }
                CallEvent::Hangup { reason } => {
                    log_line(&format!("hangup: {reason:?}"));
                    let _ = call.hangup(None);
                    return;
                }
                _ => {}
            }
        }

        // Inbound audio. Buffer it while recording; the first frame after Idle
        // starts a recording (audio only flows while the far end is keyed). We
        // ignore audio that arrives during Pending/playback (the parrot has the
        // floor until it finishes replaying).
        while let Ok(frame) = rx_frames.try_recv() {
            match &mut state {
                State::Idle => {
                    buffer.clear();
                    buffer.push(frame);
                    log_line("recording…");
                    state = State::Recording {
                        last: Instant::now(),
                    };
                }
                State::Recording { last } => {
                    buffer.push(frame);
                    *last = Instant::now();
                }
                State::Pending(_) => { /* parrot is talking; drop incoming */ }
            }
        }

        // Fallback end-of-transmission: a long enough gap means they unkeyed.
        if let State::Recording { last } = &state
            && last.elapsed() >= cfg.silence_gap
        {
            state = finish_recording(&buffer, cfg, &mut log_line);
        }

        // Play back once the delay elapses.
        if let State::Pending(at) = state
            && Instant::now() >= at
        {
            let secs = (buffer.len() as u64 * 20) / 1000;
            log_line(&format!("playing back {} frames (~{secs}s)…", buffer.len()));
            // Signal keyed so the node lights its remote-PTT / RX meter.
            let _ = call.set_ptt(true);
            let mut torn_down = false;
            // Pace against ABSOLUTE deadlines (not a fixed per-frame sleep,
            // which overshoots and drifts the stream slower than 50 fps until
            // the receiver underruns → choppy). A small head-start cushion
            // pre-sends the first few frames so the receiver always has a
            // ~100 ms playout buffer to absorb jitter.
            let playback_start = Instant::now();
            for (i, frame) in buffer.drain(..).enumerate() {
                let nominal = FRAME_PERIOD * u32::try_from(i).unwrap_or(u32::MAX);
                let target = playback_start + nominal.saturating_sub(PLAYOUT_CUSHION);
                let now = Instant::now();
                if target > now {
                    std::thread::sleep(target - now);
                }
                if tx_frames.send(frame).is_err() {
                    torn_down = true;
                    break;
                }
            }
            let _ = call.set_ptt(false);
            if torn_down {
                let _ = call.hangup(None);
                return;
            }
            log_line("done — key the handset to go again.");
            state = State::Idle;
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

/// End a recording: announce it and schedule playback after the delay.
fn finish_recording(
    buffer: &[Vec<i16>],
    cfg: &ParrotConfig,
    log_line: &mut impl FnMut(&str),
) -> State {
    if buffer.is_empty() {
        return State::Idle;
    }
    log_line(&format!(
        "got {} frames; playing back in {}s…",
        buffer.len(),
        cfg.playback_delay.as_secs()
    ));
    State::Pending(Instant::now() + cfg.playback_delay)
}
