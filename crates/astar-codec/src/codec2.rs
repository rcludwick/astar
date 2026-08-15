// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Codec 2 speech vocoder — the payload codec M17 rides over (iax-f2b8).
//!
//! Two mutually-independent backends, both opt-in via Cargo features:
//!
//! - `codec2-runtime`: `dlopen`/`dlsym`s a system `libcodec2` (the C
//!   reference implementation, LGPL-2.1) at runtime via [`libloading`]. No
//!   licensed code is linked into the binary; if the library isn't present
//!   at any of the candidate paths, [`open_codec2`] just returns `None`.
//! - `codec2-static`: links the `codec2` crate directly (a pure-Rust
//!   reimplementation, dual `LGPL-2.1-only AND MIT` licensed).
//!
//! # Licensing (read before touching `Cargo.toml`)
//!
//! The `codec2` crate carries LGPL terms. It may **only** ever be pulled in
//! behind the `codec2-static` feature, and that feature must **never** be
//! part of any crate's default feature set — that's what keeps a plain
//! `cargo build` (here or in anything depending on this crate) free of LGPL
//! code. Verify with:
//!
//! ```sh
//! cargo tree -p astar-codec                                      # no codec2, no libloading
//! cargo tree -p astar-codec --features codec2-static,codec2-runtime
//! ```
//!
//! Mode is fixed at `CODEC2_MODE_3200` (64 bits / 20 ms frame, 160 samples
//! @ 8 kHz) — the rate M17 payloads use.

use std::path::PathBuf;

/// A live Codec 2 encoder/decoder instance, mode 3200: 160 PCM samples
/// (20 ms @ 8 kHz) encode to / decode from 8 bytes (64 bits).
pub trait Codec2Voice: Send {
    /// Encode one 20 ms frame of 16-bit linear PCM into 64 compressed bits.
    fn encode(&mut self, pcm: &[i16; 160]) -> [u8; 8];
    /// Decode 64 compressed bits back into one 20 ms frame of PCM.
    fn decode(&mut self, bits: &[u8; 8]) -> [i16; 160];
}

/// Which concrete implementation backed a [`Codec2Voice`] returned by
/// [`open_codec2`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec2Backend {
    /// Statically linked pure-Rust `codec2` crate (`codec2-static` feature).
    Static,
    /// `dlopen`ed system `libcodec2` (`codec2-runtime` feature).
    Runtime,
}

/// Open a Codec 2 voice instance, preferring the runtime-loaded system
/// library over the statically-linked backend.
///
/// Search order:
/// 1. `IAX_CODEC2_PATH` env var, if set — a single file path override.
/// 2. Each of `search_dirs`, joined with the platform library filename
///    (`libcodec2.dylib` / `libcodec2.so`).
/// 3. Hard-coded system paths (`/opt/homebrew/lib`, `/usr/local/lib`,
///    `/usr/lib`, with the platform extension).
///
/// Each candidate is `dlopen`ed and sanity-checked (`codec2_samples_per_frame
/// == 160 && codec2_bits_per_frame == 64` for mode 3200); a candidate that
/// fails to load or fails the sanity check is rejected (destroyed/unloaded)
/// and the search continues. Only available when `codec2-runtime` is
/// enabled; otherwise this step is skipped entirely.
///
/// If no runtime library was found (or `codec2-runtime` isn't enabled),
/// falls back to the statically-linked backend when `codec2-static` is
/// enabled. Returns `None` if neither backend is available/enabled or no
/// library could be found.
#[must_use]
pub fn open_codec2(search_dirs: &[PathBuf]) -> Option<(Box<dyn Codec2Voice>, Codec2Backend)> {
    // Always "use" the parameter: some feature combos below never reference
    // it (e.g. `codec2-static` only), and `&[PathBuf]` is a reference so this
    // costs nothing and moves nothing.
    let _ = search_dirs;

    #[cfg(feature = "codec2-runtime")]
    {
        if let Some(voice) = runtime::try_open(search_dirs) {
            return Some((Box::new(voice), Codec2Backend::Runtime));
        }
    }

    #[cfg(feature = "codec2-static")]
    {
        Some((
            Box::new(static_backend::StaticCodec2::new()),
            Codec2Backend::Static,
        ))
    }

    #[cfg(not(feature = "codec2-static"))]
    {
        None
    }
}

/// Cheap probe for whether a Codec 2 backend is available under the given
/// search dirs, without keeping the instance around.
///
/// Not cached: `search_dirs` legitimately varies between calls (tests point
/// it at different fixture directories), and a cache would need to be keyed
/// on that argument to stay correct — the probe itself (a handful of
/// `dlopen`/function calls, or none at all for the static backend) is cheap
/// enough that a plain re-probe is simpler and safer than stale state.
#[must_use]
pub fn codec2_available(search_dirs: &[PathBuf]) -> bool {
    open_codec2(search_dirs).is_some()
}

