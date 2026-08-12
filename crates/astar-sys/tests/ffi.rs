// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Offline tests of the C-ABI, exercised through the `rlib` crate-type by
//! calling the `extern "C"` functions directly. No network, no fixed ports,
//! no hardware: only handle lifecycle, idle snapshots, null-guards, and error
//! codes are covered (live dial is manual parrot acceptance).

use std::ffi::CString;
use std::ptr;

use astar_sys::*;

fn null_config() -> IaxConfig {
    IaxConfig {
        input: ptr::null(),
        output: ptr::null(),
        portal_user: ptr::null(),
        portal_pass: ptr::null(),
        portal_node: ptr::null(),
        secret: ptr::null(),
        codec_policy: ptr::null(),
    }
}

// --- iax-3e53: codec_policy config string at construction ---

#[test]
fn codec_policy_null_empty_and_default_construct() {
    // NULL (covered by null_config everywhere), "", and "default" all select
    // the library default policy — construction succeeds.
    for policy in ["", "default"] {
        let c = CString::new(policy).unwrap();
        let cfg = IaxConfig {
            codec_policy: c.as_ptr(),
            ..null_config()
        };
        let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
        assert!(!st.is_null(), "policy string {policy:?} must construct");
        unsafe { iax_station_free(st) };
    }
}

#[test]
fn codec_policy_documented_strings_construct() {
    for policy in ["ulaw_only", "allow_slin", "prefer_slin", "prefer_slin16"] {
        let c = CString::new(policy).unwrap();
        let cfg = IaxConfig {
            codec_policy: c.as_ptr(),
            ..null_config()
        };
        let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
        assert!(!st.is_null(), "policy string {policy:?} must construct");
        unsafe { iax_station_free(st) };
    }
}

#[test]
fn codec_policy_invalid_string_fails_construction() {
    // An unknown policy string must fail construction (NULL), not silently
    // fall back to the default.
    for bad in ["opus", "slin16", "PREFER_SLIN16", "prefer_slin16 "] {
        let c = CString::new(bad).unwrap();
        let cfg = IaxConfig {
            codec_policy: c.as_ptr(),
            ..null_config()
        };
        let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
        assert!(st.is_null(), "policy string {bad:?} must fail construction");
    }
}

// --- Task 3.2: handle lifecycle ---

#[test]
fn new_then_free() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert!(!st.is_null());
    unsafe { iax_station_free(st) };
}

#[test]
fn new_with_null_config_is_null() {
    let st = unsafe { iax_station_new(ptr::null()) };
    assert!(st.is_null());
}

#[test]
fn free_null_is_noop() {
    unsafe { iax_station_free(ptr::null_mut()) };
}

// --- Task 3.3: snapshot fill + error codes ---

#[test]
fn snapshot_fills_idle_state() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    // Pre-poison the struct so we can prove it is overwritten.
    let mut state = IaxState {
        status: IaxStatus::Hangup,
        ptt: true,
        remote_ptt: true,
        tx_db: 0.0,
        rx_db: 0.0,
        input_db: 0.0,
        rtt_ms: 99,
        mode: IaxMode::Node,
        tx_reanchors: 7,
        tx_capture_overruns: 11,
        negotiated_format: 64,
        dtmf_played: 5,
        dtmf_total: 9,
        m17_available: true,
        m17_active: true,
        dstar_available: true,
        dstar_active: true,
    };
    let rc = unsafe { iax_station_snapshot(st, std::ptr::from_mut(&mut state)) };
    assert_eq!(rc, IAX_OK);
    assert!(matches!(state.status, IaxStatus::Idle));
    assert!(!state.ptt);
    assert!(!state.remote_ptt);
    // iax-f2b8 Task 5: a never-connected station never reports a live M17
    // session, proving the field is overwritten from the pre-poisoned value.
    assert!(!state.m17_active);
    // iax-4c8e: likewise for D-Star. `dstar_available` is deliberately NOT
    // asserted here — it reports whether a ThumbDV is plugged into the
    // machine running the test, which is not ours to pin.
    assert!(!state.dstar_active);
    assert_eq!(state.rtt_ms, -1);
    // iax-5c30: idle (no active call) reports the silence floor.
    assert!((state.input_db + 60.0).abs() < 1e-3);
    // A fresh station defaults to WT mode.
    assert!(matches!(state.mode, IaxMode::Wt));
    // iax-9e55: idle (no active call) zeroes the TX health counters, proving
    // they are overwritten from the pre-poisoned values.
    assert_eq!(state.tx_reanchors, 0);
    assert_eq!(state.tx_capture_overruns, 0);
    // iax-3e53: idle (no active call) reports no negotiated codec (0),
    // overwriting the pre-poisoned value.
    assert_eq!(state.negotiated_format, 0);

    // set_ptt while idle → not connected.
    assert_eq!(
        unsafe { iax_station_set_ptt(st, true) },
        IAX_ERR_NOT_CONNECTED
    );

    unsafe { iax_station_free(st) };
}

