# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""ctypes binding for astar-serial-sys (serial radio-interface client)."""
from __future__ import annotations

import ctypes
import enum
import os
import sys
from ctypes import (
    POINTER, Structure, c_bool, c_char, c_char_p, c_float, c_int, c_uint32,
)

_LIB_ENV = "ASTAR_SERIAL_LIB"


def _default_lib_path() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(os.path.dirname(here))
    if sys.platform == "darwin":
        name = "libastar_serial_sys.dylib"
    elif sys.platform == "win32":
        name = "astar_serial_sys.dll"
    else:
        name = "libastar_serial_sys.so"
    return os.path.join(root, "target", "debug", name)


class Transport(enum.IntEnum):
    """How the device is spoken to (mirrors ``IaxSerialTransport``).

    ``USB`` is the default everywhere else in astar (iax-c7e1) and needs no
    driver. ``TTY`` is opt-in: opening a USB radio interface's tty asserts RTS,
    which is the radio-key line.
    """

    TTY = 0
    USB = 1


class KeyLine(enum.IntEnum):
    CTS = 0
    DCD = 1
    DSR = 2
    RI = 3


class RadioLine(enum.IntEnum):
    RTS = 0
    DTR = 1


class RxKeyMode(enum.IntEnum):
    REMOTE_PTT = 0
    RX_ACTIVITY = 1


class _IaxSerialConfig(Structure):
    """Mirrors ``IaxSerialConfig`` in astarserial.h — FIELD-FOR-FIELD, IN ORDER.

    ``iax_serial_open`` reads ``sizeof(IaxSerialConfig)`` bytes through the
    pointer, so a missing field is an out-of-bounds read plus a one-slot shift
    of everything after it. ``_check_abi`` asserts the size at load time.
    """

    _fields_ = [
        ("port_path", c_char_p),
        ("transport", c_int),  # IaxSerialTransport; Tty=0 asserts RTS, Usb=1 does not
        ("key_line", c_int),
        ("key_active_high", c_bool),
        ("radio_line", c_int),
        ("radio_active_high", c_bool),
        ("cts_debounce_ms", c_uint32),
        ("rx_mode", c_int),
        ("rx_floor_db", c_float),
        ("rx_hang_ms", c_uint32),
    ]


_IaxSerialPtr = ctypes.c_void_p


class SerialError(Exception):
    def __init__(self, code: int, text: str) -> None:
        super().__init__(f"SerialError({code}): {text}")
        self.code = code
        self.text = text


class SerialConfig:
    """Serial PTT settings.

    ``transport`` defaults to raw USB, astar's driver-free default (iax-c7e1).
    ``port_path`` is a TTY-backend concept: the USB backend ignores it and
    enumerates internally, so pass ``transport=Transport.TTY`` alongside a path.
    """

    def __init__(
        self,
        port_path: str | None = None,
        transport: Transport = Transport.USB,
        key_line: KeyLine = KeyLine.CTS,
        key_active_high: bool = True,
        radio_line: RadioLine = RadioLine.RTS,
        radio_active_high: bool = True,
        debounce_ms: int = 30,
        rx_mode: RxKeyMode = RxKeyMode.REMOTE_PTT,
        rx_floor_db: float = -45.0,
        rx_hang_ms: int = 250,
    ) -> None:
        self.port_path = port_path
        self.transport = transport
        self.key_line = key_line
        self.key_active_high = key_active_high
        self.radio_line = radio_line
        self.radio_active_high = radio_active_high
        self.debounce_ms = debounce_ms
        self.rx_mode = rx_mode
        self.rx_floor_db = rx_floor_db
        self.rx_hang_ms = rx_hang_ms


class AbiMismatchError(RuntimeError):
    """The loaded cdylib lays out ``IaxSerialConfig`` differently than we do."""


def _check_abi(lib: ctypes.CDLL) -> None:
    want = int(lib.iax_serial_config_size())
    have = ctypes.sizeof(_IaxSerialConfig)
    if have != want:
        raise AbiMismatchError(
            f"_IaxSerialConfig is {have} bytes here but {want} bytes in the loaded "
            "astar-serial-sys cdylib — the ctypes mirror has drifted from "
            "crates/astar-serial-sys/include/astarserial.h. Regenerate with "
            "`just cbindgen` and bring _fields_ back in step, field for field."
        )


def _bind(lib: ctypes.CDLL) -> ctypes.CDLL:
    lib.iax_serial_open.argtypes = [POINTER(_IaxSerialConfig)]
    lib.iax_serial_open.restype = _IaxSerialPtr
    lib.iax_serial_close.argtypes = [_IaxSerialPtr]
    lib.iax_serial_close.restype = None
    lib.iax_serial_ptt_tick.argtypes = [
        _IaxSerialPtr, c_bool, c_float, POINTER(c_bool), POINTER(c_bool)
    ]
    lib.iax_serial_ptt_tick.restype = c_int
    lib.iax_serial_autodetect.argtypes = [POINTER(c_char), ctypes.c_size_t]
    lib.iax_serial_autodetect.restype = c_int
    lib.iax_serial_error_text.argtypes = [c_int]
    lib.iax_serial_error_text.restype = c_char_p
    lib.iax_serial_config_size.argtypes = []
    lib.iax_serial_config_size.restype = ctypes.c_size_t
    _check_abi(lib)
    return lib


class SerialClient:
    def __init__(self, config: SerialConfig, *, lib_path: str | None = None) -> None:
        path = lib_path or os.environ.get(_LIB_ENV) or _default_lib_path()
        self._lib = _bind(ctypes.CDLL(path))
        self._path_buf = (
            config.port_path.encode() if config.port_path is not None else None
        )
        c = _IaxSerialConfig(
            port_path=self._path_buf,
            transport=int(config.transport),
            key_line=int(config.key_line),
            key_active_high=config.key_active_high,
            radio_line=int(config.radio_line),
            radio_active_high=config.radio_active_high,
            cts_debounce_ms=config.debounce_ms,
            rx_mode=int(config.rx_mode),
            rx_floor_db=config.rx_floor_db,
            rx_hang_ms=config.rx_hang_ms,
        )
        self._handle = self._lib.iax_serial_open(ctypes.byref(c))
        if not self._handle:
            raise SerialError(-3, "serial open failed")

    @staticmethod
    def autodetect(*, lib_path: str | None = None) -> str | None:
        path = lib_path or os.environ.get(_LIB_ENV) or _default_lib_path()
        lib = _bind(ctypes.CDLL(path))
        buf = ctypes.create_string_buffer(256)
        rc = lib.iax_serial_autodetect(buf, len(buf))
        return buf.value.decode() if rc == 0 else None

    def ptt_tick(self, remote_keyed: bool, rx_db: float) -> tuple[bool, bool]:
        on = c_bool(False)
        changed = c_bool(False)
        rc = self._lib.iax_serial_ptt_tick(
            self._handle, remote_keyed, rx_db, ctypes.byref(on), ctypes.byref(changed)
        )
        if rc < 0:
            text = self._lib.iax_serial_error_text(rc).decode()
            raise SerialError(rc, text)
        return (changed.value, on.value)

    def close(self) -> None:
        handle, self._handle = getattr(self, "_handle", None), None
        if handle:
            self._lib.iax_serial_close(handle)

    def __enter__(self) -> "SerialClient":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()