// ---------------------------------------------------------------------
// Static backend: links the pure-Rust `codec2` crate directly.
// ---------------------------------------------------------------------

#[cfg(feature = "codec2-static")]
mod static_backend {
    use super::Codec2Voice;

    /// Wraps `codec2::Codec2` (pinned `codec2 = "0.3.1"`, a pure-Rust
    /// reimplementation — no FFI, no unsafe). Its API: `Codec2::new(mode)`,
    /// `.samples_per_frame()`, `.bits_per_frame()`,
    /// `.encode(&mut [u8], &[i16])`, `.decode(&mut [i16], &[u8])`.
    pub(super) struct StaticCodec2 {
        inner: codec2::Codec2,
    }

    impl StaticCodec2 {
        pub(super) fn new() -> Self {
            let inner = codec2::Codec2::new(codec2::Codec2Mode::MODE_3200);
            debug_assert_eq!(inner.samples_per_frame(), 160);
            debug_assert_eq!(inner.bits_per_frame(), 64);
            Self { inner }
        }
    }

    impl Codec2Voice for StaticCodec2 {
        fn encode(&mut self, pcm: &[i16; 160]) -> [u8; 8] {
            let mut bits = [0u8; 8];
            self.inner.encode(&mut bits, pcm);
            bits
        }

        fn decode(&mut self, bits: &[u8; 8]) -> [i16; 160] {
            let mut out = [0i16; 160];
            self.inner.decode(&mut out, bits);
            out
        }
    }
}

// ---------------------------------------------------------------------
// Runtime backend: dlopen's a system libcodec2 via `libloading`.
// ---------------------------------------------------------------------

#[cfg(feature = "codec2-runtime")]
mod runtime {
    use super::Codec2Voice;
    use libloading::{Library, Symbol};
    use std::ffi::c_void;
    use std::os::raw::c_int;
    use std::path::{Path, PathBuf};

    /// `CODEC2_MODE_3200` from `codec2.h`.
    const CODEC2_MODE_3200: c_int = 0;

    type CreateFn = unsafe extern "C" fn(c_int) -> *mut c_void;
    type DestroyFn = unsafe extern "C" fn(*mut c_void);
    // codec2.h: `void codec2_encode(struct CODEC2*, unsigned char *bits, short *speech_in)`
    // (speech_in is non-const on the C side, even though it isn't mutated).
    type EncodeFn = unsafe extern "C" fn(*mut c_void, *mut u8, *mut i16);
    // codec2.h: `void codec2_decode(struct CODEC2*, short *speech_out, const unsigned char *bits)`
    type DecodeFn = unsafe extern "C" fn(*mut c_void, *mut i16, *const u8);
    type QueryFn = unsafe extern "C" fn(*mut c_void) -> c_int;