#[test]
fn null_guards_return_err_null() {
    let mut state = IaxState {
        status: IaxStatus::Idle,
        ptt: false,
        remote_ptt: false,
        tx_db: 0.0,
        rx_db: 0.0,
        input_db: 0.0,
        rtt_ms: -1,
        mode: IaxMode::Wt,
        tx_reanchors: 0,
        tx_capture_overruns: 0,
        negotiated_format: 0,
        dtmf_played: 5,
        dtmf_total: 9,
        m17_available: false,
        m17_active: false,
        dstar_available: false,
        dstar_active: false,
    };
    assert_eq!(
        unsafe { iax_station_snapshot(ptr::null_mut(), std::ptr::from_mut(&mut state)) },
        IAX_ERR_NULL
    );
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert_eq!(
        unsafe { iax_station_snapshot(st, ptr::null_mut()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_ptt(ptr::null_mut(), true) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_disconnect(ptr::null_mut()) },
        IAX_ERR_NULL
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn send_dtmf_idle_and_validation_codes() {
    // Reinterpret an ASCII byte as the platform `c_char` (i8 or u8) without a
    // sign-wrap lint.
    let cc = |b: u8| core::ffi::c_char::from_ne_bytes([b]);
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };

    // Valid DTMF keys — now the full 16-key set, A-D included — with no active
    // call → not connected.
    for key in *b"5*#AD" {
        assert_eq!(
            unsafe { iax_station_send_dtmf(st, cc(key)) },
            IAX_ERR_NOT_CONNECTED,
            "byte {key} is a valid DTMF key → not-connected (no active call)"
        );
    }
    // A non-DTMF character → invalid digit, and validation happens before the
    // not-connected check.
    for bad in *b"xE +" {
        assert_eq!(
            unsafe { iax_station_send_dtmf(st, cc(bad)) },
            IAX_ERR_INVALID_DIGIT,
            "byte {bad} must be rejected as an invalid digit"
        );
    }
    // NULL handle → null guard.
    assert_eq!(
        unsafe { iax_station_send_dtmf(ptr::null_mut(), cc(b'5')) },
        IAX_ERR_NULL
    );

    unsafe { iax_station_free(st) };
}

// --- iax-7fff: per-call DTMF emission mode (in-band tone vs protocol frames) ---

#[test]
fn set_dtmf_mode_accepts_both_modes() {
    // The setter stores the mode on the station (applies to future digits), so
    // idle is fine — both documented modes return IAX_OK, repeatedly.
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert_eq!(
        unsafe { iax_station_set_dtmf_mode(st, IaxDtmfMode::Protocol) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_dtmf_mode(st, IaxDtmfMode::InBand) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_dtmf_mode_null_guard() {
    assert_eq!(
        unsafe { iax_station_set_dtmf_mode(ptr::null_mut(), IaxDtmfMode::InBand) },
        IAX_ERR_NULL
    );
}

#[test]
fn next_event_idle_is_none() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    let mut ev = IaxEvent {
        kind: IaxEventKind::Answered,
        remote_ptt: true,
    };
    let rc = unsafe { iax_station_next_event(st, std::ptr::from_mut(&mut ev)) };
    assert_eq!(rc, IAX_OK);
    assert!(matches!(ev.kind, IaxEventKind::None));
    unsafe { iax_station_free(st) };
}

#[test]
fn connect_wt_without_portal_is_portal_err() {
    let cfg = null_config(); // no portal_* → no WT path
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    let node = CString::new("55553").unwrap();
    let rc = unsafe { iax_station_connect_wt(st, node.as_ptr()) };
    assert_eq!(rc, IAX_ERR_PORTAL);
    unsafe { iax_station_free(st) };
}

// --- iax-6b58: standalone WT credential validation (mint + discard) ---

#[test]
fn mint_token_without_portal_is_portal_err() {
    // No portal_* fields → no WT credentials held → a clean Portal error, and
    // crucially no call is placed (offline, deterministic).
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    let rc = unsafe { iax_station_mint_token(st) };
    assert_eq!(rc, IAX_ERR_PORTAL);
    // The mint opened no call: the station is still Idle.
    let mut state = IaxState {
        status: IaxStatus::Hangup,
        ptt: false,
        remote_ptt: false,
        tx_db: 0.0,
        rx_db: 0.0,
        input_db: 0.0,
        rtt_ms: -1,
        mode: IaxMode::Wt,
        tx_reanchors: 0,
        tx_capture_overruns: 0,
        negotiated_format: 0,
        dtmf_played: 5,
        dtmf_total: 9,
        m17_available: false,
        m17_active: false,
        dstar_available: false,
        dstar_active: false,
    };
    assert_eq!(
        unsafe { iax_station_snapshot(st, std::ptr::from_mut(&mut state)) },
        IAX_OK
    );
    assert!(matches!(state.status, IaxStatus::Idle));
    unsafe { iax_station_free(st) };
}

#[test]
fn mint_token_null_guard() {
    assert_eq!(
        unsafe { iax_station_mint_token(ptr::null_mut()) },
        IAX_ERR_NULL
    );
}

#[test]
fn disconnect_idle_is_ok() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert_eq!(unsafe { iax_station_disconnect(st) }, IAX_OK);
    unsafe { iax_station_free(st) };
}

#[test]
fn gains_are_ok() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert_eq!(unsafe { iax_station_set_input_gain(st, 0.5) }, IAX_OK);
    assert_eq!(unsafe { iax_station_set_output_gain(st, 1.5) }, IAX_OK);
    // iax-a4e7: output gain's ceiling is 4.0, above the input side's 2.0 —
    // the FFI entry point itself imposes no ceiling (Station/ConsoleSession
    // clamp underneath), so a value above the OLD 2.0 ceiling must still
    // return IAX_OK end to end.
    assert_eq!(unsafe { iax_station_set_output_gain(st, 3.5) }, IAX_OK);
    unsafe { iax_station_free(st) };
}

#[test]
fn rx_compression_toggles_are_ok() {
    // iax-a4e7 PHASE 1: RX/output compression, mirroring the mic-side
    // compression FFI smoke test.
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert_eq!(unsafe { iax_station_set_rx_compression(st, true) }, IAX_OK);
    assert_eq!(unsafe { iax_station_set_rx_compression(st, false) }, IAX_OK);
    // Strength: in-range, and out-of-range (clamped underneath, still OK).
    assert_eq!(
        unsafe { iax_station_set_rx_compression_level(st, 0.5) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_rx_compression_level(st, 2.0) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_rx_compression_level(st, -1.0) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn rx_compression_toggles_null_guard() {
    assert_eq!(
        unsafe { iax_station_set_rx_compression(ptr::null_mut(), true) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_rx_compression_level(ptr::null_mut(), 0.5) },
        IAX_ERR_NULL
    );
}

#[test]
fn mic_dsp_toggles_are_ok() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    // Idle (no active call) is fine — the toggle is stored and applied on connect.
    assert_eq!(unsafe { iax_station_set_compression(st, true) }, IAX_OK);
    assert_eq!(unsafe { iax_station_set_compression(st, false) }, IAX_OK);
    assert_eq!(unsafe { iax_station_set_noise_reduction(st, true) }, IAX_OK);
    assert_eq!(
        unsafe { iax_station_set_noise_reduction(st, false) },
        IAX_OK
    );
    // Compression level: in-range, and out-of-range (clamped, still OK).
    assert_eq!(
        unsafe { iax_station_set_compression_level(st, 0.5) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_compression_level(st, 2.0) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_compression_level(st, -1.0) },
        IAX_OK
    );
    // VOX pre-roll (iax-2733): disabled, in-range, and over-the-cap (clamped).
    assert_eq!(unsafe { iax_station_set_vox_preroll_ms(st, 0) }, IAX_OK);
    assert_eq!(unsafe { iax_station_set_vox_preroll_ms(st, 150) }, IAX_OK);
    assert_eq!(
        unsafe { iax_station_set_vox_preroll_ms(st, 10_000) },
        IAX_OK
    );
    // Spectrum decay (iax-8616): default, in-range, and out-of-range (clamped,
    // still OK) — idle is fine (applies to currently-live analyzers only).
    assert_eq!(unsafe { iax_station_set_spectrum_decay(st, 100.0) }, IAX_OK);
    assert_eq!(unsafe { iax_station_set_spectrum_decay(st, 5.0) }, IAX_OK);
    assert_eq!(
        unsafe { iax_station_set_spectrum_decay(st, 99_999.0) },
        IAX_OK
    );
    assert_eq!(unsafe { iax_station_set_spectrum_decay(st, -1.0) }, IAX_OK);
    unsafe { iax_station_free(st) };
}

#[test]
fn mic_dsp_toggles_null_guard() {
    assert_eq!(
        unsafe { iax_station_set_compression(ptr::null_mut(), true) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_noise_reduction(ptr::null_mut(), true) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_compression_level(ptr::null_mut(), 0.5) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_vox_preroll_ms(ptr::null_mut(), 100) },
        IAX_ERR_NULL
    );
    // iax-8616: NULL station → IAX_ERR_NULL on the spectrum-decay setter.
    assert_eq!(
        unsafe { iax_station_set_spectrum_decay(ptr::null_mut(), 100.0) },
        IAX_ERR_NULL
    );
}

#[test]
fn monitor_null_guards() {
    // iax-2377: NULL station → IAX_ERR_NULL on both monitor fns (input may be
    // NULL — that means "system default" — so it is not a guard).
    assert_eq!(
        unsafe { iax_station_monitor_start(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_monitor_stop(ptr::null_mut()) },
        IAX_ERR_NULL
    );
}

#[test]
fn monitor_stop_when_idle_is_noop() {
    // Stopping a monitor that was never started is a harmless no-op.
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    assert_eq!(unsafe { iax_station_monitor_stop(st) }, IAX_OK);
    unsafe { iax_station_free(st) };
}

#[test]
fn mic_spectrum_null_guards_and_idle_zero() {
    // iax-e73e: NULL station → IAX_ERR_NULL.
    let mut buf = [0.0_f32; 64];
    assert_eq!(
        unsafe { iax_station_mic_spectrum(ptr::null_mut(), buf.as_mut_ptr(), buf.len()) },
        IAX_ERR_NULL
    );
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    // NULL out with cap > 0 → IAX_ERR_NULL.
    assert_eq!(
        unsafe { iax_station_mic_spectrum(st, ptr::null_mut(), 64) },
        IAX_ERR_NULL
    );
    // cap == 0 is a no-op that returns 0 (sizing-style call).
    assert_eq!(
        unsafe { iax_station_mic_spectrum(st, ptr::null_mut(), 0) },
        0
    );
    // Not monitoring → 0 bins.
    assert_eq!(
        unsafe { iax_station_mic_spectrum(st, buf.as_mut_ptr(), buf.len()) },
        0
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn tx_rx_spectrum_null_guards_and_idle_zero() {
    // iax-3f34: the live-call TX/RX spectrum entrypoints share mic_spectrum's
    // contract — NULL guards, cap==0 no-op, and 0 bins with no active call.
    let mut buf = [0.0_f32; 64];
    for f in [iax_station_tx_spectrum, iax_station_rx_spectrum] {
        assert_eq!(
            unsafe { f(ptr::null_mut(), buf.as_mut_ptr(), buf.len()) },
            IAX_ERR_NULL
        );
        let cfg = null_config();
        let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
        // NULL out with cap > 0 → IAX_ERR_NULL.
        assert_eq!(unsafe { f(st, ptr::null_mut(), 64) }, IAX_ERR_NULL);
        // cap == 0 is a no-op that returns 0.
        assert_eq!(unsafe { f(st, ptr::null_mut(), 0) }, 0);
        // No active call → 0 bins.
        assert_eq!(unsafe { f(st, buf.as_mut_ptr(), buf.len()) }, 0);
        unsafe { iax_station_free(st) };
    }
}

#[test]
fn characterize_null_guard_and_idle_empty() {
    // iax-5fb6: NULL station → IAX_ERR_NULL.
    let mut buf = [0_i8; 256];
    assert_eq!(
        unsafe { iax_station_characterize(ptr::null_mut(), false, buf.as_mut_ptr(), buf.len()) },
        IAX_ERR_NULL
    );
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    // Not monitoring → empty JSON, length 0 (size query also returns 0).
    assert_eq!(
        unsafe { iax_station_characterize(st, false, ptr::null_mut(), 0) },
        0
    );
    assert_eq!(
        unsafe { iax_station_characterize(st, true, buf.as_mut_ptr(), buf.len()) },
        0
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_mic_profile_null_guards_clear_and_round_trip() {
    // iax-2095: NULL station → IAX_ERR_NULL.
    assert_eq!(
        unsafe { iax_station_set_mic_profile(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    // NULL json CLEARS the profile (no active call) → IAX_OK.
    assert_eq!(
        unsafe { iax_station_set_mic_profile(st, ptr::null()) },
        IAX_OK
    );
    // A valid secret-free MicProfile JSON applies → IAX_OK.
    let good = CString::new(
        r#"{"highpass_hz":90.0,"notches":[{"freq_hz":588.0,"q":20.0}],"noise_floor_dbfs":-52.0,"gate_threshold_db":-46.0}"#,
    )
    .unwrap();
    assert_eq!(
        unsafe { iax_station_set_mic_profile(st, good.as_ptr()) },
        IAX_OK
    );
    // Malformed JSON → IAX_ERR_AUDIO (never a panic).
    let bad = CString::new("{not valid json").unwrap();
    assert_eq!(
        unsafe { iax_station_set_mic_profile(st, bad.as_ptr()) },
        IAX_ERR_AUDIO
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_devices_then_unset() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    let mic = CString::new("USB Audio").unwrap();
    assert_eq!(
        unsafe { iax_station_set_devices(st, mic.as_ptr(), ptr::null()) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_devices(st, ptr::null(), ptr::null()) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn list_inputs_sizing_query_and_fill() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    // Sizing query (len == 0) returns the needed byte count without writing.
    let needed = unsafe { iax_station_list_inputs(st, ptr::null_mut(), 0) };
    assert!(needed >= 0, "needed = {needed}");
    // Fill into a generous buffer.
    let mut buf = vec![0i8; 4096];
    let n = unsafe { iax_station_list_inputs(st, buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, needed);
    unsafe { iax_station_free(st) };
}

#[test]
fn error_text_is_static_and_secret_free() {
    use std::ffi::CStr;
    for code in [
        IAX_OK,
        IAX_ERR_PANIC,
        IAX_ERR_NULL,
        IAX_ERR_NOT_CONNECTED,
        IAX_ERR_ALREADY_CONNECTED,
        IAX_ERR_PORTAL,
        IAX_ERR_RESOLVE,
        IAX_ERR_AUDIO,
        IAX_ERR_IAX,
        IAX_ERR_SERIAL,
        IAX_ERR_UTF8,
        -999, // unknown
    ] {
        let p = unsafe { iax_error_text(code) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        let low = s.to_lowercase();
        assert!(!low.contains("secret"), "code {code}: {s}");
        assert!(!low.contains("password"), "code {code}: {s}");
        assert!(!low.contains("token"), "code {code}: {s}");
    }
}

// --- iax-4cfe: WT/Node mode + node config surface (offline) ---

#[test]
fn fresh_station_reports_wt_mode() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };
    let mut mode = IaxMode::Node;
    let rc = unsafe { iax_station_mode(st, std::ptr::from_mut(&mut mode)) };
    assert_eq!(rc, IAX_OK);
    assert!(matches!(mode, IaxMode::Wt));
    unsafe { iax_station_free(st) };
}

fn node_cfg() -> (IaxNodeConfig, CString) {
    // Ephemeral loopback bind keeps this hardware/port-free; storing the config
    // does not start the listener (that happens on set_mode(Node)).
    let bind = CString::new("127.0.0.1:0").unwrap();
    let cfg = IaxNodeConfig {
        bind: bind.as_ptr(),
        answer: IaxAnswerPolicy::Manual,
        auth: IaxAuthPolicy::Off,
        registrar: ptr::null(),
        register_user: ptr::null(),
        refresh_secs: 0,
    };
    (cfg, bind)
}

#[test]
fn set_node_config_stores_listen_only_config() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let (ncfg, _bind) = node_cfg();
    let rc = unsafe { iax_station_set_node_config(st, std::ptr::from_ref(&ncfg)) };
    assert_eq!(rc, IAX_OK);
    unsafe { iax_station_free(st) };
}

#[test]
fn set_node_config_rejects_bad_bind_address() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let bind = CString::new("not-an-address").unwrap();
    let ncfg = IaxNodeConfig {
        bind: bind.as_ptr(),
        answer: IaxAnswerPolicy::Auto,
        auth: IaxAuthPolicy::Off,
        registrar: ptr::null(),
        register_user: ptr::null(),
        refresh_secs: 0,
    };
    let rc = unsafe { iax_station_set_node_config(st, std::ptr::from_ref(&ncfg)) };
    assert_eq!(rc, IAX_ERR_RESOLVE);
    unsafe { iax_station_free(st) };
}

#[test]
fn set_node_config_registrar_without_user_is_null_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let bind = CString::new("0.0.0.0:4569").unwrap();
    let registrar = CString::new("127.0.0.1:4569").unwrap();
    let ncfg = IaxNodeConfig {
        bind: bind.as_ptr(),
        answer: IaxAnswerPolicy::Auto,
        auth: IaxAuthPolicy::Off,
        registrar: registrar.as_ptr(),
        register_user: ptr::null(), // missing → error, no secret involved
        refresh_secs: 60,
    };
    let rc = unsafe { iax_station_set_node_config(st, std::ptr::from_ref(&ncfg)) };
    assert_eq!(rc, IAX_ERR_NULL);
    unsafe { iax_station_free(st) };
}

#[test]
fn answer_and_reject_in_wt_mode_are_not_connected() {
    // `answer`/`reject` in WT mode (or with no pending offer) now return
    // IAX_ERR_NOT_CONNECTED (-3) — `ModeMismatch` is retired; the semantic
    // is "no pending call to answer/reject".
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    assert_eq!(unsafe { iax_station_answer(st) }, IAX_ERR_NOT_CONNECTED);
    assert_eq!(unsafe { iax_station_reject(st) }, IAX_ERR_NOT_CONNECTED);
    unsafe { iax_station_free(st) };
}

#[test]
fn incoming_from_is_empty_before_any_call() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let needed = unsafe { iax_station_incoming_from(st, ptr::null_mut(), 0) };
    assert_eq!(needed, 0, "no incoming caller id yet");
    unsafe { iax_station_free(st) };
}

#[test]
fn node_surface_null_guards() {
    assert_eq!(
        unsafe { iax_station_set_mode(ptr::null_mut(), IaxMode::Node) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_mode(ptr::null_mut(), ptr::null_mut()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_node_config(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );
    assert_eq!(unsafe { iax_station_answer(ptr::null_mut()) }, IAX_ERR_NULL);
    assert_eq!(unsafe { iax_station_reject(ptr::null_mut()) }, IAX_ERR_NULL);
    assert_eq!(
        unsafe { iax_station_incoming_from(ptr::null_mut(), ptr::null_mut(), 0) },
        IAX_ERR_NULL
    );
}

// --- iax-a1fb P4: enable_inbound / disable_inbound / register / deregister ---

#[test]
fn enable_disable_inbound_over_ffi() {
    // Build a station, set node config bound to an ephemeral loopback port,
    // then toggle the inbound listener.  NULL registrar means no registration.
    unsafe {
        let scfg = null_config();
        let st = iax_station_new(std::ptr::from_ref(&scfg));
        assert!(!st.is_null());
        let (nc, _bind) = node_cfg(); // 127.0.0.1:0, no registrar
        assert_eq!(
            iax_station_set_node_config(st, std::ptr::from_ref(&nc)),
            IAX_OK
        );
        assert_eq!(
            iax_station_enable_inbound(st, std::ptr::from_ref(&nc)),
            IAX_OK
        );
        assert_eq!(iax_station_disable_inbound(st), IAX_OK);
        iax_station_free(st);
    }
}

#[test]
fn enable_inbound_deregister_null_guards() {
    // NULL station handle → IAX_ERR_NULL on all four new fns.
    assert_eq!(
        unsafe { iax_station_enable_inbound(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_disable_inbound(ptr::null_mut()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_register(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_deregister(ptr::null_mut()) },
        IAX_ERR_NULL
    );
}

#[test]
fn set_credential_resolver_accepts_callback_and_null_guards() {
    extern "C" fn noop(
        _user: *const std::ffi::c_char,
        _out: *mut std::ffi::c_char,
        _cap: usize,
        _data: *mut std::ffi::c_void,
    ) -> i32 {
        IAX_OK
    }
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    assert_eq!(
        unsafe { iax_station_set_credential_resolver(st, noop, ptr::null_mut()) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

// ---------------------------------------------------------------------------
// Link surface (iax-1075)
// ---------------------------------------------------------------------------

#[test]
fn link_null_guards_return_err_null() {
    let node = CString::new("55553").unwrap();
    let cid = CString::new("cid").unwrap();
    let sec = CString::new("s").unwrap();
    assert_eq!(
        unsafe {
            iax_station_link_connect(
                ptr::null_mut(),
                node.as_ptr(),
                IaxLinkMode::Monitor,
                cid.as_ptr(),
                sec.as_ptr(),
                false,
            )
        },
        IAX_ERR_NULL
    );
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    assert_eq!(
        unsafe {
            iax_station_link_connect(
                st,
                ptr::null(),
                IaxLinkMode::Monitor,
                cid.as_ptr(),
                sec.as_ptr(),
                false,
            )
        },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe {
            iax_station_link_connect_at(
                st,
                node.as_ptr(),
                ptr::null(),
                IaxLinkMode::Monitor,
                cid.as_ptr(),
                sec.as_ptr(),
                false,
            )
        },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_link_disconnect(st, ptr::null()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_link_next_event(st, ptr::null_mut()) },
        IAX_ERR_NULL
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn link_ops_without_engine_report_link_error_not_crash() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let node = CString::new("55553").unwrap();
    assert_eq!(
        unsafe { iax_station_link_disconnect(st, node.as_ptr()) },
        IAX_ERR_LINK,
        "no engine/link yet -> IAX_ERR_LINK"
    );
    assert_eq!(
        unsafe { iax_station_link_set_mode(st, node.as_ptr(), IaxLinkMode::Monitor) },
        IAX_ERR_LINK
    );
    assert_eq!(
        unsafe { iax_station_link_key(st, node.as_ptr(), true) },
        IAX_ERR_LINK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn link_roster_json_is_empty_list_before_any_link() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let mut buf = [0i8; 256];
    let n = unsafe { iax_station_link_roster_json(st, buf.as_mut_ptr(), buf.len()) };
    assert!(n >= 0, "roster json fills, got {n}");
    let json = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap();
    assert_eq!(json, r#"{"links":[]}"#);
    unsafe { iax_station_free(st) };
}

#[test]
fn link_next_event_with_none_pending_returns_zero_and_kind_none() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let mut ev = IaxLinkEvent {
        kind: IaxLinkEventKind::Keyed,
        call: 7,
        keyed: true,
    };
    let rc = unsafe { iax_station_link_next_event(st, &raw mut ev) };
    assert_eq!(rc, 0, "no events pending");
    assert!(ev.kind == IaxLinkEventKind::None && ev.call == 0 && !ev.keyed);
    unsafe { iax_station_free(st) };
}

// ---------------------------------------------------------------------------
// WireGuard link transport (iax-912e)
// ---------------------------------------------------------------------------

/// 32 zero bytes base64 — length-valid x25519 key material (mirrors the
/// engine's transport tests). Never a real secret.
const WG_KEY32: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// Owned C strings backing a valid [`IaxWireguardConfig`]; the config borrows
/// them, so the struct must outlive the `iax_station_set_wireguard` call.
struct WgCfgStrings {
    endpoint: CString,
    peer_public_key: CString,
    tunnel_ip: CString,
    private_key: CString,
}

impl WgCfgStrings {
    fn valid() -> Self {
        // A loopback endpoint keeps this offline: setting the transport with
        // no engine defers the tunnel build, so no traffic is sent here.
        Self {
            endpoint: CString::new("127.0.0.1:51820").unwrap(),
            peer_public_key: CString::new(WG_KEY32).unwrap(),
            tunnel_ip: CString::new("10.99.0.2/32").unwrap(),
            private_key: CString::new(WG_KEY32).unwrap(),
        }
    }

    fn cfg(&self) -> IaxWireguardConfig {
        IaxWireguardConfig {
            endpoint: self.endpoint.as_ptr(),
            peer_public_key: self.peer_public_key.as_ptr(),
            tunnel_ip: self.tunnel_ip.as_ptr(),
            allowed_ips: ptr::null(),
            keepalive_secs: 0,
            private_key: self.private_key.as_ptr(),
            also_bind_udp: ptr::null(),
        }
    }
}

#[test]
fn set_wireguard_valid_config_then_null_clear_round_trips() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    let cfg = s.cfg();
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_OK,
        "valid config before connect must be accepted"
    );
    // NULL config clears back to plain OS UDP.
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, ptr::null()) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_accepts_optional_fields() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    // Comma-separated allowed-IPs (with breathing space) + an extra plain UDP
    // bind + an explicit keepalive.
    let allowed = CString::new("10.99.0.0/24, 192.168.7.0/24").unwrap();
    let also = CString::new("0.0.0.0:4569").unwrap();
    let cfg = IaxWireguardConfig {
        allowed_ips: allowed.as_ptr(),
        also_bind_udp: also.as_ptr(),
        keepalive_secs: 15,
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_OK
    );
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, ptr::null()) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_null_station_guard() {
    let s = WgCfgStrings::valid();
    let cfg = s.cfg();
    assert_eq!(
        unsafe { iax_station_set_wireguard(ptr::null_mut(), std::ptr::from_ref(&cfg)) },
        IAX_ERR_NULL
    );
    // NULL station beats NULL config.
    assert_eq!(
        unsafe { iax_station_set_wireguard(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );
}

#[test]
fn set_wireguard_missing_required_field_is_null_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    for cfg in [
        IaxWireguardConfig {
            endpoint: ptr::null(),
            ..s.cfg()
        },
        IaxWireguardConfig {
            peer_public_key: ptr::null(),
            ..s.cfg()
        },
        IaxWireguardConfig {
            tunnel_ip: ptr::null(),
            ..s.cfg()
        },
        IaxWireguardConfig {
            private_key: ptr::null(),
            ..s.cfg()
        },
    ] {
        assert_eq!(
            unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
            IAX_ERR_NULL,
            "each required field must be NULL-guarded"
        );
    }
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_non_utf8_string_is_utf8_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    // A NUL-terminated byte string that is not valid UTF-8.
    let bad = [0xffu8, 0xfe, 0x00];
    let cfg = IaxWireguardConfig {
        endpoint: bad.as_ptr().cast(),
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_ERR_UTF8
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_bad_tunnel_cidr_is_link_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    let bad = CString::new("not-a-cidr").unwrap();
    let cfg = IaxWireguardConfig {
        tunnel_ip: bad.as_ptr(),
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_ERR_LINK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_bad_peer_key_base64_is_link_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    let bad = CString::new("not-base64!!").unwrap();
    let cfg = IaxWireguardConfig {
        peer_public_key: bad.as_ptr(),
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_ERR_LINK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_bad_endpoint_is_resolve_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    // No port → immediate parse error, no DNS traffic.
    let bad = CString::new("nope").unwrap();
    let cfg = IaxWireguardConfig {
        endpoint: bad.as_ptr(),
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_ERR_RESOLVE
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_bad_allowed_ip_entry_is_link_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    let bad = CString::new("10.99.0.0/24,xyz").unwrap();
    let cfg = IaxWireguardConfig {
        allowed_ips: bad.as_ptr(),
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_ERR_LINK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_bad_also_bind_udp_is_resolve_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let s = WgCfgStrings::valid();
    let bad = CString::new("not-an-address").unwrap();
    let cfg = IaxWireguardConfig {
        also_bind_udp: bad.as_ptr(),
        ..s.cfg()
    };
    assert_eq!(
        unsafe { iax_station_set_wireguard(st, std::ptr::from_ref(&cfg)) },
        IAX_ERR_RESOLVE
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_wireguard_error_texts_never_carry_key_material() {
    // The only error-message surface across this ABI is `iax_error_text`
    // (static, generic strings). Assert none of the codes the WG setter can
    // return carries the private key passed in — defense-in-depth for the
    // house secret rule.
    use std::ffi::CStr;
    for code in [
        IAX_OK,
        IAX_ERR_NULL,
        IAX_ERR_UTF8,
        IAX_ERR_LINK,
        IAX_ERR_RESOLVE,
        IAX_ERR_ALREADY_CONNECTED,
        IAX_ERR_PANIC,
    ] {
        let p = unsafe { iax_error_text(code) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert!(!s.contains(WG_KEY32), "code {code} leaked key material");
        let low = s.to_lowercase();
        assert!(!low.contains("key"), "code {code} names key material: {s}");
    }
}

// NOTE: the AlreadyConnected refusal (set while a session is up) is covered at
// the station layer (crates/astar-station/tests/wireguard_transport.rs)
// via a shared-session arrangement this C ABI deliberately does not expose;
// the code mapping (StationError::AlreadyConnected → IAX_ERR_ALREADY_CONNECTED)
// is the same `err_code` path every other setter uses.

// --- iax-4b7a: send_dtmf_string sequencer + snapshot progress ---

#[test]
fn send_dtmf_string_idle_and_validation_codes() {
    let cfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&cfg)) };

    // Any invalid character rejects the whole command — before the
    // not-connected check, sending nothing.
    let bad = CString::new("*3x").unwrap();
    assert_eq!(
        unsafe { iax_station_send_dtmf_string(st, bad.as_ptr()) },
        IAX_ERR_INVALID_DIGIT
    );
    // An empty command has nothing to send.
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { iax_station_send_dtmf_string(st, empty.as_ptr()) },
        IAX_ERR_INVALID_DIGIT
    );
    // A valid command with no active call → not connected, nothing queued.
    let ok = CString::new("*3AD").unwrap();
    assert_eq!(
        unsafe { iax_station_send_dtmf_string(st, ok.as_ptr()) },
        IAX_ERR_NOT_CONNECTED
    );
    // Null guards: handle and string.
    assert_eq!(
        unsafe { iax_station_send_dtmf_string(ptr::null_mut(), ok.as_ptr()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_send_dtmf_string(st, ptr::null()) },
        IAX_ERR_NULL
    );

    // cancel is idle-safe and null-guarded.
    assert_eq!(unsafe { iax_station_cancel_dtmf(st) }, IAX_OK);
    assert_eq!(
        unsafe { iax_station_cancel_dtmf(ptr::null_mut()) },
        IAX_ERR_NULL
    );

    // Idle snapshot reports no sequence (the new-field-zero pattern, iax-9e55).
    let mut state = unsafe { std::mem::zeroed::<IaxState>() };
    assert_eq!(unsafe { iax_station_snapshot(st, &raw mut state) }, IAX_OK);
    assert_eq!(
        (state.dtmf_played, state.dtmf_total),
        (0, 0),
        "idle snapshot must report no DTMF sequence"
    );

    unsafe { iax_station_free(st) };
}

// ---------------------------------------------------------------------------
// M17 (iax-f2b8 Task 5)
// ---------------------------------------------------------------------------

/// Reinterpret an ASCII byte as the platform `c_char` (i8 or u8) without a
/// sign-wrap lint (mirrors `send_dtmf_idle_and_validation_codes`'s helper).
fn cc(b: u8) -> core::ffi::c_char {
    core::ffi::c_char::from_ne_bytes([b])
}

#[test]
fn connect_m17_null_guards() {
    let host = CString::new("127.0.0.1").unwrap();
    let call = CString::new("N0CALL").unwrap();
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };

    assert_eq!(
        unsafe {
            iax_station_connect_m17(
                ptr::null_mut(),
                host.as_ptr(),
                17000,
                cc(b'A'),
                call.as_ptr(),
            )
        },
        IAX_ERR_NULL,
        "NULL station"
    );
    assert_eq!(
        unsafe { iax_station_connect_m17(st, ptr::null(), 17000, cc(b'A'), call.as_ptr()) },
        IAX_ERR_NULL,
        "NULL host"
    );
    assert_eq!(
        unsafe { iax_station_connect_m17(st, host.as_ptr(), 17000, cc(b'A'), ptr::null()) },
        IAX_ERR_NULL,
        "NULL callsign"
    );
    assert_eq!(
        unsafe { iax_station_m17_disconnect(ptr::null_mut()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_set_codec_dirs(ptr::null_mut(), ptr::null()) },
        IAX_ERR_NULL
    );

    unsafe { iax_station_free(st) };
}

#[test]
fn connect_m17_utf8_guard() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    // A NUL-terminated byte string that is not valid UTF-8.
    let bad = [0xffu8, 0xfe, 0x00];
    let call = CString::new("N0CALL").unwrap();
    assert_eq!(
        unsafe { iax_station_connect_m17(st, bad.as_ptr().cast(), 17000, cc(b'A'), call.as_ptr()) },
        IAX_ERR_UTF8,
        "non-UTF-8 host"
    );
    let host = CString::new("127.0.0.1").unwrap();
    assert_eq!(
        unsafe { iax_station_connect_m17(st, host.as_ptr(), 17000, cc(b'A'), bad.as_ptr().cast()) },
        IAX_ERR_UTF8,
        "non-UTF-8 callsign"
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn connect_m17_non_ascii_module_is_m17_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let host = CString::new("127.0.0.1").unwrap();
    let call = CString::new("N0CALL").unwrap();
    // A high-bit byte (non-ASCII) is never a valid module letter, guarded
    // here before the station's own A-Z validation ever runs.
    assert_eq!(
        unsafe { iax_station_connect_m17(st, host.as_ptr(), 17000, cc(0xe9), call.as_ptr()) },
        IAX_ERR_M17
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn connect_m17_to_dead_port_does_not_hang() {
    // Non-blocking-ish per the station's contract: `connect` binds + spawns;
    // link failure surfaces later via snapshot. Hermetic: this station is
    // built via `iax_station_new` (the real `CpalBackend`, not the crate's
    // test-only `NullBackend`), so the outcome legitimately varies by
    // machine: IAX_OK (codec2 + a usable audio device are both present —
    // clean it up immediately), IAX_ERR_M17 (codec2/the `m17` feature is
    // unavailable), or IAX_ERR_AUDIO (no accessible input/output device —
    // the common case on a headless/sandboxed test runner with no mic
    // permission). Any of the three proves the call returned promptly
    // instead of hanging.
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let host = CString::new("127.0.0.1").unwrap();
    let call = CString::new("N0CALL").unwrap();
    let rc = unsafe { iax_station_connect_m17(st, host.as_ptr(), 1, cc(b'A'), call.as_ptr()) };
    assert!(
        rc == IAX_OK || rc == IAX_ERR_M17 || rc == IAX_ERR_AUDIO,
        "unexpected code {rc} connecting to a dead port"
    );
    if rc == IAX_OK {
        assert_eq!(unsafe { iax_station_m17_disconnect(st) }, IAX_OK);
    }
    unsafe { iax_station_free(st) };
}

#[test]
fn m17_disconnect_idle_is_ok() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    assert_eq!(unsafe { iax_station_m17_disconnect(st) }, IAX_OK);
    unsafe { iax_station_free(st) };
}

// --- iax-4c8e: D-Star entry points ---
//
// Deliberately NO connect-to-a-dead-port test here, unlike M17's above.
// `iax_station_connect_dstar` scans serial ports and runs the ThumbDV init
// cookbook before it ever touches the network, so such a test would seize the
// dongle out from under whatever else is using it every time anyone runs
// `cargo test`. The hardware path is covered, gated behind
// `IAX_THUMBDV_TESTS=1`, in `astar-station/tests/dstar_thumbdv_hardware.rs`.
// Everything below returns before any hardware work begins.

#[test]
fn connect_dstar_null_guards() {
    let host = CString::new("127.0.0.1").unwrap();
    let call = CString::new("N0CALL").unwrap();
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };

    assert_eq!(
        unsafe {
            iax_station_connect_dstar(
                ptr::null_mut(),
                host.as_ptr(),
                30001,
                cc(b'A'),
                call.as_ptr(),
            )
        },
        IAX_ERR_NULL,
        "NULL station"
    );
    assert_eq!(
        unsafe { iax_station_connect_dstar(st, ptr::null(), 30001, cc(b'A'), call.as_ptr()) },
        IAX_ERR_NULL,
        "NULL host"
    );
    assert_eq!(
        unsafe { iax_station_connect_dstar(st, host.as_ptr(), 30001, cc(b'A'), ptr::null()) },
        IAX_ERR_NULL,
        "NULL callsign"
    );
    assert_eq!(
        unsafe { iax_station_dstar_disconnect(ptr::null_mut()) },
        IAX_ERR_NULL
    );
    assert_eq!(
        unsafe { iax_station_dstar_state(ptr::null_mut(), ptr::null_mut(), 0) },
        IAX_ERR_NULL
    );

    unsafe { iax_station_free(st) };
}

#[test]
fn connect_dstar_utf8_guard() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    // A NUL-terminated byte string that is not valid UTF-8.
    let bad = [0xffu8, 0xfe, 0x00];
    let call = CString::new("N0CALL").unwrap();
    assert_eq!(
        unsafe {
            iax_station_connect_dstar(st, bad.as_ptr().cast(), 30001, cc(b'A'), call.as_ptr())
        },
        IAX_ERR_UTF8,
        "non-UTF-8 host"
    );
    let host = CString::new("127.0.0.1").unwrap();
    assert_eq!(
        unsafe {
            iax_station_connect_dstar(st, host.as_ptr(), 30001, cc(b'A'), bad.as_ptr().cast())
        },
        IAX_ERR_UTF8,
        "non-UTF-8 callsign"
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn connect_dstar_non_ascii_module_is_dstar_err() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let host = CString::new("127.0.0.1").unwrap();
    let call = CString::new("N0CALL").unwrap();
    // A high-bit byte (non-ASCII) is never a valid module letter, guarded
    // here before the station's own A-Z validation — and, critically, before
    // the dongle scan.
    assert_eq!(
        unsafe { iax_station_connect_dstar(st, host.as_ptr(), 30001, cc(0xe9), call.as_ptr()) },
        IAX_ERR_DSTAR
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn dstar_disconnect_idle_is_ok() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    assert_eq!(unsafe { iax_station_dstar_disconnect(st) }, IAX_OK);
    // Idempotent.
    assert_eq!(unsafe { iax_station_dstar_disconnect(st) }, IAX_OK);
    unsafe { iax_station_free(st) };
}

#[test]
fn dstar_state_with_no_session_is_an_empty_json_object() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };

    // A len == 0 call is a sizing query: it reports the needed bytes and must
    // tolerate a NULL buffer.
    let needed = unsafe { iax_station_dstar_state(st, ptr::null_mut(), 0) };
    assert_eq!(needed, 2, "\"{{}}\" is two bytes, excluding the NUL");

    let mut buf = [0i8; 64];
    let n = unsafe { iax_station_dstar_state(st, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n, 2);
    let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr().cast()) }
        .to_str()
        .expect("the JSON must be valid UTF-8");
    assert_eq!(s, "{}", "no session → an empty object, never a partial one");
    unsafe { iax_station_free(st) };
}

#[test]
fn dstar_state_truncates_safely_into_a_short_buffer() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    // One byte holds nothing but the NUL. The report must still be the FULL
    // length so a caller can detect the truncation and re-ask with a bigger
    // buffer.
    let mut buf = [0x7fi8; 1];
    let n = unsafe { iax_station_dstar_state(st, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n, 2, "the report is what the full text needs, not what fit");
    assert_eq!(
        buf[0], 0,
        "the buffer must be NUL-terminated, never overrun"
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn set_codec_dirs_null_and_empty_and_colon_list_are_ok() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    // NULL clears.
    assert_eq!(
        unsafe { iax_station_set_codec_dirs(st, ptr::null()) },
        IAX_OK
    );
    // Empty string clears.
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { iax_station_set_codec_dirs(st, empty.as_ptr()) },
        IAX_OK
    );
    // ':'-separated list.
    let dirs = CString::new("/opt/app/lib:/usr/local/lib").unwrap();
    assert_eq!(
        unsafe { iax_station_set_codec_dirs(st, dirs.as_ptr()) },
        IAX_OK
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn idle_snapshot_m17_active_is_false() {
    let scfg = null_config();
    let st = unsafe { iax_station_new(std::ptr::from_ref(&scfg)) };
    let mut state = unsafe { std::mem::zeroed::<IaxState>() };
    assert_eq!(unsafe { iax_station_snapshot(st, &raw mut state) }, IAX_OK);
    assert!(!state.m17_active, "idle station must report no M17 session");
    unsafe { iax_station_free(st) };
}
