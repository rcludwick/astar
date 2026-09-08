# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import ctypes

import pytest

from astarserial import SerialClient, SerialConfig, Transport, _IaxSerialConfig, _bind


def _lib():
    import os

    from astarserial import _LIB_ENV, _default_lib_path

    return _bind(ctypes.CDLL(os.environ.get(_LIB_ENV) or _default_lib_path()))


def test_config_layout_matches_library():
    """`iax_serial_open` READS sizeof(IaxSerialConfig) through our pointer.

    A field missing here is an out-of-bounds read plus a one-slot shift of
    everything after it — which silently redirects `transport`, the field that
    picks the tty path that asserts RTS (the radio-key line).
    """
    lib = _lib()
    assert ctypes.sizeof(_IaxSerialConfig) == lib.iax_serial_config_size()


def test_default_transport_is_usb():
    """Raw USB is the driver-free default everywhere in astar (iax-c7e1)."""
    assert SerialConfig().transport is Transport.USB


def test_open_bogus_path_raises():
    """A bogus device path must fail to open.

    Pinned to TTY on purpose: `port_path` is a tty-backend concept, and the USB
    backend ignores it and enumerates real attached hardware instead — which a
    test must never do.
    """
    with pytest.raises(Exception):
        SerialClient(
            SerialConfig(
                port_path="/dev/iax-nonexistent-serial",
                transport=Transport.TTY,
            )
        )


def test_autodetect_is_none_or_path():
    p = SerialClient.autodetect()
    assert p is None or (isinstance(p, str) and p)
