# Ported: `imbe-vocoder`

This directory holds a Rust port of Pavel Yazev's fixed-point P25 IMBE
vocoder, the `imbe_vocoder` directory of OP25. IMBE is the full-rate voice
codec of P25 Phase 1: every 20 ms of 8 kHz speech becomes one 88-bit frame
of model parameters (pitch, voiced/unvoiced bands, spectral amplitudes), and
the decoder turns those parameters back into speech. Both halves are ported:
`ImbeEncoder` and `ImbeDecoder`.

| | |
|---|---|
| Upstream repository | <https://github.com/boatbod/op25> |
| Upstream path | `op25/gr-op25_repeater/lib/imbe_vocoder/` |
| Upstream commit | `38a37491df4532d932f9f1d5634617af410cb3c9` (2026-11-14) |
| Ported | 2026-11-21 |
| Licence | `GPL-3.0-or-later` (every upstream file header; see below) |
| Author | Pavel Yazev |

Upstream's file headers read:

```
Project 25 IMBE Encoder/Decoder Fixed-Point implementation
Developed by Pavel Yazev E-mail: pyazev@gmail.com
Version 1.0 (c) Copyright 2009
```

followed by the GNU General Public License notice, "either version 3, or (at
your option) any later version". OP25 does not ship the COPYING file those
headers refer to, so `COPYING` here is the GPL-3.0 text, a byte-for-byte copy
of the `LICENSE-GPL-3.0.txt` that astar's D-Star software vocoder island
already carries.

## A port, not a copy

No upstream source is in this repository. The C++ was read and rewritten as
Rust, module for module and function for function, keeping upstream's
function names in snake case so the two can be read side by side. That makes
this a derivative work, so it stays GPL-3.0-or-later: every `.rs` file keeps
Pavel Yazev's copyright line, adds "Rust port (c) 2026 Rob Ludwick", and
carries `SPDX-License-Identifier: GPL-3.0-or-later`. GPL-3.0 and astar's
AGPL-3.0 may be combined (GPL-3.0 section 13, AGPL-3.0 section 13), which is
why this can sit as a licence island inside the AGPL-3.0-only workspace. The
patent reasoning is in `LICENSE-EXCEPTIONS.md`.

The port is held to the original bit for bit. `tools/make-golden.sh` fetches
OP25 at the commit above into a temporary directory, compiles it with a small
driver of astar's own (`c++ -std=c++11 -O0`), and records what the C++ does:

* `tests/golden/encode.bin`: seven 2-second test signals (two tones, a
  synthetic vowel, noise, silence, a chirp and a full-scale square wave), 700
  frames of 160 input samples each, and the 11 bytes the encoder made of
  each.
* `tests/golden/decode.bin`: those 700 frames, then 50 frames of random bytes,
  10 null frames, 10 all-zero and 10 all-`FF` frames, and the 160 samples the
  decoder made of each.
* `tests/golden/MANIFEST`: the commit, the compiler, the date and the SHA-256
  of both files.

The script is run by hand, once, and never by CI; `just imbe-check` only
reads the committed vectors, so CI needs no C++ compiler and no network. The
decoder half runs in its own process, because upstream's random generator
keeps its seed in a file-static variable shared by every `imbe_vocoder` in a
process (see "Changes from upstream").

## File-to-module map

Each upstream `.cc` becomes the `.rs` of the same stem; the headers' types and
constants live with the code that uses them.

| Upstream | Port | |
|---|---|---|
| `basicop2.cc`, `basic_op.h`, `typedef.h`, `typedefs.h` | `src/basic_op.rs` | rewritten from the operators' definitions, see below |
| `tbls.cc`, `tbls.h` | `src/tables.rs` | values unchanged |
| `aux_sub.cc` | `src/aux_sub.rs` | |
| `math_sub.cc` | `src/math_sub.rs` | |
| `dsp_sub.cc` | `src/dsp_sub.rs` | `fft_init` is `Fft::new` |
| `qnt_sub.cc` | `src/qnt_sub.rs` | |
| `rand_gen.cc` | `src/rand_gen.rs` | |
| `ch_decode.cc` | `src/ch_decode.rs` | |
| `sa_decode.cc` | `src/sa_decode.rs` | |
| `sa_enh.cc` | `src/sa_enh.rs` | |
| `v_synt.cc` | `src/v_synt.rs` | |
| `uv_synt.cc` | `src/uv_synt.rs` | |
| `decode.cc`, `imbe_vocoder.cc`, `imbe.h` | `src/decode.rs`, `src/lib.rs` | `ImbeDecoder`, `ImbeParam` |
| `dc_rmv.cc` | `src/dc_rmv.rs` | |
| `pe_lpf.cc` | `src/pe_lpf.rs` | |
| `pitch_est.cc` | `src/pitch_est.rs` | `PitchEst` holds the tracker's memory |
| `pitch_ref.cc` | `src/pitch_ref.rs` | |
| `v_uv_det.cc` | `src/v_uv_det.rs` | `VUvDet`; upstream's `pitch_ref_init` is `VUvDet::new` |
| `sa_encode.cc` | `src/sa_encode.rs` | `SaEncoder` |
| `ch_encode.cc` | `src/ch_encode.rs` | |
| `encode.cc`, `imbe_vocoder.cc` | `src/encode.rs` | `ImbeEncoder` |
| (none) | `src/frame.rs` | astar's: the 88-bit packing |

