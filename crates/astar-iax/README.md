# astar-iax

High-level IAX2 (RFC 5456) client: a `Manager` owns a pool of `Call` handles
and the audio router, with audio via a pluggable `AudioBackend` (cpal
included). This is the lower-level escape hatch; most apps want
`astar-console`.

```rust,no_run
use astar_iax::{CallId, CallMode, DialSpec, Manager};
use astar_audio::OutputId;

let mut mgr = Manager::new(Box::new(astar_audio::CpalBackend::new()));
let out = OutputId::new(mgr.default_output().unwrap().id.as_str());
let id = mgr.dial(DialSpec {
    id: CallId(0),
    node: "55553".into(),
    peer: "104.232.32.242:4569".parse().unwrap(),
    output: out,
    caller_id: "allstar-public".into(),
    secret: "allstar".into(),
    mode: CallMode::Standard,
    dest: "55553".into(),
    frame_observer: None,
})?;
mgr.key(id)?; // 160-byte/20ms G.711 frames + RADIO_KEY on the wire
for ev in mgr.take_events(id).into_iter().flatten() {
    /* Answered / RemotePtt / Hangup ... */
}
# Ok::<(), astar_iax::IaxError>(())
```

Voice TX is conformant ASL framing (fixed 20 ms frames, media-clock
timestamps, RADIO_KEY/UNKEY signalling). See `astar-console` for the
session/state layer and `crates/astar-console/examples/asl3_call.rs` for
the end-to-end ASL3 recipe.