    #[cfg(target_os = "macos")]
    fn platform_lib_name() -> &'static str {
        "libcodec2.dylib"
    }
    #[cfg(not(target_os = "macos"))]
    fn platform_lib_name() -> &'static str {
        "libcodec2.so"
    }

    #[cfg(target_os = "macos")]
    fn default_lib_paths() -> &'static [&'static str] {
        &[
            "/opt/homebrew/lib/libcodec2.dylib",
            "/usr/local/lib/libcodec2.dylib",
            "/usr/lib/libcodec2.dylib",
        ]
    }
    #[cfg(not(target_os = "macos"))]
    fn default_lib_paths() -> &'static [&'static str] {
        &[
            "/opt/homebrew/lib/libcodec2.so",
            "/usr/local/lib/libcodec2.so",
            "/usr/lib/libcodec2.so",
        ]
    }

    /// Build the ordered candidate list: env override, then `search_dirs`
    /// joined with the platform lib filename, then hard-coded system paths.
    fn candidate_paths(search_dirs: &[PathBuf]) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        if let Ok(over) = std::env::var("IAX_CODEC2_PATH") {
            candidates.push(PathBuf::from(over));
        }
        for dir in search_dirs {
            candidates.push(dir.join(platform_lib_name()));
        }
        candidates.extend(default_lib_paths().iter().map(PathBuf::from));
        candidates
    }

    /// Runtime-loaded Codec 2 instance. Holds the `libloading::Library`
    /// alongside the raw fn pointers resolved from it: once a `Symbol` is
    /// dereferenced to a bare fn pointer (required to store it in a struct
    /// independent of the `Symbol`'s borrow), the compiler no longer tracks
    /// the lifetime relationship to the library — it is on us to keep
    /// `_lib` alive for as long as `encode_fn`/`decode_fn`/`destroy_fn` might
    /// be called, which owning it here (dropped only when `RuntimeCodec2`
    /// itself is dropped) guarantees.
    pub(super) struct RuntimeCodec2 {
        state: *mut c_void,
        encode_fn: EncodeFn,
        decode_fn: DecodeFn,
        destroy_fn: DestroyFn,
        _lib: Library,
    }

    impl Drop for RuntimeCodec2 {
        fn drop(&mut self) {
            // SAFETY: `state` was produced by `codec2_create` (from this same
            // `_lib`) and has not been freed anywhere else. `destroy_fn` is a
            // valid function pointer resolved from `_lib`, which is still
            // mapped at this point: Rust runs an explicit `Drop::drop` body
            // before recursively dropping the struct's fields, so `_lib` has
            // not been unloaded yet when this call happens.
            unsafe { (self.destroy_fn)(self.state) };
        }
    }

    // SAFETY: `RuntimeCodec2` owns its `CODEC2*` state exclusively (it is
    // never aliased outside this struct, never shared), and every method
    // takes `&mut self`, so calls into the C library are never concurrent
    // with each other for a given instance. `codec2_encode`/`codec2_decode`/
    // `codec2_destroy` operate purely on the passed-in state pointer per
    // `codec2.h` (no hidden global/thread-local state), so moving the whole
    // bundle — pointer, fn pointers, and the `Library` that keeps them valid
    // — to another thread is sound. We deliberately do not implement `Sync`.
    unsafe impl Send for RuntimeCodec2 {}

    impl Codec2Voice for RuntimeCodec2 {
        fn encode(&mut self, pcm: &[i16; 160]) -> [u8; 8] {
            let mut bits = [0u8; 8];
            let mut pcm_buf = *pcm; // C signature wants a non-const short*
            // SAFETY: `state` is a live `CODEC2*` created for this instance
            // by `codec2_create`; `bits` (8 bytes) and `pcm_buf` (160
            // samples) match the sizes the load-time sanity check confirmed
            // (`bits_per_frame == 64`, `samples_per_frame == 160`) for mode
            // 3200. `encode_fn` was resolved from `_lib`, which outlives
            // this call.
            unsafe {
                (self.encode_fn)(self.state, bits.as_mut_ptr(), pcm_buf.as_mut_ptr());
            }
            bits
        }

        fn decode(&mut self, bits: &[u8; 8]) -> [i16; 160] {
            let mut out = [0i16; 160];
            // SAFETY: as above; `bits` is `const` on the C side so passing
            // an immutable pointer is correct.
            unsafe {
                (self.decode_fn)(self.state, out.as_mut_ptr(), bits.as_ptr());
            }
            out
        }
    }

    /// Try every candidate path in order, returning the first one that
    /// loads and passes the mode-3200 sanity check.
    pub(super) fn try_open(search_dirs: &[PathBuf]) -> Option<RuntimeCodec2> {
        for path in candidate_paths(search_dirs) {
            if !path.is_file() {
                continue;
            }
            if let Some(voice) = try_load(&path) {
                return Some(voice);
            }
        }
        None
    }

    fn try_load(path: &Path) -> Option<RuntimeCodec2> {
        // SAFETY: `Library::new` runs the target's static
        // initializers/constructors — arbitrary code from disk. Every path
        // reaching this call comes from a fixed, non-attacker-controlled
        // allow-list: the `IAX_CODEC2_PATH` env var (operator-controlled,
        // same trust level as any other env-based config), caller-supplied
        // `search_dirs`, or the hard-coded system library paths above —
        // never data derived from network input or IAX2 wire content.
        let lib = unsafe { Library::new(path) }.ok()?;

        // SAFETY: `Library::get::<T>` looks up a named symbol and reinterprets
        // it as `T`; this is sound only if the symbol really has that
        // signature. We assume the loaded library is genuinely `libcodec2`
        // per the documented `codec2.h` prototypes, and back that assumption
        // with the samples/bits-per-frame sanity check below — a
        // same-named symbol in some unrelated library with an incompatible
        // ABI would be UB, which is exactly what restricting candidate paths
        // to the curated list above (never attacker-controlled) and this
        // sanity check together guard against.
        let create: Symbol<CreateFn> = unsafe { lib.get(b"codec2_create\0") }.ok()?;
        let destroy: Symbol<DestroyFn> = unsafe { lib.get(b"codec2_destroy\0") }.ok()?;
        let encode: Symbol<EncodeFn> = unsafe { lib.get(b"codec2_encode\0") }.ok()?;
        let decode: Symbol<DecodeFn> = unsafe { lib.get(b"codec2_decode\0") }.ok()?;
        let samples_per_frame: Symbol<QueryFn> =
            unsafe { lib.get(b"codec2_samples_per_frame\0") }.ok()?;
        let bits_per_frame: Symbol<QueryFn> =
            unsafe { lib.get(b"codec2_bits_per_frame\0") }.ok()?;

        // Copy each symbol out to a bare fn pointer now, while the `Symbol`
        // borrow still proves `lib` is alive; only the encode/decode/destroy
        // pointers are kept (in `RuntimeCodec2`, alongside `lib` itself).
        let create_fn: CreateFn = *create;
        let destroy_fn: DestroyFn = *destroy;
        let encode_fn: EncodeFn = *encode;
        let decode_fn: DecodeFn = *decode;
        let samples_per_frame_fn: QueryFn = *samples_per_frame;
        let bits_per_frame_fn: QueryFn = *bits_per_frame;

        // SAFETY: `create_fn` is `codec2_create` resolved above from a still
        // -live `lib`; `CODEC2_MODE_3200` (0) is a valid mode per codec2.h.
        let state = unsafe { create_fn(CODEC2_MODE_3200) };
        if state.is_null() {
            return None;
        }

        // SAFETY: `state` was just returned by `codec2_create` and has not
        // been freed; these query functions take only the state pointer.
        let samples = unsafe { samples_per_frame_fn(state) };
        let bits = unsafe { bits_per_frame_fn(state) };
        if samples != 160 || bits != 64 {
            // Wrong sanity values: not the library/mode we expect. Destroy
            // the state and reject this candidate; `lib` unloads when it's
            // dropped at the end of this function.
            // SAFETY: `state` is still valid (just created, not yet freed)
            // and `destroy_fn` comes from the same still-live `lib`.
            unsafe { destroy_fn(state) };
            return None;
        }

        Some(RuntimeCodec2 {
            state,
            encode_fn,
            decode_fn,
            destroy_fn,
            _lib: lib,
        })
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(all(test, feature = "codec2-static"))]
mod static_tests {
    use super::open_codec2;

    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_lossless,
        clippy::cast_possible_wrap
    )]
    fn static_backend_round_trips_energy() {
        let (mut c2, _) = open_codec2(&[]).expect("static backend available under feature");
        // 160 samples of a 400 Hz tone at moderate level — codec2 must
        // preserve rough energy (it's a vocoder, not a waveform codec;
        // assert loosely).
        let pcm: [i16; 160] = core::array::from_fn(|i| {
            ((i as f32 * 400.0 * core::f32::consts::TAU / 8000.0).sin() * 8000.0) as i16
        });
        let bits = c2.encode(&pcm);
        let out = c2.decode(&bits);
        let energy = |s: &[i16]| s.iter().map(|&x| (x as i64).pow(2)).sum::<i64>() / s.len() as i64;
        assert!(
            energy(&out) > energy(&pcm) / 100,
            "decoded audio must not be silence"
        );
    }
}

