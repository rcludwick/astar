# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""Offline smoke test for the ctypes Station binding (no network).

Requires the cdylib built::

    cargo build -p astar-sys          # or --release

Run::

    python3 bindings/python/test_smoke.py

Exercises: new -> snapshot(Idle) -> set_ptt(True) raises NOT_CONNECTED ->
next_event(None) -> list devices -> free. Plus a secret-free guard.
"""

import ctypes
import os
import re
import sys

from astarstation import (
    IAX_ERR_NOT_CONNECTED,
    IAX_ERR_NULL,
    IAX_ERR_RESOLVE,
    AnswerPolicy,
    AuthPolicy,
    Mode,
    DenoiseChain,
    NodeConfig,
    Station,
    StationError,
    Status,
    load_library,
)
from astarstation import _IaxEvent, _IaxState


def test_struct_layout_matches_library() -> None:
    """The ctypes mirrors must be byte-identical in size to the Rust structs.

    `iax_station_snapshot` writes `sizeof(IaxState)` bytes into a buffer this
    module allocates, so a field added on the Rust side and not mirrored here
    overflows the Python heap — a segfault whose crash site (usually the GC) is
    nowhere near the cause — and shifts every field after the divergence, which
    is how a fresh station reported `dtmf_played == 48000`. Assert the size
    directly so the next drift fails here instead.
    """
    lib = load_library()
    assert ctypes.sizeof(_IaxState) == lib.iax_state_size(), (
        f"IaxState mirror is {ctypes.sizeof(_IaxState)} bytes, "
        f"library says {lib.iax_state_size()}"
    )
    assert ctypes.sizeof(_IaxEvent) == lib.iax_event_size(), (
        f"IaxEvent mirror is {ctypes.sizeof(_IaxEvent)} bytes, "
        f"library says {lib.iax_event_size()}"
    )


def _header_struct_fields(name: str) -> list[str]:
    """Field names, in order, of the `typedef struct { ... } <name>;` in astar.h."""
    here = os.path.dirname(os.path.abspath(__file__))
    header = os.path.join(here, "..", "..", "crates", "astar-sys", "include", "astar.h")
    body = re.split(r"\}\s*" + name + r";", open(header, encoding="utf-8").read())[0]
    body = body.rsplit("typedef struct {", 1)[1]
    body = re.sub(r"/\*.*?\*/", "", body, flags=re.S)  # strip doc comments
    return [m.group(1) for m in re.finditer(r"\b(\w+)\s*;", body)]


def test_struct_field_order_matches_header() -> None:
    """The mirror's field NAMES and ORDER must match astar.h, not just its size.

    Two fields of the same width swapped is a size-clean mismatch that the size
    assertion cannot see, and it silently transposes their values.
    """
    for struct, name in ((_IaxState, "IaxState"), (_IaxEvent, "IaxEvent")):
        want = _header_struct_fields(name)
        have = [f[0] for f in struct._fields_]
        assert have == want, f"{name}: mirror {have} != header {want}"


def test_new_snapshot_idle_ptt_event_free() -> None:
    with Station() as st:
        # snapshot() -> Idle, unknown rtt.
        snap = st.snapshot()
        assert snap.status == Status.IDLE, f"expected Idle, got {snap.status}"
        assert snap.rtt_ms is None, f"expected unknown rtt, got {snap.rtt_ms}"
        assert snap.ptt is False
        assert snap.remote_ptt is False
        # TX health counters start at zero on a fresh station (iax-9e55).
        assert snap.tx_reanchors == 0
        assert snap.tx_capture_overruns == 0
        # iax-rxjb: RX health counters start at zero; the jitter buffer
        # reports Asterisk chan_iax2's defaults.
        assert snap.rx_underruns == 0
        assert snap.rx_jitter_ms == 0
        assert snap.rx_jb_depth_ms == 0
        assert snap.rx_frames_lost == 0
        assert snap.rx_frames_late == 0
        assert snap.rx_frames_ooo == 0
        assert snap.rx_jb_enabled
        assert snap.rx_jb_min_ms == 40
        assert snap.rx_jb_max_ms == 200
        # No DTMF sequence is playing on a fresh station (iax-4b7a). These two
        # read as garbage the moment the struct mirror drifts, so they double as
        # a layout canary.
        assert snap.dtmf_played == 0, f"expected 0, got {snap.dtmf_played}"
        assert snap.dtmf_total == 0, f"expected 0, got {snap.dtmf_total}"
        # Never-connected station: no call, so no negotiated codec.
        assert snap.negotiated_format == 0, f"got {snap.negotiated_format}"
        # The denoise fields are host-dependent (they depend on this machine's
        # audio devices), so assert only that they decode to their own types.
        assert isinstance(snap.denoise_chain, DenoiseChain)
        assert isinstance(snap.denoise_device_rate, int)
        assert isinstance(snap.denoise_live, bool)
        # No session of any mode is live on a fresh station.
        assert snap.dstar_active is False, f"got {snap.dstar_active}"
        assert snap.ysf_active is False, f"got {snap.ysf_active}"
        assert snap.nxdn_active is False, f"got {snap.nxdn_active}"
        assert snap.dmr_active is False, f"got {snap.dmr_active}"
        assert isinstance(snap.dstar_available, bool)
        assert isinstance(snap.ysf_available, bool)
        assert isinstance(snap.nxdn_available, bool)
        assert isinstance(snap.dmr_available, bool)

        # set_ptt(True) while idle -> NOT_CONNECTED.
        try:
            st.set_ptt(True)
        except StationError as e:
            assert e.code == IAX_ERR_NOT_CONNECTED, f"got {e.code}"
        else:
            raise AssertionError("set_ptt(True) should raise while idle")

        # next_event() -> None for an idle station.
        assert st.next_event() is None

        # Gains never raise.
        st.set_input_gain(0.5)
        st.set_output_gain(1.5)

        # Mic DSP toggles never raise (stored idle, applied on connect).
        st.set_compression(True)
        st.set_compression(False)
        st.set_noise_reduction(True)
        st.set_noise_reduction(False)
        # Compression level: in-range + out-of-range (clamped) never raise.
        st.set_compression_level(0.5)
        st.set_compression_level(2.0)
        st.set_compression_level(-1.0)


def test_secret_free_surfaces() -> None:
    """No readable surface of the binding may carry a secret/password."""
    with Station() as st:
        # Even after registering a resolver and configuring a node, no
        # secret/password may surface in any readable object.
        st.set_credential_resolver(lambda user: "be04-secret")
        st.set_node_config(bind="127.0.0.1:0", register_user="77777")
        snap = st.snapshot()
        ev = st.next_event()
        surfaces = [repr(st), str(snap), str(ev)]
        for s in surfaces:
            low = s.lower()
            assert "secret" not in low, f"'secret' leaked: {s!r}"
            assert "password" not in low, f"'password' leaked: {s!r}"
            assert "portal" not in low, f"'portal' leaked: {s!r}"


def test_default_mode_is_wt() -> None:
    """Fresh station defaults to WT mode in both mode() and snapshot()."""
    with Station() as st:
        assert st.mode() == Mode.WT, f"expected WT, got {st.mode()}"
        assert st.snapshot().mode == Mode.WT, f"expected snapshot WT, got {st.snapshot().mode}"


def test_set_node_config_listen_only_ok() -> None:
    """Listen-only node config (no registrar) succeeds and does NOT switch mode."""
    with Station() as st:
        st.set_node_config(
            bind="127.0.0.1:0",
            answer=AnswerPolicy.MANUAL,
            auth=AuthPolicy.OFF,
        )
        # No mode switch -> no listener starts.
        assert st.mode() == Mode.WT, "set_node_config must not switch mode"


def test_set_node_config_bad_bind_resolve() -> None:
    """An unparseable bind address raises IAX_ERR_RESOLVE."""
    with Station() as st:
        try:
            st.set_node_config(bind="not-an-address")
        except StationError as e:
            assert e.code == IAX_ERR_RESOLVE, f"got {e.code}"
        else:
            raise AssertionError("bad bind should raise RESOLVE")


def test_set_node_config_registrar_without_user_null() -> None:
    """A registrar without register_user raises IAX_ERR_NULL."""
    with Station() as st:
        try:
            st.set_node_config(registrar="127.0.0.1:4569", register_user=None)
        except StationError as e:
            assert e.code == IAX_ERR_NULL, f"got {e.code}"
        else:
            raise AssertionError("registrar w/o user should raise NULL")


def test_answer_reject_errors_in_wt() -> None:
    """answer()/reject() in the default WT mode raise a negative error code.

    The library returns IAX_ERR_NOT_CONNECTED (-3) when there is no active call
    (no inbound offer pending), which is the correct result for an idle WT
    station — no mode-mismatch, just no call.
    """
    with Station() as st:
        for op in (st.answer, st.reject):
            try:
                op()
            except StationError as e:
                assert e.code < 0, f"expected a negative code, got {e.code}"
            else:
                raise AssertionError(f"{op.__name__}() should raise while idle/WT")


def test_incoming_from_empty_initially() -> None:
    """No Incoming event yet -> incoming_from() is empty."""
    with Station() as st:
        assert st.incoming_from() == "", f"got {st.incoming_from()!r}"


def test_credential_resolver_registers() -> None:
    """Registering a resolver succeeds and the station still snapshots fine.

    Offline cannot trigger the resolver (that needs network registration); we
    only assert the callback registers and the Station stays healthy.
    """
    with Station() as st:
        st.set_credential_resolver(lambda user: "be04-secret")
        snap = st.snapshot()
        assert snap.status == Status.IDLE
        assert snap.mode == Mode.WT


def test_close_is_idempotent() -> None:
    st = Station()
    st.close()
    st.close()  # no crash
    assert repr(st) == "<Station closed>"


def test_enable_disable_inbound() -> None:
    """enable_inbound(loopback:0) starts the listener; disable_inbound() stops it.

    Uses port 0 (ephemeral) so the bind never fails due to a busy port.
    The mode() is derived: Node when listening, WT when not.
    """
    nc = NodeConfig(bind="127.0.0.1:0", answer=AnswerPolicy.AUTO, auth=AuthPolicy.OFF)
    with Station() as st:
        assert st.mode() == Mode.WT, "fresh station must start WT"

        # enable_inbound: starts the always-on inbound listener; mode derives to Node.
        st.enable_inbound(nc)
        assert st.mode() == Mode.NODE, "mode() must derive Node when listener is up"

        # disable_inbound: stops it; mode derives back to WT.
        st.disable_inbound()
        assert st.mode() == Mode.WT, "mode() must derive back to WT after disable_inbound"


def test_register_deregister_no_registrar_raises_null() -> None:
    """register() with no registrar set raises IAX_ERR_NULL (required field)."""
    nc = NodeConfig(bind="127.0.0.1:0")
    with Station() as st:
        try:
            st.register(nc)
        except StationError as e:
            assert e.code == IAX_ERR_NULL, f"expected NULL, got {e.code}"
        else:
            raise AssertionError("register() with no registrar should raise NULL")


def test_deregister_idempotent() -> None:
    """deregister() when not registered is a no-op (returns ok)."""
    with Station() as st:
        st.deregister()  # idempotent no-op; must not raise


def test_m17_snapshot_fields() -> None:
    """A fresh station never reports a live M17 session; availability is a bool.

    Hermetic (iax-f2b8 Task 5): `m17_available` depends on whether this
    machine has a working codec2 backend, so only its type is asserted, not
    its value. `m17_active` is always False on a never-connected station.
    """
    with Station() as st:
        snap = st.snapshot()
        assert snap.m17_active is False, f"expected False, got {snap.m17_active}"
        assert isinstance(snap.m17_available, bool), f"got {type(snap.m17_available)}"


def test_characterize_idle_is_empty() -> None:
    """characterize() on a station that is not monitoring returns "".

    Hardware-free: no monitor is started, so no device is opened. Covers the
    default margin and an explicit one — both take the same idle path.
    """
    with Station() as st:
        assert st.characterize() == "", "idle characterize must be empty"
        assert st.characterize(peak_margin_db=24.0) == ""
        assert st.characterize(harmonic_comb=True, peak_margin_db=0.0) == ""
        assert st.characterize(threshold_dbfs=-60.0) == ""
        assert st.characterize(harmonic_comb=True, threshold_dbfs=-90.0) == ""


def main() -> int:
    tests = [
        test_struct_layout_matches_library,
        test_struct_field_order_matches_header,
        test_new_snapshot_idle_ptt_event_free,
        test_default_mode_is_wt,
        test_set_node_config_listen_only_ok,
        test_set_node_config_bad_bind_resolve,
        test_set_node_config_registrar_without_user_null,
        test_answer_reject_errors_in_wt,
        test_incoming_from_empty_initially,
        test_credential_resolver_registers,
        test_secret_free_surfaces,
        test_close_is_idempotent,
        test_enable_disable_inbound,
        test_register_deregister_no_registrar_raises_null,
        test_deregister_idempotent,
        test_m17_snapshot_fields,
        test_characterize_idle_is_empty,
    ]
    for t in tests:
        t()
        print(f"ok: {t.__name__}")
    print(f"\nall {len(tests)} smoke tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
