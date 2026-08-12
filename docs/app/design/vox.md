# VOX (voice-activated PTT) — design review & root cause

_Status: root cause confirmed. Blocked on an astar-lib engine change (iax-5c30).
astar nugget: `astar-e9ff`._

## Symptom

With the **VOX** toggle on (dialing page), speaking into the mic does **not** key
the call. It never transmits, regardless of how loud you speak.

## How VOX is wired in astar (today)

- UI toggle → `CallSession.setVoxEnabled(_:)` sets `voxEnabled` (persisted).
- `CallSession.poll()` (20 Hz, now always-on): when `voxEnabled`, it feeds the
  snapshot's **`txDB`** to a `VoxGate` (threshold −40 dBFS, 250 ms hangover) and
  calls `station.setPTT(keyed)` on the rising/falling edge.
- `VoxGate` (AstarCore) is a correct, unit-tested threshold+hangover gate.

So astar's side is sound **if** `txDB` reflects the live mic level while unkeyed.
It does not.

## Root cause (engine, not astar)

`txDB` only reflects the mic level **while the call is keyed**. When unkeyed it
sits at the floor (−60 dBFS), so the `VoxGate` threshold is never crossed —
a chicken-and-egg: VOX needs the *unkeyed* mic level to decide when to key, but
the engine only meters the mic while *already* transmitting.

Traced through astar-lib:

- `astar-station/src/node.rs:315` — while a call is active, the snapshot sets
  `tx_level_db = manager.tx_dbfs(id)`. (Good: refreshed whenever active, not gated
  on PTT at this layer.)
- `manager.rs:847 tx_dbfs` → `router.rs:505 mic_tx_dbfs` → reads `tx_peak`
  ("post-DSP peak" on the mic lane).
- **`router.rs:721 MicLane::write()`** is the mic capture/DSP hot path. At
  **`router.rs:733`**:

  ```rust
  if !self.gate.load(Ordering::Relaxed) {
      self.buf_ulaw.clear(); // drop partial tail on unkey
      return;                // <-- EARLY RETURN when unkeyed
  }
  ...
  self.tx_peak.store(peak(&self.buf_f32)..., Ordering::Relaxed); // line 761 — only reached when keyed
  ```

  The audio backend calls `write()` continuously with live mic samples, but when
  the PTT gate is closed it returns **before** computing `tx_peak`. So `tx_peak`
  (hence `tx_dbfs`/`txDB`) is stale/floored whenever we're not transmitting.

The `MicLane` doc even calls the unkeyed state "monitor-only" (`router.rs:643`),
but metering isn't actually performed in that state — only sending is gated.

## Fix

### Engine (astar-lib) — required → **iax-5c30**

Meter the mic input **continuously (monitor-only)** when unkeyed: compute a peak
of the incoming samples (raw or gain-applied is fine for a level) independent of
the unkeyed early-return, and expose it to consumers.

Preferred shape: a **dedicated input/monitor level** (`mic_input_dbfs` →
`input_db` on the snapshot → C-ABI + Swift `AstarStation` accessor), so `tx_db`
keeps meaning "what we transmit" and `input_db` means "what the mic hears".
(Alternative: make `tx_dbfs` continuous — simpler for consumers, but changes
`tx_db` semantics. Trade-off noted in the ticket.)

A fuller option is a **native engine VOX mode** (the engine auto-gates from the
input level). That's a nice follow-up, but the minimal unblock is just exposing
the unkeyed input level.

### astar — follow-up (after the engine exposes the level) → **astar-e9ff**

1. Add the new `input_db` (monitor level) to `CallSnapshot` + the `Station`
   adapter.
2. Point `VoxGate` at the **monitor level**, not `txDB` (which is post-gate).
3. Re-tune the VOX threshold for real mic-input levels (the −40 dBFS default may
   want to drop given the 0.90 input-gain default; consider exposing a VOX
   sensitivity slider — see open questions).
4. Verify the keyed↔unkeyed transition is clean (no feedback loop once VOX keys
   and the mic is then also being transmitted/metered post-DSP).

## Open questions (for review)

- **Threshold/UX**: fixed −40 dBFS, or a user "VOX sensitivity" slider? A live
  input meter on the dialing page would make setting it obvious — and dovetails
  with the planned mic-characterization Advanced tab (`astar-ee09`).
- **Anti-VOX / hang tuning**: 250 ms hangover good enough, or expose it?
- **Half-duplex etiquette**: should VOX refuse to key while `remotePTT`/rx audio
  is active (don't double-key over someone)? Probably yes — worth deciding.
- **Native engine VOX vs astar-side**: do we want the engine to own VOX
  eventually (consistent across all consumers), or keep it in astar?