The ETSI operators upstream never calls outside `basicop2.cc` (`abs_s`,
`L_negate`, `mac_r`, `msu_r` and the rest) are not ported.

## Changes from upstream

* **The ETSI basic operators are rewritten from their definitions**, as Rust
  `saturating_*` and `wrapping_*` arithmetic on `i16` and `i32`, rather than
  carrying the ETSI/ITU-T GSM-AMR reference text. Upstream's `Overflow` and
  `Carry` globals are gone: nothing outside `basicop2.cc` reads them.
* **`div_s` never aborts.** Upstream calls `abort()` when the dividend is
  negative or larger than the divisor, or the divisor is 0. The port
  saturates instead, in every build: 32767 when the dividend is the larger,
  0 for a negative or zero operand. Frames come off the network, so an abort
  or a panic would let one malformed frame take astar down. No golden vector
  reaches that path.
* **Encoder and decoder state are split into two structs.** Upstream's class
  shares one `IMBE_PARAM` between its encoder and decoder; here
  `ImbeDecoder` owns its parameters, phases, spectral history, overlap memory
  and FFT tables, and `ImbeEncoder` owns its own parameters, analysis
  buffers, filter memories, pitch tracker, voicing memory, amplitude history
  and FFT tables. Upstream's encoder reads one parameter it does not write
  that frame, b(L+2), the last bit of û7 (see `ch_encode.rs`); it is 0 in a
  fresh vocoder either way, and each half carries its own copy forward
  exactly as the shared one would in an encode-only or decode-only
  instance.
* **Each decoder has its own random generator.** Upstream's `rand_gen.cc`
  keeps the seed in a file-static variable, so every vocoder in a process
  draws from one sequence (the class's own `seed` member is never used). A
  new `ImbeDecoder` starts from seed 1, exactly as the first vocoder in a
  fresh process does.
* **The encoder's `d_gain_adjust` is fixed at upstream's default of 0** and
  is not exposed. Upstream subtracts it from each log amplitude only when it
  is non-zero; the port keeps the subtraction, of a constant 0
  (`D_GAIN_ADJUST_Q10_22` in `sa_encode.rs`), so the code still reads like
  upstream's.
* **Debug `printf`s are dropped**, as is the licence banner the constructor
  prints to stderr.
* **The 88-bit packing is added in `src/frame.rs`**: the eight code vectors
  û0..û7 as MMDVMHost and G4KLX's P25 software carry them, û0..û3 at 12 bits,
  û4..û6 at 11 bits, û7 at 7 bits, each most significant bit first.
* **Out-of-range reads are bounded.** Where upstream's bit unpacking would
  read past its bit-stream array if a table ever allocated more bits than a
  frame holds, the port reads 0. Integer arithmetic upstream relies on to
  wrap (phase accumulators) is spelled `wrapping_*`; everything else goes
  through `basic_op` or plain arithmetic that a debug build checks.
* **Undefined C is given one defined meaning.** `qnt_by_step` evaluates
  `1 << (bit_num - 1)` even for a 0-bit field; the port returns index 0,
  which is what that shift gives on arm64 and x86-64. `pitch_est`'s one plain
  `int` multiply wraps (`wrapping_mul`), as the C does in practice.
  `pitch_ref`'s reconstructed spectrum and both harmonic index tables are
  sized to the FFT and zeroed, where upstream's are shorter and
  uninitialised; every entry the code reads is written first, so this only
  removes the chance of an out-of-range index.
* **The test build is optimised.** `[profile.test]` in `Cargo.toml` sets
  `opt-level = 2` so the no-panic sweeps (`tests/no_panic.rs`, 200,000
  hostile frames into the decoder and thousands of extreme and random frames
  into the encoder) take seconds. Overflow checks and debug assertions stay
  on, and a test checks that they do.