#[cfg(all(test, feature = "codec2-runtime"))]
mod runtime_tests {
    use super::{Codec2Backend, open_codec2};
    use std::sync::Mutex;

    /// `IAX_CODEC2_PATH` is process-global mutable state; serialize every
    /// test that touches it so parallel test threads don't race the
    /// set/probe/remove sequence against each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn runtime_loader_fails_gracefully_when_absent() {
        let _guard = ENV_LOCK.lock().unwrap();

        let nonexistent = std::env::temp_dir().join("astar-codec2-does-not-exist.dylib");
        assert!(
            !nonexistent.exists(),
            "fixture must not exist for this test to mean anything"
        );

        // SAFETY: serialized by `ENV_LOCK` above; no other thread reads or
        // writes `IAX_CODEC2_PATH` while the guard is held.
        unsafe {
            std::env::set_var("IAX_CODEC2_PATH", &nonexistent);
        }
        let result = open_codec2(&[]);
        // SAFETY: same serialization as the `set_var` above.
        unsafe {
            std::env::remove_var("IAX_CODEC2_PATH");
        }

        // The env override is bad on purpose. Branch on whether this
        // machine happens to have a real system libcodec2 at one of the
        // fallback search paths: either no system lib exists either
        // (graceful `None`, never a panic) or a real libcodec2 was found via
        // a fallback path (the bad override correctly failed and the search
        // continued) — both are a pass. A `Static` backend is only a pass
        // when `codec2-static` is *also* enabled in this build (open_codec2
        // falls through to it once the runtime search comes up empty); if
        // this is a `codec2-runtime`-only build, seeing `Static` would mean
        // it materialized out of nowhere, which would be a bug.
        #[cfg(feature = "codec2-static")]
        match result {
            // `codec2-static` is also on in this build: a real system lib
            // absent/rejected, a real one found, or the static fallback
            // kicking in are all correct outcomes here.
            None | Some((_, Codec2Backend::Runtime | Codec2Backend::Static)) => {}
        }
        #[cfg(not(feature = "codec2-static"))]
        match result {
            None | Some((_, Codec2Backend::Runtime)) => {}
            Some((_, Codec2Backend::Static)) => {
                panic!("codec2-runtime-only build must never produce a Static backend");
            }
        }
    }
}
