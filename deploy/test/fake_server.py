#!/usr/bin/env python3
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# fake_server.py — a stand-in for astar-server's public bind, for
# deploy/test/test_verify.sh. Loopback only.
#
#     fake_server.py <cert.pem> <key.pem> <mode-file> <ports-file>
#
# Serves HTTPS on one ephemeral 127.0.0.1 port and plain HTTP (the port-80
# stand-in) on another, and writes "<https> <http>" to <ports-file>. Every
# request re-reads <mode-file>: a space-separated set of faults to inject, so
# one server covers every case. Empty = a well-behaved public bind.
#
#   key200      GET /key answers 200
#   leakaddr    /public/status carries an address field
#   post200     POST / answers 200
#   anyhost     the Host allow-list is missing
#   stream503   /stream.mp3 is 503 with a reason
#   streamhtml  /stream.mp3 is 200 text/html
#   http200     port 80 serves a page instead of redirecting
import json
import ssl
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CERT, KEY, MODE_FILE, PORTS_FILE = sys.argv[1:5]
ALLOWED_HOSTS = {"localhost"}

GOOD_STATUS = {
    "node": "1999",
    "mode": "bridge",
    "links": [{"network": "allstar", "node": "2000", "direction": "in", "up_s": 42}],
    "talker": None,
    "activity": [{"t": "2026-10-12T00:00:00Z", "event": "link_up", "node": "2000"}],
    "today": {"talk_s": 0, "transmissions": 0, "stations": 1},
    "uptime_s": 3600,
    "listeners": 0,
    "stream": True,
}


def modes():
    try:
        with open(MODE_FILE) as f:
            return set(f.read().split())
    except OSError:
        return set()


class Public(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, code, ctype, body):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def host_ok(self, m):
        host = (self.headers.get("Host") or "").rsplit(":", 1)[0]
        return "anyhost" in m or host in ALLOWED_HOSTS

    def do_GET(self):
        m = modes()
        if not self.host_ok(m):
            return self.send(421, "text/plain", b"unknown host\n")
        path = self.path.split("?")[0]
        if path == "/":
            return self.send(200, "text/html; charset=utf-8", b"<!doctype html><title>node</title>")
        if path == "/public/status":
            st = json.loads(json.dumps(GOOD_STATUS))
            if "leakaddr" in m:
                st["links"][0]["addr"] = "203.0.113.7:4569"
            return self.send(200, "application/json", json.dumps(st).encode())
        if path == "/key" and "key200" in m:
            return self.send(200, "application/json", b'{"ok":true}')
        if path == "/stream.mp3":
            if "stream503" in m:
                return self.send(503, "text/plain", b"listener limit reached (25)\n")
            if "streamhtml" in m:
                return self.send(200, "text/html", b"<html></html>")
            return self.stream()
        return self.send(404, "application/json", b'{"error":"not found"}')

    def do_POST(self):
        m = modes()
        n = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(n)
        if not self.host_ok(m):
            return self.send(421, "text/plain", b"unknown host\n")
        if "post200" in m:
            return self.send(200, "application/json", b'{"ok":true}')
        return self.send(405, "text/plain", b"method not allowed\n")

    def stream(self):
        # An endless MP3-ish stream: the client must hang up, not us.
        self.send_response(200)
        self.send_header("Content-Type", "audio/mpeg")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        frame = b"\xff\xf3\x48\xc4" + b"\x00" * 140
        try:
            for _ in range(100000):
                self.wfile.write(frame * 16)
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError, OSError):
            pass
        self.close_connection = True


class Plain(Public):
    def do_GET(self):
        if "http200" in modes():
            return self.send(200, "text/html", b"<html>plaintext</html>")
        self.send_response(301)
        self.send_header("Location", "https://localhost" + self.path)
        self.send_header("Content-Length", "0")
        self.end_headers()


def main():
    https = ThreadingHTTPServer(("127.0.0.1", 0), Public)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(CERT, KEY)
    https.socket = ctx.wrap_socket(https.socket, server_side=True)
    plain = ThreadingHTTPServer(("127.0.0.1", 0), Plain)
    for s in (https, plain):
        s.daemon_threads = True
        threading.Thread(target=s.serve_forever, daemon=True).start()
    with open(PORTS_FILE + ".tmp", "w") as f:
        f.write("%d %d\n" % (https.server_address[1], plain.server_address[1]))
    import os
    os.replace(PORTS_FILE + ".tmp", PORTS_FILE)
    threading.Event().wait()


if __name__ == "__main__":
    main()
