# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""Pure-ctypes Python binding for the ``astar-sys`` poll+snapshot C-ABI.

This module mirrors ``crates/astar-sys/include/astar.h`` exactly: the
opaque ``IaxStation*`` handle, the ``IaxConfig`` / ``IaxState`` / ``IaxEvent``
``#[repr(C)]`` structs, the ``IaxStatus`` / ``IaxEventKind`` enums, the
``IAX_*`` error-code constants, and every ``iax_station_*`` /
``iax_error_text`` function signature.

Design contract (matches the C-ABI):

* **Poll + snapshot only.** There are NO callbacks into Python. The app drives
  the call by polling :meth:`Station.snapshot` and :meth:`Station.next_event`.
* **Secret-free.** ``secret`` is only ever a :meth:`Station.connect` argument.
  It is never stored on the object, never returned from a snapshot/event, and
  never appears in any ``repr`` / exception message / log.
* **Vendor-neutral.** :meth:`Station.connect` is the generic IAX2 path;
  :meth:`Station.connect_wt` is the AllStar Web-Transceiver convenience.

Example::

    from astarstation import Station

    with Station() as st:
        snap = st.snapshot()
        print(snap.status)          # Status.IDLE
        # st.connect("55553", "55553", secret="allstar", name="py")
"""

from __future__ import annotations

import ctypes
import enum
import os
import sys
from ctypes import (
    POINTER,
    c_bool,
    c_char,
    c_char_p,
    c_float,
    c_int,
    c_size_t,
    c_uint32,
    c_uint64,
    c_void_p,
)
from dataclasses import dataclass
from typing import Callable

__all__ = [
    "Mode",
    "AnswerPolicy",
    "AuthPolicy",
    "Status",
    "EventKind",
    "NodeConfig",
    "Snapshot",
    "Event",
    "StationError",
    "Station",
    "load_library",
]


# --------------------------------------------------------------------------- #
# Error codes (mirror the IAX_* #defines in astar.h).
# --------------------------------------------------------------------------- #

IAX_OK = 0
IAX_ERR_PANIC = -1
IAX_ERR_NULL = -2
IAX_ERR_NOT_CONNECTED = -3
IAX_ERR_ALREADY_CONNECTED = -4
IAX_ERR_PORTAL = -5
IAX_ERR_RESOLVE = -6
IAX_ERR_AUDIO = -7
IAX_ERR_IAX = -8
IAX_ERR_SERIAL = -9
IAX_ERR_UTF8 = -10
IAX_ERR_UNSUPPORTED = -11
IAX_ERR_MODE_MISMATCH = -12


# --------------------------------------------------------------------------- #
# Enums (mirror IaxMode / IaxAnswerPolicy / IaxAuthPolicy / IaxStatus /
# IaxEventKind).
# --------------------------------------------------------------------------- #


class Mode(enum.IntEnum):
    """Top-level operating mode (mirrors ``IaxMode``)."""

    WT = 0  # Dial-out Web-Transceiver client (default).
    NODE = 1  # Inbound IAX2 node (accept calls, bridge to the handset).


class AnswerPolicy(enum.IntEnum):
    """How a node answers inbound calls (mirrors ``IaxAnswerPolicy``)."""

    AUTO = 0  # Auto-accept the inbound offer and bridge to the handset.
    MANUAL = 1  # Surface the offer as an Incoming event; answer()/reject().


class AuthPolicy(enum.IntEnum):
    """Inbound-authentication policy (mirrors ``IaxAuthPolicy``)."""

    REQUIRED = 0  # Every inbound NEW must authenticate.
    OPTIONAL = 1  # Challenge only when the username maps to a credential.
    OFF = 2  # Never challenge (accept anonymous). Dev dial-in only.


class Status(enum.IntEnum):
    """Call lifecycle status (mirrors ``IaxStatus``)."""

    IDLE = 0
    DIALING = 1
    ANSWERED = 2
    HANGUP = 3


class EventKind(enum.IntEnum):
    """Kind of a drained lifecycle event (mirrors ``IaxEventKind``)."""

    NONE = 0
    ANSWERED = 1
    REMOTE_PTT = 2
    HANGUP = 3
    MODE_CHANGED = 4  # Wt <-> Node; read the new mode from mode()/snapshot().
    INCOMING = 5  # Inbound call ringing (Node/Manual); see incoming_from().
    REGISTERED = 6  # Outbound node registration succeeded. Secret-free.
    REGISTER_FAILED = 7  # Outbound node registration failed. Secret-free.


# --------------------------------------------------------------------------- #
# Public NodeConfig dataclass — secret-free node-mode configuration.
# --------------------------------------------------------------------------- #


@dataclass
class NodeConfig:
    """Node-mode configuration for :meth:`Station.enable_inbound` and
    :meth:`Station.register`.

    Secret-free by construction: there is NO password field. The registrar
    password is supplied only at runtime through the callback registered via
    :meth:`Station.set_credential_resolver`.
    """

    bind: str | None = None
    """Listener bind address ``"host:port"``, or ``None`` for ``"0.0.0.0:4569"``."""
    answer: AnswerPolicy = AnswerPolicy.AUTO
    """Auto vs manual answer."""
    auth: AuthPolicy = AuthPolicy.OFF
    """Inbound auth policy."""
    registrar: str | None = None
    """Upstream registrar ``"host:port"`` to register AS a node, or ``None`` to
    only listen (no registration)."""
    register_user: str | None = None
    """The node id to register AS (e.g. ``"77777"``), required when ``registrar``
    is set, otherwise ignored."""
    refresh_secs: int = 0
    """Requested registration refresh interval in seconds, or ``0`` for ``60``."""


# --------------------------------------------------------------------------- #
# ctypes Structures (mirror the #[repr(C)] structs, field order + types exact).
# --------------------------------------------------------------------------- #


class _IaxConfig(ctypes.Structure):
    # All borrowed const char*; NULL means "unset".
    _fields_ = [
        ("input", c_char_p),
        ("output", c_char_p),
        ("portal_user", c_char_p),
        ("portal_pass", c_char_p),
        ("portal_node", c_char_p),
        ("secret", c_char_p),
        ("codec_policy", c_char_p),  # config string; NULL/""/"default" = library default
    ]


class _IaxNodeConfig(ctypes.Structure):
    """Mirrors ``IaxNodeConfig``. Secret-free by construction (no password)."""

    _fields_ = [
        ("bind", c_char_p),
        ("answer", c_int),  # IaxAnswerPolicy
        ("auth", c_int),  # IaxAuthPolicy
        ("registrar", c_char_p),
        ("register_user", c_char_p),
        ("refresh_secs", c_uint32),
    ]


class _IaxState(ctypes.Structure):
    _fields_ = [
        ("status", c_int),  # IaxStatus
        ("ptt", c_bool),
        ("remote_ptt", c_bool),
        ("tx_db", c_float),
        ("rx_db", c_float),
        ("input_db", c_float),  # continuous mic input level (iax-5c30)
        ("rtt_ms", c_int),  # -1 == unknown
        ("mode", c_int),  # IaxMode
        ("tx_reanchors", c_uint64),  # cumulative TX ts-ladder re-anchors (iax-9e55)
        ("tx_capture_overruns", c_uint64),  # cumulative cpal capture overruns
        ("negotiated_format", c_uint32),  # IAX2 format bit; 0 = none (iax-3e53)
        ("dtmf_played", c_uint32),  # send_dtmf_string progress; 0 = none (iax-4b7a)
        ("dtmf_total", c_uint32),  # send_dtmf_string total; 0 = none (iax-4b7a)
        ("m17_available", c_bool),  # M17 feature + a working codec2 backend (iax-f2b8 Task 5)
        ("m17_active", c_bool),  # an M17 session is live (iax-f2b8 Task 5)
    ]


class _IaxEvent(ctypes.Structure):
    _fields_ = [
        ("kind", c_int),  # IaxEventKind
        ("remote_ptt", c_bool),
    ]


# IaxCredentialResolver: int (*)(const char* user, char* out, uintptr_t cap,
#                                void* user_data)
_IaxCredentialResolver = ctypes.CFUNCTYPE(
    c_int,  # return: IAX_OK (0) or non-zero "no secret"
    c_char_p,  # user
    POINTER(c_char),  # out buffer
    c_size_t,  # cap
    c_void_p,  # user_data
)


class _IaxStation(ctypes.Structure):
    """Opaque; never instantiated on the Python side."""


_IaxStationPtr = POINTER(_IaxStation)


# --------------------------------------------------------------------------- #
# Public dataclasses returned by snapshot() / next_event().
#
# NOTE: deliberately NO secret/password/portal field anywhere here. These are
# safe to print/log.
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class Snapshot:
    """Latest call state from :meth:`Station.snapshot`."""

    status: Status
    ptt: bool
    remote_ptt: bool
    tx_db: float
    rx_db: float
    input_db: float  # continuous mic input level (dBFS); drive VOX from this
    rtt_ms: int | None  # None when unknown (C returns -1)
    mode: Mode  # current top-level operating mode (WT dial-out vs Node)
    tx_reanchors: int  # cumulative voice-ts-ladder re-anchors; growth = choppy TX
    tx_capture_overruns: int  # cumulative cpal capture overruns on the routed mic
    negotiated_format: int  # negotiated codec as its IAX2 format bit; 0 = none
    # (4 = ulaw, 8 = alaw, 64 = slin, 32768 = slin16 wideband; iax-3e53)
    dtmf_played: int  # digits sent of the active send_dtmf_string sequence; 0 = none
    dtmf_total: int  # total digits of the active sequence; 0 = none (iax-4b7a)
    m17_available: bool  # M17 feature compiled in AND a working codec2 backend found
    m17_active: bool  # an M17 session is currently live (iax-f2b8 Task 5)


@dataclass(frozen=True)
class Event:
    """A drained lifecycle event from :meth:`Station.next_event`."""

    kind: EventKind
    remote_ptt: bool


# --------------------------------------------------------------------------- #
# Exception. Carries the integer code + the library's generic error text.
# Secret-free by construction: iax_error_text returns 'static generic strings.
# --------------------------------------------------------------------------- #


class StationError(Exception):
    """Raised when an ``iax_station_*`` call returns a negative code.

    Carries the integer ``code`` and the library's generic ``iax_error_text``.
    Never carries a secret/password (the underlying C strings are generic).
    """

    def __init__(self, code: int, text: str) -> None:
        self.code = code
        self.text = text
        super().__init__(f"astarstation error {code}: {text}")


# --------------------------------------------------------------------------- #
# cdylib location + binding setup.
# --------------------------------------------------------------------------- #


def _lib_filename() -> str:
    if sys.platform == "darwin":
        return "libastar_sys.dylib"
    if sys.platform == "win32":
        return "astar_sys.dll"
    return "libastar_sys.so"


def _candidate_paths() -> list[str]:
    """Ordered candidate paths for the cdylib.

    1. ``ASTAR_LIB`` env override (a full path to the dylib/so, or a
       directory containing it).
    2. ``target/debug`` then ``target/release`` relative to the repo root
       (this file lives at ``<repo>/bindings/python/astarstation.py``).
    """
    name = _lib_filename()
    candidates: list[str] = []

    override = os.environ.get("ASTAR_LIB")
    if override:
        if os.path.isdir(override):
            candidates.append(os.path.join(override, name))
        else:
            candidates.append(override)

    here = os.path.dirname(os.path.abspath(__file__))
    repo_root = os.path.abspath(os.path.join(here, "..", ".."))
    for profile in ("debug", "release"):
        candidates.append(os.path.join(repo_root, "target", profile, name))

    return candidates


def load_library() -> ctypes.CDLL:
    """Locate and load the ``astar-sys`` cdylib, with argtypes/restype set.

    Search order: ``$ASTAR_LIB`` (file or dir), then ``target/debug`` and
    ``target/release`` relative to the repo. Raises ``FileNotFoundError`` with
    the searched paths if none is found.
    """
    tried: list[str] = []
    for path in _candidate_paths():
        tried.append(path)
        if os.path.isfile(path):
            lib = ctypes.CDLL(path)
            _bind(lib)
            return lib

    raise FileNotFoundError(
        "could not locate the astar-sys cdylib ("
        + _lib_filename()
        + "). Build it with "
        "`cargo build -p astar-sys` (or --release), or set "
        "ASTAR_LIB to the dylib path. Searched:\n  "
        + "\n  ".join(tried)
    )


def _bind(lib: ctypes.CDLL) -> None:
    """Set argtypes/restype for every function (matches astar.h)."""
    lib.iax_station_new.argtypes = [POINTER(_IaxConfig)]
    lib.iax_station_new.restype = _IaxStationPtr

    lib.iax_station_free.argtypes = [_IaxStationPtr]
    lib.iax_station_free.restype = None

    lib.iax_station_connect.argtypes = [
        _IaxStationPtr,
        c_char_p,  # dest
        c_char_p,  # calling
        c_char_p,  # secret (may be NULL)
        c_char_p,  # name
    ]
    lib.iax_station_connect.restype = c_int

    lib.iax_station_connect_wt.argtypes = [_IaxStationPtr, c_char_p]
    lib.iax_station_connect_wt.restype = c_int

    lib.iax_station_disconnect.argtypes = [_IaxStationPtr]
    lib.iax_station_disconnect.restype = c_int

    lib.iax_station_set_mode.argtypes = [_IaxStationPtr, c_int]
    lib.iax_station_set_mode.restype = c_int

    lib.iax_station_mode.argtypes = [_IaxStationPtr, POINTER(c_int)]
    lib.iax_station_mode.restype = c_int

    lib.iax_station_set_node_config.argtypes = [
        _IaxStationPtr,
        POINTER(_IaxNodeConfig),
    ]
    lib.iax_station_set_node_config.restype = c_int

    lib.iax_station_enable_inbound.argtypes = [
        _IaxStationPtr,
        POINTER(_IaxNodeConfig),
    ]
    lib.iax_station_enable_inbound.restype = c_int

    lib.iax_station_disable_inbound.argtypes = [_IaxStationPtr]
    lib.iax_station_disable_inbound.restype = c_int

    lib.iax_station_register.argtypes = [
        _IaxStationPtr,
        POINTER(_IaxNodeConfig),
    ]
    lib.iax_station_register.restype = c_int

    lib.iax_station_deregister.argtypes = [_IaxStationPtr]
    lib.iax_station_deregister.restype = c_int

    lib.iax_station_set_credential_resolver.argtypes = [
        _IaxStationPtr,
        _IaxCredentialResolver,
        c_void_p,
    ]
    lib.iax_station_set_credential_resolver.restype = c_int

    lib.iax_station_answer.argtypes = [_IaxStationPtr]
    lib.iax_station_answer.restype = c_int

    lib.iax_station_reject.argtypes = [_IaxStationPtr]
    lib.iax_station_reject.restype = c_int

    lib.iax_station_incoming_from.argtypes = [_IaxStationPtr, c_char_p, c_size_t]
    lib.iax_station_incoming_from.restype = c_int

    lib.iax_station_set_ptt.argtypes = [_IaxStationPtr, c_bool]
    lib.iax_station_set_ptt.restype = c_int

    lib.iax_station_set_input_gain.argtypes = [_IaxStationPtr, c_float]
    lib.iax_station_set_input_gain.restype = c_int

    lib.iax_station_set_output_gain.argtypes = [_IaxStationPtr, c_float]
    lib.iax_station_set_output_gain.restype = c_int

    lib.iax_station_set_compression.argtypes = [_IaxStationPtr, c_bool]
    lib.iax_station_set_compression.restype = c_int

    lib.iax_station_set_compression_level.argtypes = [_IaxStationPtr, c_float]
    lib.iax_station_set_compression_level.restype = c_int

    lib.iax_station_set_noise_reduction.argtypes = [_IaxStationPtr, c_bool]
    lib.iax_station_set_noise_reduction.restype = c_int

    lib.iax_station_snapshot.argtypes = [_IaxStationPtr, POINTER(_IaxState)]
    lib.iax_station_snapshot.restype = c_int

    lib.iax_station_next_event.argtypes = [_IaxStationPtr, POINTER(_IaxEvent)]
    lib.iax_station_next_event.restype = c_int

    lib.iax_station_list_inputs.argtypes = [_IaxStationPtr, c_char_p, c_size_t]
    lib.iax_station_list_inputs.restype = c_int

    lib.iax_station_list_outputs.argtypes = [_IaxStationPtr, c_char_p, c_size_t]
    lib.iax_station_list_outputs.restype = c_int

    lib.iax_station_set_devices.argtypes = [_IaxStationPtr, c_char_p, c_char_p]
    lib.iax_station_set_devices.restype = c_int

    lib.iax_error_text.argtypes = [c_int]
    lib.iax_error_text.restype = c_char_p


def _encode(value: str | None) -> bytes | None:
    """Encode an optional Python str to bytes for a ``const char*`` arg.

    ``None`` becomes a NULL pointer (ctypes maps ``None`` c_char_p -> NULL).
    """
    return None if value is None else value.encode("utf-8")


# --------------------------------------------------------------------------- #
# Idiomatic Station wrapper.
# --------------------------------------------------------------------------- #


class Station:
    """An IAX2 station over the C-ABI. Context-manager friendly.

    The handle is freed on :meth:`close` / ``__exit__`` / GC. All methods raise
    :class:`StationError` on a negative return code (except those documented to
    return a value).
    """

    def __init__(
        self,
        *,
        input: str | None = None,
        output: str | None = None,
        portal_user: str | None = None,
        portal_pass: str | None = None,
        portal_node: str | None = None,
        secret: str | None = None,
        codec_policy: str | None = None,
        lib: ctypes.CDLL | None = None,
    ) -> None:
        """Create a station.

        ``portal_*`` enable the WT path only when all three are set.
        ``portal_pass`` and ``secret`` are consumed into the station and never
        retained on this object.

        ``codec_policy`` selects the outbound codec negotiation policy
        (iax-3e53): ``"ulaw_only"`` (the library default), ``"allow_slin"``,
        ``"prefer_slin"``, or ``"prefer_slin16"`` (16 kHz wideband);
        ``None``/``""``/``"default"`` = the library default. An unknown string
        fails construction. Construction-time only — the audio pipeline rate
        is pinned when the station engine is built (no runtime setter).
        """
        self._lib = lib if lib is not None else load_library()
        cfg = _IaxConfig(
            input=_encode(input),
            output=_encode(output),
            portal_user=_encode(portal_user),
            portal_pass=_encode(portal_pass),
            portal_node=_encode(portal_node),
            secret=_encode(secret),
            codec_policy=_encode(codec_policy),
        )
        # Do NOT keep cfg or any secret around after this call.
        self._handle = self._lib.iax_station_new(ctypes.byref(cfg))
        if not self._handle:
            raise StationError(IAX_ERR_PANIC, "iax_station_new returned NULL")

    # -- error helper ----------------------------------------------------- #

    def _check(self, code: int) -> int:
        if code < 0:
            raw = self._lib.iax_error_text(code)
            text = raw.decode("utf-8", "replace") if raw else "unknown error"
            raise StationError(code, text)
        return code

    def _require_handle(self) -> None:
        if not self._handle:
            raise StationError(IAX_ERR_NULL, "station is closed")

    # -- lifecycle -------------------------------------------------------- #

    def connect(
        self,
        dest: str,
        calling: str,
        *,
        secret: str | None = None,
        name: str = "astar",
    ) -> None:
        """Manual / non-WT connect (the vendor-neutral generic IAX2 path).

        ``secret`` is a call-time argument; it is passed straight to the C-ABI
        and never stored on this object. ``None`` uses the configured guest
        secret.
        """
        self._require_handle()
        self._check(
            self._lib.iax_station_connect(
                self._handle,
                _encode(dest),
                _encode(calling),
                _encode(secret),
                _encode(name),
            )
        )

    def connect_wt(self, dest_node: str) -> None:
        """Web-Transceiver connect (the AllStar convenience path).

        Requires the three ``portal_*`` fields to have been set at construction;
        otherwise raises :class:`StationError` with ``IAX_ERR_PORTAL``.
        """
        self._require_handle()
        self._check(self._lib.iax_station_connect_wt(self._handle, _encode(dest_node)))

    def disconnect(self) -> None:
        """Tear down any active call. Idempotent (no-op while idle)."""
        self._require_handle()
        self._check(self._lib.iax_station_disconnect(self._handle))

    # -- mode + node ------------------------------------------------------ #

    def set_mode(self, mode: Mode) -> None:
        """Switch the operating mode (WT dial-out <-> inbound Node).

        Entering Node mode starts the listener (and fires registration if a
        registrar was configured via :meth:`set_node_config` and a resolver is
        set); leaving it tears the node down and deregisters.

        NOTE: this performs BLOCKING work (device + socket setup, registration);
        call it off any UI thread. Raises :class:`StationError`
        (``IAX_ERR_UNSUPPORTED`` / ``IAX_ERR_NOT_CONNECTED``) for invalid
        switches.
        """
        self._require_handle()
        self._check(self._lib.iax_station_set_mode(self._handle, int(Mode(mode))))

    def mode(self) -> Mode:
        """Return the current operating mode (reads via ``iax_station_mode``)."""
        self._require_handle()
        out = c_int()
        self._check(self._lib.iax_station_mode(self._handle, ctypes.byref(out)))
        return Mode(out.value)

    def set_node_config(
        self,
        *,
        bind: str | None = None,
        answer: AnswerPolicy = AnswerPolicy.AUTO,
        auth: AuthPolicy = AuthPolicy.OFF,
        registrar: str | None = None,
        register_user: str | None = None,
        refresh_secs: int = 0,
    ) -> None:
        """Configure Node mode (takes effect on the next switch to Node mode).

        Secret-free by construction: there is NO password field here. The
        registrar password is supplied only at runtime through the callback
        registered via :meth:`set_credential_resolver`.

        ``bind`` is the listener address ``"host:port"`` (``None`` ->
        ``"0.0.0.0:4569"``). ``registrar`` is the upstream ``"host:port"`` to
        register AS a node (``None`` -> listen only, no registration);
        ``register_user`` is the node id to register AS and is REQUIRED when
        ``registrar`` is set. ``refresh_secs`` is the refresh interval (``0`` ->
        ``60``).

        Raises :class:`StationError` (``IAX_ERR_NULL`` when ``registrar`` is set
        without ``register_user``; ``IAX_ERR_RESOLVE`` for an unparseable
        ``bind``/``registrar`` address; ``IAX_ERR_UTF8``).
        """
        self._require_handle()
        # Keep the encoded strings alive across the C call.
        b_bind = _encode(bind)
        b_registrar = _encode(registrar)
        b_register_user = _encode(register_user)
        cfg = _IaxNodeConfig(
            bind=b_bind,
            answer=int(AnswerPolicy(answer)),
            auth=int(AuthPolicy(auth)),
            registrar=b_registrar,
            register_user=b_register_user,
            refresh_secs=c_uint32(int(refresh_secs)),
        )
        self._check(
            self._lib.iax_station_set_node_config(self._handle, ctypes.byref(cfg))
        )

    def enable_inbound(self, config: NodeConfig) -> None:
        """Start the inbound IAX2 listener with the given node configuration.

        Binds the UDP listener to ``config.bind`` (``None`` -> ``"0.0.0.0:4569"``),
        configures the auth/answer policy from ``config``. The station's
        operating mode is **not** changed — the listener runs independently of
        the WT/Node mode flag. Call this to bring up the always-on inbound path
        without switching modes.

        NOTE: this binds a UDP socket; call it off any UI thread. Raises
        :class:`StationError` (``IAX_ERR_NULL`` when ``st``/``cfg`` is NULL;
        ``IAX_ERR_RESOLVE`` for an unparseable ``bind``; ``IAX_ERR_LISTEN``
        if the bind fails; ``IAX_ERR_UTF8``).
        """
        self._require_handle()
        b_bind = _encode(config.bind)
        b_registrar = _encode(config.registrar)
        b_register_user = _encode(config.register_user)
        cfg = _IaxNodeConfig(
            bind=b_bind,
            answer=int(AnswerPolicy(config.answer)),
            auth=int(AuthPolicy(config.auth)),
            registrar=b_registrar,
            register_user=b_register_user,
            refresh_secs=c_uint32(int(config.refresh_secs)),
        )
        self._check(
            self._lib.iax_station_enable_inbound(self._handle, ctypes.byref(cfg))
        )

    def disable_inbound(self) -> None:
        """Stop the inbound listener (or the Node-mode listener). Idempotent.

        The station's operating mode is **not** changed. Raises
        :class:`StationError` (``IAX_ERR_NULL`` on NULL ``st``).
        """
        self._require_handle()
        self._check(self._lib.iax_station_disable_inbound(self._handle))

    def register(self, config: NodeConfig) -> None:
        """Start outbound node registration using ``config.registrar`` /
        ``config.register_user`` / ``config.refresh_secs``.

        The registrar credential is resolved on demand via the callback
        registered via :meth:`set_credential_resolver` — no credential is
        passed directly here.

        NOTE: opens a UDP socket; call it off any UI thread. Raises
        :class:`StationError` (``IAX_ERR_NULL`` when ``registrar``/
        ``register_user`` not set; ``IAX_ERR_RESOLVE`` for an unparseable
        ``registrar``; ``IAX_ERR_UTF8``).
        """
        self._require_handle()
        b_bind = _encode(config.bind)
        b_registrar = _encode(config.registrar)
        b_register_user = _encode(config.register_user)
        cfg = _IaxNodeConfig(
            bind=b_bind,
            answer=int(AnswerPolicy(config.answer)),
            auth=int(AuthPolicy(config.auth)),
            registrar=b_registrar,
            register_user=b_register_user,
            refresh_secs=c_uint32(int(config.refresh_secs)),
        )
        self._check(
            self._lib.iax_station_register(self._handle, ctypes.byref(cfg))
        )

    def deregister(self) -> None:
        """Stop outbound node registration. Sends REGREL and joins the
        registration thread. Idempotent — a no-op when not currently registered.

        Raises :class:`StationError` (``IAX_ERR_NULL`` on NULL ``st``).
        """
        self._require_handle()
        self._check(self._lib.iax_station_deregister(self._handle))

    def set_credential_resolver(self, fn: Callable[[str], str]) -> None:
        """Register the credential resolver — the ONLY secret channel.

        ``fn`` receives a username (``str``) and must return its secret
        (``str``). The library invokes it at runtime when it needs a password
        (e.g. to answer a registrar's MD5 challenge). The callback may be
        invoked from a background thread.

        Secrets never appear in any config struct, snapshot, event, repr, or
        log; this resolver is the sole place a secret is handled.
        """
        self._require_handle()

        def _trampoline(user, out, cap, _user_data):  # type: ignore[no-untyped-def]
            try:
                name = user.decode("utf-8", "replace") if user else ""
                secret = fn(name)
                raw = secret.encode("utf-8")
                if cap <= 0:
                    return IAX_OK
                n = min(len(raw), cap - 1)
                for i in range(n):
                    out[i] = raw[i : i + 1]
                out[n] = b"\x00"
                return IAX_OK
            except Exception:
                return -1

        cb = _IaxCredentialResolver(_trampoline)
        # CRITICAL: keep the CFUNCTYPE instance alive while the C side holds it,
        # otherwise it is garbage-collected and the next call crashes.
        self._resolver_cb = cb
        self._check(
            self._lib.iax_station_set_credential_resolver(self._handle, cb, None)
        )

    def answer(self) -> None:
        """Answer the pending inbound offer (Node/Manual).

        Raises :class:`StationError` (``IAX_ERR_NOT_CONNECTED``) when not in
        Node mode or no offer is pending.
        """
        self._require_handle()
        self._check(self._lib.iax_station_answer(self._handle))

    def reject(self) -> None:
        """Reject the pending inbound offer (Node/Manual).

        Raises :class:`StationError` (``IAX_ERR_NOT_CONNECTED``) when not in
        Node mode or no offer is pending.
        """
        self._require_handle()
        self._check(self._lib.iax_station_reject(self._handle))

    def incoming_from(self) -> str:
        """Caller id of the most recent Incoming event (``""`` if none).

        The id is a node identifier — secret-free.
        """
        self._require_handle()
        needed = self._lib.iax_station_incoming_from(self._handle, None, 0)
        if needed < 0:
            self._check(needed)
        if needed == 0:
            return ""
        buf = ctypes.create_string_buffer(needed + 1)
        rc = self._lib.iax_station_incoming_from(self._handle, buf, len(buf))
        if rc < 0:
            self._check(rc)
        return buf.value.decode("utf-8", "replace")

    # -- transmit + gains ------------------------------------------------- #

    def set_ptt(self, on: bool) -> None:
        """Set local transmit (PTT). Raises ``NOT_CONNECTED`` while idle."""
        self._require_handle()
        self._check(self._lib.iax_station_set_ptt(self._handle, bool(on)))

    def set_input_gain(self, gain: float) -> None:
        """Set the input (TX/mic) gain multiplier (clamped ``[0.0, 2.0]``)."""
        self._require_handle()
        self._check(self._lib.iax_station_set_input_gain(self._handle, float(gain)))

    def set_output_gain(self, gain: float) -> None:
        """Set the output (RX/speaker) gain multiplier (clamped ``[0.0, 4.0]``:
        100%-400% headroom for boosting a quiet station, iax-a4e7)."""
        self._require_handle()
        self._check(self._lib.iax_station_set_output_gain(self._handle, float(gain)))

    def set_compression(self, on: bool) -> None:
        """Toggle mic voice compression on the live/next call (capture lane)."""
        self._require_handle()
        self._check(self._lib.iax_station_set_compression(self._handle, bool(on)))

    def set_compression_level(self, level: float) -> None:
        """Set mic compression strength (0.0..1.0, clamped; 0.90 default)."""
        self._require_handle()
        self._check(
            self._lib.iax_station_set_compression_level(self._handle, float(level))
        )

    def set_noise_reduction(self, on: bool) -> None:
        """Toggle mic noise reduction (denoise) on the live/next call (capture lane)."""
        self._require_handle()
        self._check(self._lib.iax_station_set_noise_reduction(self._handle, bool(on)))

    # -- poll ------------------------------------------------------------- #

    def snapshot(self) -> Snapshot:
        """Latest call state. Cheap; poll it."""
        self._require_handle()
        out = _IaxState()
        self._check(self._lib.iax_station_snapshot(self._handle, ctypes.byref(out)))
        return Snapshot(
            status=Status(out.status),
            ptt=bool(out.ptt),
            remote_ptt=bool(out.remote_ptt),
            tx_db=float(out.tx_db),
            rx_db=float(out.rx_db),
            input_db=float(out.input_db),
            rtt_ms=None if out.rtt_ms < 0 else int(out.rtt_ms),
            mode=Mode(out.mode),
            tx_reanchors=int(out.tx_reanchors),
            tx_capture_overruns=int(out.tx_capture_overruns),
            negotiated_format=int(out.negotiated_format),
            dtmf_played=int(out.dtmf_played),
            dtmf_total=int(out.dtmf_total),
            m17_available=bool(out.m17_available),
            m17_active=bool(out.m17_active),
        )

    def next_event(self) -> Event | None:
        """Drain one queued lifecycle event, or ``None`` if none is queued."""
        self._require_handle()
        out = _IaxEvent()
        self._check(self._lib.iax_station_next_event(self._handle, ctypes.byref(out)))
        if out.kind == EventKind.NONE:
            return None
        return Event(kind=EventKind(out.kind), remote_ptt=bool(out.remote_ptt))

    # -- devices ---------------------------------------------------------- #

    def _list(self, fn) -> list[str]:
        self._require_handle()
        # Query the required size (len == 0), then fill.
        needed = fn(self._handle, None, 0)
        if needed < 0:
            self._check(needed)
        if needed == 0:
            return []
        buf = ctypes.create_string_buffer(needed + 1)
        rc = fn(self._handle, buf, len(buf))
        if rc < 0:
            self._check(rc)
        text = buf.value.decode("utf-8", "replace")
        return [line for line in text.split("\n") if line]

    def list_inputs(self) -> list[str]:
        """Enumerate input (capture) device names."""
        return self._list(self._lib.iax_station_list_inputs)

    def list_outputs(self) -> list[str]:
        """Enumerate output (playback) device names."""
        return self._list(self._lib.iax_station_list_outputs)

    def set_devices(self, input: str | None, output: str | None) -> None:
        """Set the capture/playback devices applied to the next connect.

        ``None`` selects the system default for that direction.
        """
        self._require_handle()
        self._check(
            self._lib.iax_station_set_devices(
                self._handle, _encode(input), _encode(output)
            )
        )

    # -- teardown --------------------------------------------------------- #

    def close(self) -> None:
        """Free the underlying handle (idempotent). Tears down any call."""
        handle, self._handle = getattr(self, "_handle", None), None
        if handle:
            self._lib.iax_station_free(handle)

    def __enter__(self) -> "Station":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __del__(self) -> None:
        # Best-effort; interpreter shutdown may have torn down ctypes already.
        try:
            self.close()
        except Exception:
            pass

    def __repr__(self) -> str:
        # Secret-free: never reflects config/secret. Only liveness.
        state = "open" if getattr(self, "_handle", None) else "closed"
        return f"<Station {state}>"


# --------------------------------------------------------------------------- #
# Offline self-check (no network): python3 astarstation.py
# --------------------------------------------------------------------------- #


def _self_check() -> None:
    """new -> snapshot(Idle) -> set_ptt(NOT_CONNECTED) -> next_event(None) -> free.

    Plus a secret-free guard over repr / snapshot / event.
    """
    with Station() as st:
        snap = st.snapshot()
        assert snap.status == Status.IDLE, f"expected Idle, got {snap.status}"
        assert snap.rtt_ms is None, f"expected unknown rtt, got {snap.rtt_ms}"
        # TX health counters start at zero on a fresh station (iax-9e55).
        assert snap.tx_reanchors == 0, f"expected 0 reanchors, got {snap.tx_reanchors}"
        assert (
            snap.tx_capture_overruns == 0
        ), f"expected 0 overruns, got {snap.tx_capture_overruns}"
        print(f"snapshot: {snap}")

        try:
            st.set_ptt(True)
        except StationError as e:
            assert e.code == IAX_ERR_NOT_CONNECTED, f"expected NOT_CONNECTED, got {e.code}"
            print(f"set_ptt(True) while idle -> StationError({e.code}: {e.text})")
        else:  # pragma: no cover
            raise AssertionError("set_ptt(True) should have raised while idle")

        ev = st.next_event()
        assert ev is None, f"expected no event, got {ev}"
        print("next_event() -> None")

        # -- mode + node surface (offline; do NOT switch to Node mode) ----- #
        assert st.mode() == Mode.WT, f"expected WT, got {st.mode()}"
        assert snap.mode == Mode.WT, f"expected snapshot WT, got {snap.mode}"
        print(f"mode() -> {st.mode().name}; snapshot.mode -> {snap.mode.name}")

        # listen-only node config (no registrar) -> ok, does NOT switch mode.
        st.set_node_config(
            bind="127.0.0.1:0",
            answer=AnswerPolicy.MANUAL,
            auth=AuthPolicy.OFF,
        )
        assert st.mode() == Mode.WT, "set_node_config must not switch mode"
        print("set_node_config(listen-only) -> ok; still WT")

        # unparseable bind -> IAX_ERR_RESOLVE.
        try:
            st.set_node_config(bind="not-an-address")
        except StationError as e:
            assert e.code == IAX_ERR_RESOLVE, f"expected RESOLVE, got {e.code}"
            print(f"set_node_config(bad bind) -> StationError({e.code}: {e.text})")
        else:  # pragma: no cover
            raise AssertionError("set_node_config(bad bind) should have raised")

        # registrar without register_user -> IAX_ERR_NULL.
        try:
            st.set_node_config(registrar="127.0.0.1:4569", register_user=None)
        except StationError as e:
            assert e.code == IAX_ERR_NULL, f"expected NULL, got {e.code}"
            print(f"set_node_config(no user) -> StationError({e.code}: {e.text})")
        else:  # pragma: no cover
            raise AssertionError("set_node_config(no register_user) should have raised")

        # answer()/reject() in WT mode -> IAX_ERR_NOT_CONNECTED.
        for name, op in (("answer", st.answer), ("reject", st.reject)):
            try:
                op()
            except StationError as e:
                assert e.code == IAX_ERR_NOT_CONNECTED, f"expected NOT_CONNECTED, got {e.code}"
                print(f"{name}() in WT mode -> StationError({e.code}: {e.text})")
            else:  # pragma: no cover
                raise AssertionError(f"{name}() should have raised in WT mode")

        # no Incoming event yet.
        assert st.incoming_from() == "", f"expected empty, got {st.incoming_from()!r}"
        print("incoming_from() -> ''")

        # resolver registration succeeds; offline cannot trigger it.
        st.set_credential_resolver(lambda user: "be04-secret")
        post = st.snapshot()
        assert post.status == Status.IDLE, f"expected Idle, got {post.status}"
        print("set_credential_resolver() -> ok; snapshot still fine")

        # Secret-free guard: nothing the app can read mentions a secret.
        secret = "topsecret-please-do-not-leak"
        # connect() would need the network; we only assert the secret never
        # surfaces in any readable surface of an idle station.
        haystacks = [repr(st), str(snap)]
        ev2 = st.next_event()
        if ev2 is not None:  # pragma: no cover
            haystacks.append(str(ev2))
        for h in haystacks:
            assert secret not in h  # trivially true; documents the invariant
            assert "secret" not in h.lower(), f"'secret' leaked in: {h!r}"
            assert "password" not in h.lower(), f"'password' leaked in: {h!r}"
        print("secret-free guard: ok (repr/snapshot/event carry no secret)")

    print("self-check ok")


if __name__ == "__main__":
    _self_check()
