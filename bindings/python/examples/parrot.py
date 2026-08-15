#!/usr/bin/env python3
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""parrot.py — Python consumer of the astar-sys poll+snapshot C-ABI.

Mirrors crates/astar-sys/examples/parrot.c: new -> connect (the generic,
vendor-neutral path) -> poll snapshot/next_event in a loop -> disconnect ->
free. No callbacks.

Secret-free by construction: the guest secret is a connect() IN-param
("allstar" here) and is never read back out of any snapshot/event/object.

Usage::

    python3 examples/parrot.py            # dials the AllStar parrot 55553
    python3 examples/parrot.py --dry-run  # offline smoke, no network

The --dry-run path needs no network: new -> idle snapshot ->
set_ptt(NOT_CONNECTED) -> next_event(None) -> free, with a secret-free guard.
"""

import os
import sys
import time

# Allow `python3 examples/parrot.py` from anywhere: import the sibling module.
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..")))

from astarstation import (  # noqa: E402
    IAX_ERR_NOT_CONNECTED,
    Station,
    StationError,
    Status,
)


def dry_run() -> int:
    """Offline smoke: prove the poll surface + not-connected guard. No network."""
    secret = "allstar"  # in-param only; must never surface anywhere readable.
    with Station() as st:
        snap = st.snapshot()
        if snap.status != Status.IDLE:
            print(f"expected idle snapshot, got {snap.status}", file=sys.stderr)
            return 1
        print(f"snapshot: status={snap.status.name} tx={snap.tx_db}dB rx={snap.rx_db}dB")

        try:
            st.set_ptt(True)
        except StationError as e:
            if e.code != IAX_ERR_NOT_CONNECTED:
                print(f"expected NOT_CONNECTED, got {e.code} ({e.text})", file=sys.stderr)
                return 1
            print(f"set_ptt(True) while idle -> {e}")
        else:
            print("set_ptt(True) should have raised while idle", file=sys.stderr)
            return 1

        if st.next_event() is not None:
            print("expected no event while idle", file=sys.stderr)
            return 1
        print("next_event() -> None")

        # Secret-free guard: the in-param secret must not appear anywhere the
        # app can read.
        for surface in (repr(st), str(snap)):
            if secret in surface or "secret" in surface.lower():
                print(f"secret leaked in: {surface!r}", file=sys.stderr)
                return 1
        print("secret-free guard: ok")

    print("dry run ok")
    return 0


def live(node: str = "55553") -> int:
    """Dial the parrot and poll for ~8s. Requires network + audio hardware."""
    with Station() as st:
        # Generic IAX2 connect (vendor-neutral). secret is an in-param.
        try:
            st.connect(node, node, secret="allstar", name="astar")
        except StationError as e:
            print(f"connect failed: {e}", file=sys.stderr)
            return 1
        print("connected; polling ~8s...")

        for _ in range(80):
            snap = st.snapshot()
            print(
                f"status={snap.status.name:<8} tx={snap.tx_db:.1f}dB "
                f"rx={snap.rx_db:.1f}dB ptt={int(snap.ptt)} "
                f"rtt={snap.rtt_ms if snap.rtt_ms is not None else -1}ms"
            )
            while (ev := st.next_event()) is not None:
                if ev.kind.name == "REMOTE_PTT":
                    print(f"  event: remote_ptt={int(ev.remote_ptt)}")
                else:
                    print(f"  event: {ev.kind.name.lower()}")
            time.sleep(0.1)

        st.disconnect()
    print("done")
    return 0


def main(argv: list[str]) -> int:
    if "--dry-run" in argv or os.environ.get("IAX_PARROT_DRYRUN") == "1":
        return dry_run()
    return live()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
