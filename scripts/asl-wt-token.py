#!/usr/bin/env -S uv run --script
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# /// script
# requires-python = ">=3.11"
# dependencies = ["requests>=2.31", "python-dotenv>=1.0"]
# ///
"""Mint an AllStarLink Web Transceiver (WT) token for use as the IAX2
CALLING_NAME, replicating DroidStar's obtain_asl_wt_creds():

  1. POST callsign + AllStar ACCOUNT password to portal/login.php  (-> session cookie)
  2. GET  portal/webtransceiver.php?node=<ASL_NODE> with that cookie (-> HTML w/ token)
  3. extract the `callingName` value and print it on stdout

The node's [allstar-public] dialplan does CURL(authwebphone.pl?<token>) and only
proceeds if it returns OHYES<callsign>. The token (NOT the raw callsign) is what
flips that from "???" to "OHYES". No IP binding; mint fresh per session.
webtransceiver.php only emits the token for a node the account OWNS — the old
DroidStar dummy node 12345 returns "Node not found", so ASL_NODE must be yours.

Credentials come from .env (gitignored) or the environment:
  ASL_USER=<callsign>  ASL_PASS=<portal account password>  ASL_NODE=<node you own>

Run with uv (PEP 723 inline deps, no manual venv):
  uv run scripts/asl-wt-token.py            # prints the token
  ./scripts/asl-wt-token.py                 # same, via the uv shebang
"""

import os
import re
import sys

import requests
from dotenv import load_dotenv

LOGIN_URL = "https://www.allstarlink.org/portal/login.php"
WT_URL = "https://www.allstarlink.org/portal/webtransceiver.php"
TIMEOUT = 15


def fail(msg: str) -> "None":
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def require(name: str, hint: str) -> str:
    val = os.environ.get(name, "").strip()
    if not val:
        fail(f"set {name} ({hint}) in .env")
    return val


def main() -> None:
    # Load .env from the current working directory if present (the binary is
    # launched from the repo root, which holds the gitignored .env).
    load_dotenv()

    user = require("ASL_USER", "your callsign")
    password = require("ASL_PASS", "your AllStarLink ACCOUNT/portal password")
    node = require("ASL_NODE", "a node you OWN, for minting; e.g. 77777")

    session = requests.Session()

    # 1. Authenticate — a plain HTML form POST; success = receiving session cookies.
    try:
        session.post(LOGIN_URL, data={"user": user, "pass": password}, timeout=TIMEOUT)
    except requests.RequestException as exc:
        fail(f"login.php POST failed: {exc}")

    if "PHPSESSID" not in session.cookies:
        print(
            "WARNING: no session cookie from login.php — credentials likely wrong.",
            file=sys.stderr,
        )

    # 2. Fetch the WT page (carrying the session cookie) and scrape the token.
    try:
        html = session.get(WT_URL, params={"node": node}, timeout=TIMEOUT).text
    except requests.RequestException as exc:
        fail(f"webtransceiver.php GET failed: {exc}")

    # The page embeds <param name="callingName" value="<TOKEN>"/>. Match that,
    # then fall back to a looser pattern in case the markup shifts.
    match = re.search(r'name="callingName"\s+value="([^"]+)"', html) or re.search(
        r'callingName[^"]*"[^"]*"([^"]+)"', html
    )
    if not match:
        fail(
            "could not find a callingName token in webtransceiver.php "
            "(login failed, the account lacks WT access for this node, or the "
            "markup changed)"
        )

    print(match.group(1))


if __name__ == "__main__":
    main()
