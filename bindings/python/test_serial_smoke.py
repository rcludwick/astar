# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import pytest

from astarserial import SerialClient, SerialConfig


def test_open_bogus_path_raises():
    with pytest.raises(Exception):
        SerialClient(SerialConfig(port_path="/dev/iax-nonexistent-serial"))


def test_autodetect_is_none_or_path():
    p = SerialClient.autodetect()
    assert p is None or (isinstance(p, str) and p)
