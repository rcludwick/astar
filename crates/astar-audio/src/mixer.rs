// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Summing output mixer: one bus, N call RX channels (iax-42e9 phase 2).
//!
//! Each call routed to a bus contributes a `Receiver<Vec<i16>>` of PCM RX
//! frames + its own jitter cushion (`residual`, mirroring `SpeakerSource`).
//! `read` normalizes i16 → f32, sums sample-aligned across calls, and hard
//! clamps to [-1, 1] so two loud calls can't wrap. A starved call contributes
//! silence and never blocks the others. Codec transcode happens at the network
//! edge (iax-31f7), not here.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;

/// Opaque per-call slot id within a [`Mixer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixCallId(u64);

struct Lane {
    id: MixCallId,
    inbound: Receiver<Vec<i16>>,
    residual: VecDeque<f32>,
    /// Set once for a finite announcement lane; flipped + removed when the
    /// inbound channel is closed and the residual is drained (iax-e30d).
    done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Audio-thread record that the inbound sender has hung up.
    disconnected: bool,
}

/// Sums several calls' decoded RX audio onto one output bus.
#[derive(Default)]
pub struct Mixer {
    lanes: Vec<Lane>,
    next_id: u64,
}

impl Mixer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a call's RX channel; returns its slot id for later removal.
    pub fn add_call(&mut self, inbound: Receiver<Vec<i16>>) -> MixCallId {
        let id = MixCallId(self.next_id);
        self.next_id += 1;
        self.lanes.push(Lane {
            id,
            inbound,
            residual: VecDeque::new(),
            done: None,
            disconnected: false,
        });
        id
    }

    /// Register a finite announcement source: a closed-ended `Receiver` whose
    /// `done` flag is flipped when its audio is fully drained. The lane removes
    /// itself from the bus at that point (iax-e30d).
    pub fn add_finite_call(
        &mut self,
        inbound: Receiver<Vec<i16>>,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> MixCallId {
        let id = MixCallId(self.next_id);
        self.next_id += 1;
        self.lanes.push(Lane {
            id,
            inbound,
            residual: VecDeque::new(),
            done: Some(done),
            disconnected: false,
        });
        id
    }

    /// Remove a call from the bus (monitor-off / hangup).
    pub fn remove_call(&mut self, id: MixCallId) {
        self.lanes.retain(|l| l.id != id);
    }

    /// Detach a call from the bus and return its RX `Receiver` so it can be
    /// re-registered on another bus's mixer (the `set_output` / re-route path).
    /// The per-call jitter `residual` is dropped (Q3: accept a ≤20 ms glitch on
    /// a bus change). Returns `None` if the id isn't on this bus.
    pub fn take_call(&mut self, id: MixCallId) -> Option<Receiver<Vec<i16>>> {
        let pos = self.lanes.iter().position(|l| l.id == id)?;
        Some(self.lanes.remove(pos).inbound)
    }

    #[must_use]
    pub fn call_count(&self) -> usize {
        self.lanes.len()
    }

    /// Fill `out` with the clamped sum of every lane. Returns the number of
    /// samples written (0 when there are no lanes).
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        if self.lanes.is_empty() {
            return 0;
        }
        for slot in out.iter_mut() {
            *slot = 0.0;
        }
        let mut produced = 0usize;
        for lane in &mut self.lanes {
            loop {
                match lane.inbound.try_recv() {
                    Ok(frame) => {
                        for s in frame {
                            lane.residual.push_back(f32::from(s) / 32768.0);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        lane.disconnected = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
            let n = out.len().min(lane.residual.len());
            for slot in out.iter_mut().take(n) {
                *slot += lane.residual.pop_front().unwrap_or(0.0);
            }
            produced = produced.max(n);
        }
        for slot in out.iter_mut() {
            *slot = slot.clamp(-1.0, 1.0);
        }
        // iax-e30d: a finite lane whose sender closed and whose residual is
        // empty is finished — flip its done flag and drop it from the bus.
        self.lanes.retain(|l| {
            let finished = l.disconnected && l.residual.is_empty();
            if finished && let Some(d) = &l.done {
                d.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            !(finished && l.done.is_some())
        });
        // Report the largest lane fill so the cpal output path treats the
        // unfilled tail as silence, not as a starve-stall (mirrors
        // SpeakerSource which returns min(out, residual)).
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn frame(level: i16, n: usize) -> Vec<i16> {
        vec![level; n]
    }

    #[test]
    fn two_buses_sum_sample_aligned() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let (b_tx, b_rx) = channel();
        let a = mixer.add_call(a_rx);
        let b = mixer.add_call(b_rx);
        // Each call sends a constant-level frame; the sum is louder than either.
        a_tx.send(frame(8000, 160)).unwrap();
        b_tx.send(frame(8000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(n, 160);
        let one = f32::from(8000i16) / 32768.0;
        assert!((out[0] - 2.0 * one).abs() < 1e-3, "two equal calls sum");
        let _ = (a, b);
    }

    #[test]
    fn overlap_is_hard_clamped_to_unit() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let (b_tx, b_rx) = channel();
        mixer.add_call(a_rx);
        mixer.add_call(b_rx);
        // Two near-full-scale calls would sum past 1.0 — must clamp.
        a_tx.send(frame(30000, 160)).unwrap();
        b_tx.send(frame(30000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        mixer.read(&mut out);
        assert!(
            out.iter().all(|&s| (-1.0..=1.0).contains(&s)),
            "clamped to [-1,1]"
        );
    }

    #[test]
    fn one_starved_call_does_not_block_the_other() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let (_b_tx, b_rx) = channel(); // B never sends — starved
        mixer.add_call(a_rx);
        mixer.add_call(b_rx);
        a_tx.send(frame(8000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(
            n, 160,
            "A fills the buffer; B contributes silence, not a stall"
        );
        assert!(out[0].abs() > 0.0, "A's audio is present");
    }

    #[test]
    fn finite_lane_flips_done_and_self_removes_when_drained() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut mixer = Mixer::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        mixer.add_finite_call(rx, Arc::clone(&done));
        tx.send(frame(8000, 160)).unwrap();
        drop(tx); // sender closed → lane is finite and will end
        let mut out = [0.0_f32; 160];
        assert_eq!(mixer.read(&mut out), 160, "plays its one frame");
        let mut out2 = [0.0_f32; 160];
        mixer.read(&mut out2); // disconnected + drained → done + removed
        assert!(done.load(Ordering::Relaxed), "done flips when drained");
        assert_eq!(mixer.call_count(), 0, "finite lane self-removes");
    }

    #[test]
    fn dropping_a_call_removes_it_from_the_sum() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let id = mixer.add_call(a_rx);
        mixer.remove_call(id);
        // The lane (and its Receiver) is gone, so this send has nowhere to land
        // — the failed send is itself evidence the call was removed.
        let _ = a_tx.send(frame(8000, 160));
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(n, 0, "no calls → nothing to mix");
    }
}
