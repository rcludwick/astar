// The listener page's AudioWorklet: a small jitter buffer between the Opus
// decoder (main thread) and the audio clock. Decoded PCM arrives as
// Float32Array messages at the context's own rate. Playback starts once
// TARGET seconds are buffered, the oldest samples are dropped above MAX,
// and an empty buffer mid-stream counts an underrun and re-buffers.
"use strict";

const TARGET = 0.12;
const MAX = 0.4;

class AstarJitter extends AudioWorkletProcessor {
  constructor() {
    super();
    this.chunks = [];
    this.offset = 0;
    this.buffered = 0;
    this.playing = false;
    this.underruns = 0;
    this.dropped = 0;
    this.port.onmessage = (e) => {
      const d = e.data;
      if (d === "reset") {
        this.chunks = [];
        this.offset = 0;
        this.buffered = 0;
        this.playing = false;
        return;
      }
      this.chunks.push(d);
      this.buffered += d.length;
      const max = Math.round(MAX * sampleRate);
      while (this.buffered - this.offset > max && this.chunks.length > 1) {
        const first = this.chunks.shift();
        this.buffered -= first.length;
        this.dropped += first.length - this.offset;
        this.offset = 0;
      }
    };
  }

  process(_inputs, outputs) {
    const out = outputs[0][0];
    if (!out) return true;
    const available = this.buffered - this.offset;
    if (!this.playing) {
      if (available >= Math.round(TARGET * sampleRate)) this.playing = true;
      else {
        out.fill(0);
        return true;
      }
    }
    let i = 0;
    while (i < out.length && this.chunks.length) {
      const c = this.chunks[0];
      const n = Math.min(out.length - i, c.length - this.offset);
      out.set(c.subarray(this.offset, this.offset + n), i);
      i += n;
      this.offset += n;
      if (this.offset >= c.length) {
        this.chunks.shift();
        this.buffered -= c.length;
        this.offset = 0;
      }
    }
    if (i < out.length) {
      out.fill(0, i);
      this.underruns += 1;
      this.playing = false;
      this.port.postMessage({ underruns: this.underruns, dropped: this.dropped });
    }
    return true;
  }
}

registerProcessor("astar-jitter", AstarJitter);
