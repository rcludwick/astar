# The RX jitter buffer

Received audio on an AllStarLink call used to stutter on an idle machine, and
disconnecting and reconnecting cured it. The reason was not subtle: **there was
no jitter buffer on the receive path at all.**

`crates/astar-iax/src/runtime.rs` decoded each `VoiceReceived` and pushed the
PCM straight into the mixer lane's residual; `listener.rs` did the same for
inbound legs. The lane's target depth was therefore zero, so any inter-arrival
gap longer than one device callback (~10 ms) was an audible hole, and nothing
at all compensated sender-vs-device clock drift. Nothing counted the holes
either: the mic side had `tx_capture_overruns`, the speaker side had no
underrun counter and no jitter statistics, so "it's stuttering" could not be
turned into a number.

`crates/astar-codec/src/jitter.rs` — a faithful port of Steve Kann's
`jitterbuf.c`, the one Asterisk uses — had been sitting in the tree, complete
with parity tests, used by nothing.

## Why the mixer lane

The buffer lives in `crates/astar-audio/src/mixer.rs`, one per lane, pulled by
the playback clock on every `Mixer::read`.

A jitter buffer has to be **pulled by the consumer's clock**, not pushed by the
producer's. The device callback is the only clock in the system that matters:
it is what decides when a sample is played and it is what runs dry. Putting the
buffer anywhere upstream — in the run loop, on the decode edge — would mean
inventing a second clock to drive it, and then reconciling the two. The mixer
lane is also exactly where the per-call cushion (`residual`) already lived, so
the buffer replaces a mechanism rather than layering on one.

Three consequences fall out of that placement:

* **It is per lane, so it is per call.** Two calls summed onto one bus each ride
  out their own network.
* **Only lanes with a sender clock are buffered.** `RxFrame` carries an optional
  `RxClock { ts_ms, ms }`. Bare PCM — `RxFrame::from(pcm)` — goes straight into
  the residual exactly as it always did: the parrot, announcements, and the
  M17/D-Star/DMR/YSF/NXDN decoders, all of which produce decoded frames on the
  protocol's own tick and have no wire clock to schedule against.
* **The lane paces itself against `JitterBuf::next()`.** `get` hands frames back
  as fast as it is asked, so without the pacing check the first callback of a
  talk spurt would drain the whole cushion it had just built.

## Putting the sender's clock back together

The buffer schedules against the SENDER's timestamp, so the frame has to carry
one. RFC 5456 §6.4 gives a full voice frame the whole 32-bit millisecond
timestamp but a mini frame only its low 16 bits, and the session FSM reports
both as a plain `u32`. `crates/astar-iax/src/rx_clock.rs` anchors on the highest
timestamp seen and restores the high half; without it the clock would appear to
fall off a cliff every 65.536 seconds and the buffer would resynchronise,
audibly. The anchor never walks backwards, so one reordered frame cannot drag
the epoch with it.

The lane then normalises both axes to its own first frame — first timestamp and
first arrival both become zero — so `now - ts` is the transit delay relative to
that frame whatever the two clocks' absolute origins are, and joining a stream
mid-flight costs nothing.

## The Asterisk numbers

`IAX_JITTER_CONFIG` is Asterisk `chan_iax2`'s shipped default, and that is the
point: the node at the other end of an AllStarLink call **is** Asterisk, so
matching its buffer is what makes our playout behave the way operators already
expect.

| | ms | `jitterbuf.c` | meaning |
|---|---|---|---|
| `target_extra` | 40 | slack over measured jitter | the floor the adaptive target sits on |
| `max_jitterbuf` | 200 | ceiling | the depth may not grow past this |
| `resync_threshold` | 1000 | | a delay jump this big, three in a row, rebases the timestamp axis |
| `max_contig_interp` | 10 | ×20 ms | 200 ms of consecutive interpolation ends the talk spurt |

The first two are the knobs: `RxJitterConfig { enabled, min_ms, max_ms }` maps
onto `target_extra` and `max_jitterbuf`. Both bounds are clamped to 0..=500 ms
and a `max_ms` below `min_ms` is raised to meet it — an operator's setting is
repaired, never refused. The resync threshold and the interpolation cap are not
knobs: they are what makes the port behave like the thing it is a port of.

The configuration is live. Switching the buffer **off** mid-call drains what it
is holding into the residual (the audio is received and not yet played, so it
comes out once, in order) and puts the lane on the same direct path bare PCM
takes. Switching it back **on** starts a fresh, empty buffer.

## What a missing frame costs

One interpolation: exactly 20 ms of silence, and no more. There is no
packet-loss concealment. Synthesising a plausible waveform would be a
fabrication about what was received, and on a ham radio link the difference
between "we received this" and "we invented this" is not a detail.

## The counters

| | means |
|---|---|
| `rx_underruns` | Device callbacks that got **no audio at all** while a lane was out of audio mid-talk-spurt. The receive-side counterpart of `tx_capture_overruns`. This is the number that grows while received audio stutters. |
| `rx_jitter_ms` | Measured network jitter: max-min over the delay history, Asterisk's percentile estimate. |
| `rx_jb_depth_ms` | How much audio the buffer is currently holding back. Latency you are paying. |
| `rx_frames_lost` | Frames expected and never seen — each one cost 20 ms of silence. |
| `rx_frames_late` | Frames that arrived after their play time and were thrown away. |
| `rx_frames_ooo` | Frames that arrived out of timestamp order (reordered in place, not lost). |

`rx_underruns` is deliberately narrow. A lane that is **holding frames back to
deepen its cushion** is doing its job, not starving, so it does not count — the
condition is an empty buffer while the spurt is still open. And because the
counter keeps working with the buffer switched off (a lane counts as
mid-spurt for 200 ms after a timestamped frame, the same window the enabled
path uses), turning the buffer off and watching the number is a real A/B test
rather than a change of definition.

A clean end-of-transmission does **not** produce underruns: the buffer covers
the first ten missing slots with interpolations — which is what `rx_frames_lost`
is for — and then declares the spurt over. Underruns mean something went wrong,
which is what a health counter is supposed to mean.
