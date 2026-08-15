#!/usr/bin/env python3
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""node.py — Python consumer of the astar-sys Node-mode C-ABI.

Mirrors the parrot.py style, but for the inbound IAX2 Node surface: configure a
listener (and optional register-as-node), register the credential resolver (the
ONLY secret channel), switch to Node mode, then poll snapshot/next_event for
Incoming/Registered events. No callbacks into Python for state — only the
resolver is invoked by the library, and only when it needs a secret.

Secret-free by construction: the registrar password is supplied solely through
:meth:`Station.set_credential_resolver`. It never appears in the node config,
any snapshot/event, or any object's repr.

Usage::

    python3 examples/node.py            # listen on 0.0.0.0:4569 (Manual answer)
    python3 examples/node.py --dry-run  # offline smoke, no network, no listener

The --dry-run path needs no network and does NOT switch to Node mode (which
would start a real listener/engine): it proves the offline Node surface
(mode/config/answer-mismatch/incoming/resolver) with a secret-free guard.
"""

import os
import sys
import time

# Allow `python3 examples/node.py` from anywhere: import the sibling module.
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..")))

from astarstation import (  # noqa: E402
    IAX_ERR_NOT_CONNECTED,
    IAX_ERR_RESOLVE,
    AnswerPolicy,
    AuthPolicy,
    Mode,
    Station,
    StationError,
    Status,
)


def dry_run() -> int:
    """Offline smoke: prove the Node surface without a listener. No network."""
    secret = "be04-secret"  # resolver return value; must never surface readable.
    with Station() as st:
        # Fresh station defaults to WT.
        if st.mode() != Mode.WT:
            print(f"expected WT mode, got {st.mode()}", file=sys.stderr)
            return 1
        snap = st.snapshot()
        if snap.mode != Mode.WT or snap.status != Status.IDLE:
            print(f"expected idle/WT snapshot, got {snap}", file=sys.stderr)
            return 1
        print(f"mode={st.mode().name} snapshot.mode={snap.mode.name} status={snap.status.name}")

        # Listen-only node config (no registrar). Does NOT switch mode.
        st.set_node_config(
            bind="127.0.0.1:0",
            answer=AnswerPolicy.MANUAL,
            auth=AuthPolicy.OFF,
        )
        if st.mode() != Mode.WT:
            print("set_node_config must not switch mode", file=sys.stderr)
            return 1
        print("set_node_config(listen-only, Manual) -> ok; still WT")

        # Unparseable bind -> RESOLVE.
        try:
            st.set_node_config(bind="not-an-address")
        except StationError as e:
            if e.code != IAX_ERR_RESOLVE:
                print(f"expected RESOLVE, got {e.code} ({e.text})", file=sys.stderr)
                return 1
            print(f"set_node_config(bad bind) -> {e}")
        else:
            print("set_node_config(bad bind) should have raised", file=sys.stderr)
            return 1

        # answer()/reject() are NOT_CONNECTED in WT mode.
        try:
            st.answer()
        except StationError as e:
            if e.code != IAX_ERR_NOT_CONNECTED:
                print(f"expected NOT_CONNECTED, got {e.code} ({e.text})", file=sys.stderr)
                return 1
            print(f"answer() in WT mode -> {e}")
        else:
            print("answer() should have raised in WT mode", file=sys.stderr)
            return 1

        # No Incoming event yet.
        if st.incoming_from() != "":
            print(f"expected empty incoming_from, got {st.incoming_from()!r}", file=sys.stderr)
            return 1
        print("incoming_from() -> ''")

        # The resolver is the ONLY secret channel; offline cannot trigger it.
        st.set_credential_resolver(lambda user: secret)
        print("set_credential_resolver() -> ok (secret stays in the resolver)")

        # Secret-free guard: the resolver's secret must not surface anywhere.
        for surface in (repr(st), str(st.snapshot())):
            if secret in surface or "secret" in surface.lower():
                print(f"secret leaked in: {surface!r}", file=sys.stderr)
                return 1
        print("secret-free guard: ok")

    print("dry run ok")
    return 0


def live(bind: str = "0.0.0.0:4569") -> int:
    """Run a Manual-answer node and poll for ~30s. Requires network + audio.

    Switches to Node mode (BLOCKING: starts the listener), then polls for
    Incoming events; auto-answers each offer and prints the caller id.
    """
    with Station() as st:
        st.set_node_config(bind=bind, answer=AnswerPolicy.MANUAL, auth=AuthPolicy.OFF)
        # The resolver is only needed if registering as a node; harmless to set.
        st.set_credential_resolver(lambda user: os.environ.get("IAX_NODE_SECRET", ""))
        try:
            st.set_mode(Mode.NODE)  # BLOCKING: device + socket setup.
        except StationError as e:
            print(f"set_mode(NODE) failed: {e}", file=sys.stderr)
            return 1
        print(f"listening on {bind} (Manual answer); polling ~30s...")

        for _ in range(300):
            snap = st.snapshot()
            while (ev := st.next_event()) is not None:
                if ev.kind.name == "INCOMING":
                    who = st.incoming_from()
                    print(f"  incoming from {who!r}; answering")
                    try:
                        st.answer()
                    except StationError as e:
                        print(f"  answer failed: {e}", file=sys.stderr)
                else:
                    print(f"  event: {ev.kind.name.lower()}")
            print(
                f"mode={snap.mode.name} status={snap.status.name:<8} "
                f"rx={snap.rx_db:.1f}dB ptt={int(snap.ptt)}"
            )
            time.sleep(0.1)

        st.set_mode(Mode.WT)  # tear the node down and deregister.
    print("done")
    return 0


def main(argv: list[str]) -> int:
    if "--dry-run" in argv or os.environ.get("IAX_NODE_DRYRUN") == "1":
        return dry_run()
    return live()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
