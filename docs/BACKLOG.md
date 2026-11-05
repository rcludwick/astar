# Backlog

Durable backlog for **astar-lib**.

Session work goes in the **Claude Code task tracker** (`TaskCreate` / `TaskUpdate` /
`TaskList`): create a task before starting, mark it `in_progress` when you pick it up,
`completed` when it's done. Anything that outlives the session belongs in this file.

Migrated off the **beads** (`bd`) tracker on 2026-07-29 — do NOT invoke `bd`.
The 80 open items below each carry their full description and design text
inline. All 228 issues (164 of them closed) were exported to
`docs/issues-archive.jsonl`, which is gitignored and local-only; a committed copy of the
tracker's final state survives in git history at the migration commit.

## Open items (174)

### iax-vtrust — DONE 2026-10-31: A D-Star connect could key a radio on an FT-X CAT/PTT cable
*P1 high · safety · labels: dstar, ambe, thumbdv, serial, on-air-safety*

**Closed.** `work/vocoder-trust` merged to main as `3e5fcde` on 2026-10-31, with every opener-safety test passing. The hardware checks in the last paragraph below are still owed by Rob; they are listed in his to-do note, not here.

`SerialTransport::candidate_ports` in `vendor/ambe-thumbdv` accepted every FTDI `0403:6015` port, and `detect()` opened each in turn at 460800 and 230400 baud and wrote AMBE reset/PRODID bytes until one answered. That id covers FTDI's whole FT-X family (FT230X/FT231X), which many USB CAT/PTT cables use, and DVsticks enumerate as "FT230X Basic UART" with FTDI-assigned serials, so USB identity cannot tell a stick from a cable. Opening a tty on macOS raises DTR/RTS, so a D-Star connect could briefly key a radio wired to such a cable, twice, and write junk into its CAT port. `IAX_THUMBDV_PORT` only narrowed that same list. The test `detect_without_hardware_errors_not_hangs` ran that scan on every `just ci`.

The fix on `work/vocoder-trust`: the engine opens a vocoder port only when its USB serial is on a trust list (`VocoderTrust`), passed explicitly into every AMBE opener; an empty list opens nothing and scans for nothing. `IAX_THUMBDV_PORT` narrows within the trusted ports and a pin outside them is refused. Every open drives DTR/RTS low straight away. The only pre-trust open is enrolment, a user action after a CAT-cable warning, which classifies the PRODID reply (AMBE3000 trusted, AMBE3003 recognised but not added, anything else refused). Surfaces: Settings → Vocoders in the macOS app (`vocoder.trusted`, never exported in `.astarconfig`), `node.toml` `[vocoder] trusted_serials` plus `astar-server vocoder list/add` and a settings-page card, and `ASTAR_VOCODER_SERIALS` for the `*-listen` CLI tools. User docs: `docs/site/macos/vocoders.md`.

Closes on merge. Still owed after merge, by Rob on hardware: confirm a DVstick 30 enrols and decodes with DTR/RTS held low, that a DVstick 33 reports the "not supported yet" outcome, and the macOS app's full build and a visual check of Settings → Vocoders (not run by an agent: a Debug rebuild needs a keychain prompt only Rob can answer).

### iax-ambe3003 — Support the AMBE-3003 (DVstick 33) as a vocoder
*P2 medium · feature · labels: dstar, ambe, dvstick, cx:4*

The DVMEGA DVstick 33 carries an AMBE-3003, a three-channel part. Enrolment (`iax-vtrust`) recognises its `AMBE3003*` PRODID and refuses to trust it with "recognised but not supported yet", because the vendored `ambe-thumbdv` driver speaks the AMBE-3000's single-channel packet protocol only. Rob has three DVstick 33s.

Work: add AMBE-3003 support to the driver — channel-addressed packets, per-channel rate and init, and its baud/flow-control defaults — record the change in `vendor/ambe-thumbdv/VENDORED.md` (the crate keeps its own MIT/Apache licence fields), then let enrolment trust it with model "DVstick 33 / AMBE-3003". Decide whether astar uses one channel or exposes the other two (for example the transcoding bridge, `iax-e7c3`). Tests are hardware-free against a mock transport scripted with AMBE-3003 replies; the hardware-gated suite is Rob's to run.

Done when: a DVstick 33 enrols as trusted, D-Star decodes and encodes through it, the macOS Vocoders page and `docs/site/macos/vocoders.md` stop calling it unsupported, and `docs/FEATURES.md` says so.

### astar-echolink-incoming — accept incoming EchoLink calls, checked against the directory
*P2 · feature · labels: echolink, app, server, cx:5*

Both the app and astar-server only call out (Rob, 2026-10-26, "paused").
A control or audio datagram from a station astar did not call is dropped
without a reply. Accepting calls means: an "accept incoming calls" switch
(off by default), an inbound SDES opening a call only when the sender's
callsign is in the current directory list **and** the datagram's source IP
matches the IP the directory lists for it (EchoLib's check), a busy reply
while a call is up (the app holds one call; the server may hold several),
and the UI for a ringing station. Calling in also needs UDP 5198–5199
reachable from outside, so the docs must explain port forwarding, which
today's pages say is not needed.

### astar-echolink-proxy — EchoLink Proxy for stations behind carrier-grade NAT
*P3 · feature · labels: echolink, app, server, cx:5*

A station whose provider shares one public address among many customers
(CGNAT, most mobile networks) may not get EchoLink audio through. EchoLink's
answer is the [Proxy](https://echolink.org/proxy.htm): a TCP tunnel to a
public proxy that relays the UDP. Add a proxy client (host, port, proxy
password; the password is a secret, in-arguments only, like the login
password), behind the same loopback guard in tests, and a field in the app's
EchoLink pane and in `[echolink]`.

### astar-echolink-mixminus — a mix that keeps EchoLink out of the Discord and listener feeds
*P3 · feature · labels: echolink, server, discord, stream, cx:5*

`[echolink]` refuses to load beside `[discord]`, `[web] public_bind` and
`[stream] enabled = true`, because EchoLink's access policy forbids linking
it to VoIP systems people reach directly from a computer. A conference mix
that leaves EchoLink members' audio out of the Discord member's feed and out
of the stream would let them coexist. Before building it, confirm with
EchoLink support that a mix that never carries EchoLink audio to those
systems satisfies the policy (the draft email is in the EchoLink design,
notes `designs/astar/2026-10-26-echolink-design.md`). Only then relax the
load-time refusal, with tests that a Discord or stream feed carries no
EchoLink audio.

### astar-echolink-nodebench — a sealed interop rig with chan_echolink and EchoLib
*P3 · test · labels: echolink, nodebench, test, cx:5*

Every EchoLink test today runs against astar's own fakes on 127.0.0.1. An
interop rig would run a stock Asterisk `chan_echolink` node and an
EchoLib-based station (SvxLink) in podman against a fake directory, and call
between them and astar, so the wire format is checked against someone else's
implementation without ever touching EchoLink's servers. EchoLink's fixed
ports (5198/5199) mean each party needs its own address, so the rig would
run on podman `--internal` networks, as the soak tier's `nat` topology does.
**Needs Rob's OK** for that, as the soak exception did (CLAUDE.md,
"nodebench … stays sealed"). The rig must never resolve or reach an
`echolink.org` host.

### astar-gsm-multiframe — accept IAX2 GSM payloads with more than one frame
*P3 · task · labels: iax2, codec, gsm, cx:2*

IAX2 GSM accepts exactly one 33-byte frame per voice frame (EchoLink plan
D5). A 66-byte (two-frame) payload is dropped with the existing once-per-call
warning, because `EdgeAudio` pops one bus frame per wire frame and a
multi-frame payload would drift. If a node ever sends one (nodebench or a
real GSM-only node), decode every 33-byte frame in the payload and feed the
bus at the right rate.

### astar-echolink-m17-gate — an M17 monitor talker does not open EchoLink stations' receive side
*P2 · bug · labels: echolink, m17, server, conference, cx:2*

On astar-server, each EchoLink station hears the conference only while
another contributor is active: a non-link call's remote key, a link whose
mode relays onward (its remote key), or another EchoLink station talking
(EchoLink Task 10). The M17 monitor's conference member has no key flag, so
a talker on a monitored M17 reflector is mixed into the conference but never
opens an EchoLink station's receive gate; those stations hear silence. Give
the M17 source member a "receiving" key flag (the monitor already knows,
`m17.receiving` in `/status`) and include it in the gate, with a manager
test beside the link-remote-keyed one.

### astar-echolink-dns-deadline — directory host lookups have no deadline
*P3 · bug · labels: echolink, app, server, cx:2*

The presence thread resolves the directory host names with the system
resolver, which has no timeout of its own (EchoLink Task 7). A resolver that
hangs stalls the login, logout and list refresh behind it, and a logout can
then take longer than the ~80 s the code budgets for. Resolve on a helper
thread with a deadline (a few seconds), treat a timeout as a transport
failure (back off, rotate servers), and test it with a stalling fake
resolver under the loopback guard.

### astar-gsm-etsi-official — DONE 2026-10-31: run the GSM gate against Rob's own ETSI download (Rob)
*P2 · test · labels: codec, gsm, rob, cx:1*

**Closed.** Rob downloaded ETSI's own zip on 2026-10-31. Unpacked to `~/Library/Caches/astar/etsi-en300961/`, `gsm_etsi` passed bit-exactly against it (3 tests, no skip line).

`crates/astar-codec/tests/gsm_etsi.rs` ran green (EchoLink Task 1) against
the provisional copy of the ETSI EN 300 961 vectors in the oxideav-gsm repo,
every file matching `tests/fixtures/gsm/ETSI-SHA256SUMS`. The official check
against ETSI's own zip is still owed. Rob: download the zip in a browser
from ETSI, unpack it to `~/Library/Caches/astar/etsi-en300961/`, then run
`ASTAR_GSM_ETSI_DIR=$HOME/Library/Caches/astar/etsi-en300961 cargo test -p
astar-codec --features gsm --test gsm_etsi -- --nocapture`. Both tests must
pass with no skip line. If either fails, the remedy is a clean-room GSM 06.10
from EN 300 961, not a C fallback.

### astar-gsm-nodebench — the nodebench GSM cell (Rob)
*P2 · test · labels: iax2, gsm, nodebench, rob, cx:1*

EchoLink Task 2 added `gsm` to nodebench's `NODE_CODECS` and policy codecs,
but the rig was not run. Rob: `harness/nodebench/rig.sh build`, then
`just nodebench`, and confirm the gsm cells pass: "node dials" negotiates
GSM, and "server dials" is refused (no common codec), because astar
accepts GSM on calls it answers and never offers it on calls it places.

### astar-gsm-outbound — decide whether calls astar places should offer GSM
*P3 · task · labels: iax2, gsm, nodebench, cx:1*

The final EchoLink review took GSM out of every outbound NEW's CAPABILITY
(`CodecPolicy::offer_order`), under every policy, so a node astar dials can
never pick GSM over a better codec both ends have. The cost is that a
GSM-only node (`disallow=all / allow=gsm`) cannot be dialled from astar; it
has to dial in, where GSM is still accepted. Once `astar-gsm-nodebench` has
run, look at how stock ASL3 ranks a GSM offer against µ-law, and decide
whether offering GSM last outbound is safe. If it is, add it back to
`offer_order` and flip nodebench's `Direction::ServerDials` codec set.

### astar-echolink-tx-timeout — the app-wide transmit time-out must cover EchoLink
*P2 · task · labels: echolink, ptt, safety, cx:2*

EchoLink keying has its own hard engine cap today: the link stops sending
after 10 minutes of continuous talk (`Timings::max_transmit`, both the app's
call and the node leg's peer members), and the app's talk timer warns
before that. The app-wide transmit time-out designed on `work/ble-ptt` (one
global time-out across every source, 1–10 min, default 2, never off) must
cover EchoLink keying when it lands, with the 10-minute cap staying as the
backstop beneath it.

### astar-console-all-features — `cargo test -p astar-console --all-features` does not compile
*P3 · bug · labels: console, ci, cx:1*

Pre-existing on main. With the `serde` feature on, `ConsoleState` derives
`Serialize`, but its `denoise_status: astar_audio::DenoiseStatus` field does
not implement it:

```
error[E0277]: the trait bound `DenoiseStatus: serde::Serialize` is not satisfied
    --> crates/astar-console/src/state.rs:48:38
```

`just ci` never builds that combination, so nothing caught it. Derive
`Serialize` on `DenoiseStatus` behind astar-audio's own `serde` feature (or
skip the field), and consider an `--all-features` build of the leaf crates
in CI.

### astar-echolink-minors — small EchoLink follow-ups found in review
*P3 · task · labels: echolink, cx:2*

Each was judged too small to hold up the EchoLink branch:

- **Outbound chat and info text** are not stripped of `>` or CR before they
  go out (`chat_datagram` / `info_datagram`), and an empty sender yields a
  chat line from `""`.
- **Login reply parsing:** `parse_login_reply` accepts any reply that starts
  `OK`, and checks `OK`/`INCORRECT` before skipping leading CR/LF, so
  `"\r\nOK"` reads as a refusal quoting `OK`. A BOM or space before `@@@`
  gives a misleading inflate error.
- **TX packet timing:** packets leave on a 50 ms grid, so 80 ms of GSM goes
  out unevenly. Watch it on the node leg.
- **"Directory unreachable" hint:** once logged in, a directory that stays
  down keeps the state `LoggedIn` indefinitely (by design, to avoid a
  misleading "log in first"); the app could say the list is stale.
- **Spawn failure in `start_replacing`** blocks and loses the old login
  (very unlikely).
- **Downstream test builds** (cli, inspect, gui) build
  astar-console's `echolink` without `echolink-loopback-only`; harmless until
  one of their tests exercises EchoLink.
- **`ci/guard-echolink.sh`'s socket scan** does not scan `crates/*/tests`.
- **`just gsm-check`** is now redundant with astar-iax enabling `gsm`, but
  harmless.
- **The `[echolink]` block in `deploy/node.toml.example`** has no parse test
  of its own, unlike `[m17]` (`m17_example_block_parses`).

### astar-dns-ns — Look up a zone's nameservers so DNS-01 asks them directly

*P3 low · task · labels: server, web, dns, cx:3*

DNS-01 with Porkbun asks Porkbun's nameservers directly whether the challenge
record is visible. Every other provider waits until the provider lists the
record and then a blind 60 seconds, because the node does not know their
nameservers. Look up the zone's NS records, ask those servers, and drop the
fixed wait.

### astar-dns-more — Providers that need RFC 2136/TSIG or two credentials

*P3 low · feature · labels: server, web, dns, cx:5*

The ten providers wired today each take one API token. Add RFC 2136 dynamic
update with TSIG keys, and the providers that need two credentials: GoDaddy,
DNSimple, Name.com, Route 53 and similar. A new provider is a row in
`DNS_UPDATE_PROVIDERS` and an arm in `public::dns_update::build_updater`; a
two-credential one also needs the credential store to hold a pair.

### astar-dns-ui — Settings page fields for DNS provider tokens

*P3 low · task · labels: server, web, dns, ui, cx:3*

The `dns:<provider>` tokens can be set by environment variable, in the
credentials file, or with `POST /secrets`, but the settings page has no field
for them (only Porkbun has one). Add credential fields for the `dns:<provider>`
keys, shown for the provider chosen in `[web.dns]`, set-only like the other
credentials.

### astar-sec-review — DONE 2026-10-13: security review of the web-facing code, and its fixes
*P1 · task · labels: server, web, auth, security, cx:4*

**Closed.** A four-part read-only review (public bind and streams; sign-in;
control port and settings; ACME, Porkbun, DNS and the pages) found nothing
critical. Every finding was fixed test-first on `work/sec-public`,
`work/sec-auth`, `work/sec-control` and `work/sec-acme`, merged as
`work/sec-fixes`:

- one deadline for the whole request (TLS handshake, head, body); per-address
  connection caps (IPv6 per /64); port 80 has its own pool so ACME http-01 is
  never starved;
- per-address cap on long-lived streams (4) and a global cap on event feeds;
- HSTS over TLS;
- open streams end within 5 s of sign-out, removal, demotion or expiry;
- invites die with the admin who made them;
- a stranger can no longer lock an app-only account out (per-source lockout);
- the TOTP attempt is checked, evaluated and counted atomically;
- unknown names answer like real ones (lockout and `key_begin` decoys);
- limits keyed per IPv4 address / IPv6 /64; ceremony table capped per client;
  anonymous failures rolled up in the audit ring;
- removing a credential ends the sessions it signed in;
- `announce.tts.binary` needs an acknowledgement and an absolute path;
- `/link` `addr` accepts loopback literals only;
- a wildcard `[control] bind` needs `allow_wildcard_bind = true`; "local" is
  judged by the socket peer, never `Host`;
- `POST /secrets` never echoes the body;
- `credentials.file` set from the page stays beside the config or in the
  state directory;
- the certificate is saved as one file and a key that does not match its chain
  is refused; an issued certificate is never re-ordered for a failed save
  (at most 3 orders a week per name set);
- DNS-01 TXT records are cleaned up even when the create's answer is lost;
  the propagation check uses a random id, matches the question, and has one
  deadline.

Leftovers are the `astar-sec-*` items below.

### astar-sec-totp-bound — decide the TOTP guess bound across many addresses — DONE 2026-10-13
*P2 · decision · labels: server, auth, security, cx:2*

Since `astar-sec-review`, the per-user lockout is per source (IPv4 address or
IPv6 /64) plus a strangers' lockout that never escalates past 15 minutes, so a
stranger cannot keep the owner out. The cost: an attacker with many addresses
gets about 100 guesses a day against one name, roughly a 10% chance of a hit
in a sustained year (the design said under 1%). Options: let the strangers'
lockout escalate (the owner's trusted sources already ignore it, so only an
owner on a new network would be kept out); or require a passkey for admins
and accept the bound for viewers. Rob to decide.

**Closed 2026-10-13 (Rob): admins sign in only with a passkey or security
key; codes sign in viewers only.** An admin's code at `/auth/totp` fails
inside the same `attempt()` write exactly as a wrong one (status, body,
lockout, audit), and TOTP step-up in an existing session stays. An admin
always keeps a key: admin (and admin re-enrol) invites refuse an app, nobody
without a key is promoted, and an admin's last key can't be removed. An admin
from before the rule with only codes keeps them until they add a key, with a
start-up warning naming them; the hostname guard rail no longer counts an
admin's app once they have a key.

### astar-sec-decoy-timing — `key_begin` timing for unknown names is not equalised
*P3 · task · labels: server, auth, security, cx:2*

Decoy answers now match real ones in shape, but their latency is not
equalised, and the decoy lockout map is in memory (4096 names; a restart or
a flood forgets it). Measure, then pad or precompute if the gap is visible.

### astar-sec-ceremony-churn — more than 1024 /64s can still churn the ceremony table
*P3 · task · labels: server, auth, security, cx:2*

The table is 1024 entries, 8 per client, evicting from the heaviest holder.
An attacker with more than 1024 IPv6 /64s can still evict legitimate
in-flight ceremonies inside the 5-minute TTL. A per-/48 tier or a cookie
proof-of-work on `begin` would close it.

### astar-sec-expiry-test — no test that an open stream ends at session expiry
*P3 · test · labels: server, auth, test, cx:1*

The stream recheck ends a stream at sign-out, removal and demotion (tested),
and at expiry (not tested: the rigs use the real clock). Inject the clock into
`AuthService` for this test.

### astar-http-total-write — one deadline for writing a whole answer
*P3 · task · labels: server, http, security, cx:2*

astar-http bounds the request phase with one deadline, but an answer is
written under only a 30 s stall limit per write. A client that reads one
byte every 29 s never stalls long enough to be dropped, so it can hold a
connection (and its slot) for as long as the answer is large. The per-IP cap
of 16 bounds the damage but does not remove it. A total deadline for the
whole answer, sized to the largest body, would close it; streams (SSE, audio)
keep their own rules.

### astar-http-accept-poll — the accept loop polls every 50 ms
*P3 · task · labels: server, http, cx:1*

The astar-http accept loop polls a non-blocking listener every 50 ms. That is
new on the control port, which used to block in tiny_http, and adds about
25 ms of latency on average to each new connection. Switching to a blocking
accept, woken on stop by a connection the server makes to itself, would
remove both the polling and the added latency.

### astar-ddns-aaaa — DDNS manages only A records; apex DDNS rewrites every A
*P3 · task · labels: server, dns, cx:2*

`ddns = true` rewrites every A record at the name (`editByNameType`), so on an
apex it overwrites a website's records; and AAAA is never managed, so a stale
AAAA sends IPv6 clients (and Let's Encrypt's http-01, which prefers IPv6)
elsewhere. Document the apex behaviour in `listening.md`, and either manage
AAAA or warn at start-up when one exists.

### astar-auth-webauthn-rp — evaluate `webauthn_rp` to drop OpenSSL from release
*P4 · research · labels: server, auth, cx:3*

OpenSSL is in the release build only because webauthn-rs requires it. Rob
chose (2026-10-13) to keep the audited webauthn-rs. If OpenSSL ever needs to
go, `webauthn_rp` (RustCrypto: p256, p384, rsa, ed25519-dalek; unaudited)
is the candidate; the library-specific code is `auth/webauthn.rs` plus the
ceremony state. An evaluation must cover attestation, counters, no-PIN keys,
and a fuzzing pass over malformed authenticator data.

### astar-features-md — `docs/FEATURES.md` is missing on main
*P2 · docs · labels: docs, cx:2*

CLAUDE.md requires every user-visible change to update `docs/FEATURES.md`,
but it exists only on the unmerged `work/ambe3003` branch (commits d935331,
6b00593, c67393d). The listener, auth and security-review changes documented
themselves under `docs/site/server/` instead. Land the file on main and add
their rows.

`work/recording` and `work/echolink` each created the file with their own
rows only (recording; EchoLink), and merging main into `work/echolink`
(2026-10-30) put both sets under one header. The rest is
`astar-features-list`.

### astar-listener-signin — DONE 2026-10-12: `/admin` on the public page becomes the passkey sign-in
*P2 · feature · labels: server, web, auth, cx:3*

**Closed.** Done in `work/server-auth` (design: notes
`designs/astar/2026-10-12-server-auth-design.md`). With the `auth` feature
(in `release`) and `[web] admin_on_public`, `/admin` leads to
`/admin/signin`. The public router's sign-in groups are explicit
(`SIGNED_IN_GROUPS` beside `PUBLIC_ROUTES`): `/auth/*` and the sign-in
pages are answered by the sign-in code, and `/admin/<route>` reaches the
admin router only for a route in `access::ADMIN_ROUTES` and an admin
session (step-up by class). Unprefixed admin routes stay 404 there, tested.
Operator docs: `docs/site/server/admin.md`. Original text follows.

The public listener page (`docs/site/server/listening.md`) links to `/admin`, which today is a placeholder explaining the SSH tunnel to the loopback control port. When the operating model's passkey sign-in ships (`notes: designs/astar/2026-10-11-server-operating-model-design.md`), `/admin` becomes the sign-in page on the public bind. The seam is `PUBLIC_ROUTES` in `crates/astar-server/src/public/http.rs`: add each signed-in route there explicitly, behind its session check. Never forward the public bind to the admin router.

### astar-auth-browser-ci — the sign-in pages' browser run belongs in CI
*P2 · test · labels: server, auth, web, cx:3*

The sign-in pages were checked once by hand (2026-10-12, `work/server-auth`):
Playwright with Chromium's virtual authenticator against a local node on
`127.0.0.1` with `[control] loopback_login = true` — invite → passkey →
add an authenticator app → sign out → discoverable sign-in (autofill) →
code sign-in → "Confirm it's you" → remove a method → users and invites,
light and dark, desktop and phone. Every flow is also tested in Rust through
the handlers with software authenticators, but the JavaScript (WebAuthn JSON
conversion, the step-up retry, autofill abort) is not tested in CI. Make that
run a script in the repo (`just auth-browser-test`, skipped without a
Chromium) and run it in CI. It ran on the control port because
`localhost` is not accepted as a `[web] public_hostnames` entry (it can
carry no certificate); a test-only escape for that would let it cover the
public bind too.

### astar-auth-psl — `[web] rp_id` refuses shared parents from a short list, not the public suffix list
*P3 · task · labels: server, auth, cx:2*

`config::rp_id_problem` refuses a single label, and a built-in list of
shared wildcard-DNS and dynamic-DNS parents (`sslip.io`, `nip.io`,
`duckdns.org`, `co.uk`, …). The design asks for "anything on the public
suffix list". Vendoring the PSL (MPL-2.0 data) or a crate that embeds it
would make the refusal complete; the list covers the parents a node is
likely to sit under.

### astar-auth-guard-defaults — the hostname guard cannot see DNS-derived hostnames
*P3 · task · labels: server, auth, settings, cx:2*

`auth::guard` checks a settings save that names `web.public_hostnames` (as
a list) or `web.rp_id`. A save that removes the list, so the hostname falls
back to `[web.dns] host`/`domain`, or that changes only `web.dns.*` or
`web.public_bind`, is not checked, because the guard does not run the
loader's expansion. Run the patched file through `NodeFileConfig` and take
`WebCfg::hostnames()` instead.

### nodebench-stream — nodebench scenario: the stream carries link audio, not announcements
*P2 · test · labels: server, stream, nodebench, cx:2*

Plan Task 13 of the listener work, deferred until `work/nodebench` merges. In the ASL3-in-podman bench, enable `[stream]` and `[web] public_bind` on the rig's astar-server (publish `127.0.0.1:28743`, `public_hostnames = ["nodebench.invalid"]`), have the ASL3 node play a 1 kHz tone into the link, read `/stream.ws` (and `/stream.mp3` with `stream-mp3`), and require the tone 20 dB over the floor within 3 s; then trigger an announcement on that link and require RMS under −50 dBFS for 2 s while the tone is stopped.

### astar-listener-features — add the public page's row to `docs/FEATURES.md`
*P3 · docs · labels: server, docs, cx:1*

`docs/FEATURES.md` exists only on `work/ambe3003`, so the listener work could not add its row. When both have merged: one astar-server row, "Public read-only listener page: status, graphs, live Opus and MP3 streams, HTTPS by built-in ACME, Porkbun dynamic DNS and DNS-01", macOS and Windows/Linux "n/a (server feature)", linking `docs/site/server/listening.md`.

Add the sign-in rows from `work/server-auth` at the same time, each macOS and
Windows/Linux "n/a (server feature)", linking `docs/site/server/admin.md`:
"Admin sign-in with passkeys, security keys (with or without a PIN) and
authenticator apps, on the public page and the control port"; "Invites,
roles (admin, listener), re-enrol links and the activity log"; "`astar-server
admin` invites, users and sign-outs from the console"; "A private listener
page (`anonymous_status = false`)"; and, under the control port, "Browser
form posts refused (the Origin rule, iax-ctlcsrf)".

### iax-cnlink — astar-server drops its link to a ClearNode
*P1 high · bug · labels: server, iax2, allstar, link, cx:5*

Reported 2026-10-10 (Rob): `astar-server` disconnects from his ClearNode. The link comes up and later goes away without anyone dropping it. We don't yet know how long it stays up, which side tears it down, or whether it re-links by itself. Find those out first, in the rig from `iax-nodelab`, before any change.

What to capture on both ends of one failing link:

- IAX2 on the server's 4569 (tshark), plus the node's Asterisk log at `iax2 set debug on` / `rpt debug level` verbosity. That shows the last frames before the drop and who sent the HANGUP, or whether the call just stopped answering.
- Timings: link up to drop, gaps in PING/PONG and LAGRQ/LAGRP, the app_rpt keepalive text frames, and registration refreshes.
- Whether the home side is behind NAT, and its router's UDP mapping timeout. An idle UDP mapping that expires is the first suspect: one side stops hearing the other, and the far end retransmits until it gives up.

Candidate causes to rule in or out, in order: a NAT mapping expiring because the server sends nothing while idle; a missing or late reply to POKE/PING/LAGRQ; app_rpt's link keepalive not being answered; registration expiry; and the call's own retransmit or timeout logic hanging up.

Done when: the cause is named with evidence from a capture; a loopback test (127.0.0.1 only) reproduces it and fails before the fix; and the rig holds a link to a stock ASL3 node for 24 hours with zero drops. `key_refusal` and the transmit time-out are untouched. The test node has no transmitter, and keying in the rig stays a manual action.

**Rob, 2026-10-11:** the ClearNode drops the link after **one or two seconds**. That rules out NAT idle expiry as the first suspect, because that takes minutes; look at call setup and the first exchange instead.

**What nodebench has shown so far (2026-10-11).** nodebench is the local rig: a stock ASL3 node and astar-server in podman (`harness/nodebench/`, findings in `harness/nodebench/RIG.md`). These are leads, not a confirmed cause. Rob still has to say whether his drop follows a reconnect and which side dials.

- **Best lead: a re-link drops in 1.2–2.5 s, every time** (`relink_after_drop`, RIG.md finding 4). astar-server ACKs a frame with `iseqno` equal to its `oseqno`, where Asterisk expects `oseqno + 1`. So the node's HANGUP that ends one link is never acknowledged, and Asterisk retransmits it 2 s later. By then astar-server has reused IAX call number 1 for the node's next link. It takes the stale HANGUP, whose source call is the OLD call, as belonging to the new link and tears that down. After that it answers the node's voice with INVAL, while the node still lists the link as ESTABLISHED. Two fixes are implied: ACK with `oseqno + 1`, and reject full frames whose source call number is not the call's peer.
- **About 6 % of inbound links never come up** (finding 5). astar-server's 16-byte random binary call tokens can contain a NUL byte. Asterisk echoes the token as a C string, so it comes back truncated, and astar-server answers with REJECT "Call Token Invalid".
- **From 2 s into every node-dialled link, the node ignores astar-server's audio** (finding 6). astar-server never answers app_rpt's `!NEWKEY1!` and never sends RADIO_KEY, so app_rpt's 2 s `newkeytimer` expires. This is not a drop, but to a listener on the node it would sound like one.
- **A link astar-server dials is hung up 0.6 ms after ACCEPT** (finding 1). The NEW carries no CALLING NUMBER, and app_rpt logs "Caller ID is not numeric".
- **Not reproduced:** a plain first-time `ilink 3` from the node held 90 s in the hand gates, and for the whole hold in the quick tier's 10 s runs, with ulaw only and with slin. A full quick-tier run with 60 s holds has not finished yet.
- Also found, and not the drop: after `/link disconnect` the node keeps the link for about 8–10 s, because astar-server's HANGUP has timestamp 0 (finding 3). Registration against ASL3 storms and exhausts the node's call numbers (findings 2 and 8). A monitor link's unkeyed audio is bridged (finding 7).

**Fixed on `work/nodebench` (2026-10-11), pending Rob's real-ClearNode check.** This item stays open until a real ClearNode holds a link to astar-server, in whichever direction Rob's drop happens.

- **Re-link drop, cause confirmed and fixed** (`626df4d`). The listener consumed the node's opening NEW without counting it: through the demux, and again through the CALLTOKEN reset after the token NEW. Every ACK on an inbound leg was therefore one behind. Separately, the demux routed in-call frames by destination call number alone. Inbound legs now count the NEW (`Reliability::restart_after_peer_new`), and the token NEW bypasses dedupe. A frame whose source call is not the leg's peer gets INVAL, the way chan_iax2 answers a call it does not know. Mini frames now route by (address, source call). The loopback tests are in `crates/astar-iax/tests/inbound_seqno_loopback.rs`: 4 tests, all failing before the fix. `relink_after_drop` now passes 6 of 6 runs; the node's HANGUP is sent once and ACKed with `iseqno` = `oseqno` + 1. The duplicated DTMF `#` went away with it: `dtmf_link_command` passes.
- **NUL call tokens fixed** (`fb17eb8`). Tokens are now 16 printable bytes.
- **Server-dialled links fixed** (`3c34307`). New `CallMode::NodeLink` sends our node number as CALLING NUMBER, as app_rpt's own link dials do. `server_links_to_node` now holds 60 s without the `--fill-callerid` workaround, against slin16 and against a ulaw-only node.
- **Bench after the fixes** (60 s holds; slin16 node, then ulaw-only):
  - Pass: both dial directions hold 60 s, `ulaw_only_node`, `relink_after_drop`, all 24 `codec_matrix` cells, `dtmf_link_command` and `keepalive_exchange`.
  - Still failing then, and not drops: NEWKEY (finding 6), and the slow clear after `/link disconnect` (finding 3).
- **The rest fixed on `work/iax-fixes` (2026-10-12), each with a loopback test that failed first and a bench scenario that checks it.** Details and evidence are in RIG.md "Fix status".
  - Findings 2 and 8, registration storm (`18eed6b`): the token REGREQ now restarts at `oseqno` 0, retry timers no longer enqueue fresh REGREQs, and a VNAK from an unknown call is not answered. `register_with_node` registers with one call number.
  - Finding 6, NEWKEY (`564d091`, `306bd92`): app_rpt's TEXT strings carry their C NUL, so `!NEWKEY1!` never matched. Once it was answered, the bridge's continuous silence held the node keyed, so a member is now sent the mix only while someone it hears is talking. Server → node audio passes.
  - Finding 7, monitor bridging (`dbeeeb6`): app_rpt's keying rule per leg (`link_keying`).
  - Finding 3, slow clear (`a77f81b`): app_rpt holds a link that hangs up without `!!DISCONNECT!!` for `DISC_TIME` (10 s). astar now says it first, to app_rpt peers, and stamps the call clock on control, TEXT and HANGUP. Clears in 0.2 s.
  - Token-less NEW retransmit (`ca622a2`): now answered with CALLTOKEN, not REJECT. New scenario `calltoken_retransmit`.
  - The three debug-noise lines were small state-machine gaps, now closed (`2506816`, `9292841`).
- **Not done:** the 24-hour soak from "Done when", and Rob's check against a real ClearNode.

### iax-nodelab — a cloud test rig: astar-server on a VPS, with an AllStarLink node at home linked to it
*P1 high · infra · labels: server, testing, allstar, deploy, cx:5*

There is no real-world rig for `astar-server` linking. Bugs like `iax-cnlink` only show up across the internet, through NAT, against a stock app_rpt node, over hours. Build one that stays up and can be soaked.

- **Server side:** `astar-server` on a spare cloud VPS, deployed the way the production node is. That means the Ansible in the infrastructure repo as a role or host entry, not by hand: same image, same unit, its own node number and secrets supplied at deploy time and never committed. Firewall: 4569/udp in, and the control port reachable only from home.
- **Node side:** a stock AllStarLink 3 node on a box at home (a Pi or a VM), set to link to the VPS node at boot and re-link on drop. **No radio and no transmitter attached**: a dummy channel or a USB sound device with nothing keyed. The rig must be unable to put RF on the air.
- **Observability:** a soak script that records link up and down events with timestamps from both sides (the server's `/status` and the node's `rpt stats`), keeps a rolling IAX2 capture on the VPS, and writes a daily summary: drops, durations and who hung up.
- **Docs:** a page under `docs/` on how to bring the rig up and tear it down, and how to read the soak summary. Node numbers, addresses and secrets go in the private infrastructure notes, not here.

Done when: the VPS node and the home node link automatically after a reboot of either; the soak runs for 24 hours and produces its summary; and `iax-cnlink` can be reproduced, or ruled out, on it.

**The local half exists (2026-10-11): nodebench.** A stock ASL3 node (1999) and astar-server (1998) run in podman on the Mac, with no radio, on 127.0.0.1 inside one pod. The code is `harness/nodebench/` and `crates/astar-nodebench`, run with `just nodebench`, and documented in `harness/nodebench/README.md` and `docs/nodebench.md`.

- **What it has:**
  - a quick tier of 15 scenarios: link both ways, holds, NEWKEY, codecs, auth, audio both ways, monitor links, disconnects, keepalives, DTMF, the CALLTOKEN round and registration;
  - a soak tier (`nodebench soak`) that includes a `nat` topology on podman `--internal` networks with a settable conntrack UDP timeout;
  - a results page on 127.0.0.1:8740.
- **What it does not replace:** this item's real-world half. That is the VPS server, a home node through a real router, an internet round trip and a 24-hour soak. A real ClearNode, too, since the ulaw-only ASL3 node is only a stand-in.
- **CI:** `ci.yml` has a `nodebench` job that is off by default. The IONOS runner needs nested rootless podman first, and README.md "CI" lists exactly what the runner needs.

### iax-wglink — An outgoing WireGuard link mode: 44link, plain WireGuard, later Tailscale
*P2 medium · feature · labels: server, wireguard, link, network, cx:5*

Requested 2026-10-11 (Rob): "we also want an outgoing wireguard link mode. This should work with 44link, wireguard, and eventually tailcat/tailscale."

**What exists.** `[wireguard]` (iax-580b) is one userspace WireGuard stack to one VPS peer. When the section is present, it selects `LinkTransport::Wireguard` for the **whole** engine: outgoing calls, the registrar and the inbound listener. Its purpose is a reachable inbound port behind CGNAT.

**What is asked for.** An outgoing link that reaches its far node through a WireGuard tunnel, presumably chosen per link rather than for the whole engine. It must work with:

- 44link, the AMPRNet / 44Net WireGuard service;
- a plain WireGuard peer;
- later, Tailscale.

**Not yet designed.** It needs a brainstorming pass first. Questions for it:

- per-link versus per-engine transport;
- several tunnels at once;
- how a link names its tunnel;
- whether Tailscale means `tsnet`/userspace or the system daemon;
- how 44Net addressing and DNS interact with node lookup.

Secrets stay as they are in iax-580b: references only, resolved at build time, never in config or logs. nodebench can test it locally with a WireGuard peer container on a podman `--internal` network.

### iax-deploygate — post-deploy verification: wire the rest of the pipeline to `deploy/verify.sh`
*P2 medium · infra · labels: server, deploy, testing, cx:2*

Built 2026-10-12 on `work/deploy-gate`: `deploy/verify.sh` (`just deploy-verify`) is an observation-only gate for the node VPS. It checks the container, image tag, restart, image count, disk, `/status` registration, secrets in the log and UDP 4569 over ssh, and DNS, TLS and the public routes once `ASTAR_PUBLIC_HOST` is set. `deploy/deploy-vps.sh` runs it after the restart, prints the rollback command on a FAIL, and prunes old images only after it passes. Its hermetic tests are `just deploy-verify-test`, part of `just ci`. See `deploy/README.md#verify`.

Left to do:

- **gh-runners' `deploy-node` workflow should call the same script** (a follow-up in that repo). `ci/deploy-node-vps.sh` has its own inline `registered` poll and checks nothing else. It also runs `podman image prune -f` straight after the restart, *before* that gate, so a failed deploy there loses its fallback image: the bug `deploy-vps.sh` just fixed. The workflow also still checks out `rcludwick/iaxclient-rs` and builds `iaxclient-node`, so it needs repointing at this repo before it can run at all.
- **Turn the public checks on when the listener deploys** (`2026-10-12-listener-design`): set `ASTAR_PUBLIC_HOST` (the sslip.io name first) in the deploy's environment, and confirm all fourteen public checks pass against the real box. Until then they SKIP.
- Consider running `verify.sh` on a timer from the runner box, not only at deploy time, so a node that stops registering is noticed within the hour rather than at the next deploy (the 2026-08-22..30 gap).

Done when gh-runners' deploy calls `deploy/verify.sh`, prunes only after it passes, and a deploy with `ASTAR_PUBLIC_HOST` set goes green.

### astar-uid — Audio devices need a stable identity, not their name
*P2 medium · bug · labels: audio, macos, migration, cx:5*

A device's NAME is its identity today. `DeviceId` is `"in:<name>"` /
`"out:<name>"` (`crates/astar-audio/src/device.rs`), built in `enumerate`
and resolved by `find_device`, which returns the FIRST device whose
`cpal::Device::name()` matches (`crates/astar-audio/src/stream.rs`). The app
persists the bare name too — `audio.input` / `audio.output` in
`com.aj7hr.astar`, and `Setup.inputDevice` / `Setup.outputDevice`.

Names are not unique. An ICOM IC-7300 and an AllScan UCI150 both enumerate as
`USB Audio Device`. Reproduce on any Mac without either:

```
swiftc -O -o /tmp/dupdev apps/macos/Tools/dup-audio-devices.swift
/tmp/dupdev create
cargo run -p astar-audio --example list_devices    # TWO entries, id=in:ASTAR DUP TEST
/tmp/dupdev destroy
```

Three consequences, in increasing order of harm:

1. Duplicate `ForEach` ids and duplicate `.tag`s made every row with that name
   read as selected. **Fixed** in astar-9d41 by offering one row per distinct
   name plus a warning; that is a stopgap, not this ticket.
2. The second device is unreachable. No selection can open it.
3. **A silent swap.** A stored `"USB Audio Device"` binds to whichever device
   enumerates first. Plug in a second same-named gadget and astar may open it
   instead, with no visible change — you key one radio over serial PTT and send
   audio to another. Not a transmit-safety break (PTT is a separate line) but
   silently wrong.

**Design:** switch identity to `kAudioDevicePropertyDeviceUID`, which is stable
across replug — e.g. the UCI150 reports
`AppleUSBAudioEngine:C-Media Electronics Inc.:USB Audio Device:1124000:2,1`.

The obstacle is cpal. In 0.15.3 `Device.audio_device_id` is `pub(crate)` with
no public accessor and no UID method (`src/host/coreaudio/macos/mod.rs`), so
astar cannot ask cpal which CoreAudio device it is holding. Options, roughly in
order of preference:

* Enumerate UIDs via `CoreAudio` alongside cpal and correlate by position
  within `input_devices()` / `output_devices()`. No fork, but leans on cpal's
  enumeration order matching `kAudioHardwarePropertyDevices`; needs proving
  before it is trusted.
* Upstream a `Device::uid()` (or expose the raw id) to cpal. Cleanest, slowest.
  Per the repro-first rule, land a reproducer before opening anything upstream.
* Vendor a patched cpal. Last resort — the nusb episode is the precedent for
  how much that costs.

Migration is the other half and cannot be skipped: every existing `audio.input`
/ `audio.output` and every `Setup` holds a bare name. Resolve names to UIDs
once on first launch after the upgrade, keep the name as a display label and as
a fallback when the UID is absent, and leave configs whose device is unplugged
alone rather than clearing them.

Non-macOS hosts have no `CoreAudio` UID; keep the name-based path for them
behind the same `DeviceId` abstraction.

### iax-5d90 — Bump boringtun off release-candidate crypto (curve25519-dalek advisory)
*P3 low · chore · labels: security, wireguard, cx:1*

`cargo audit` flags RUSTSEC-2024-0344 (timing variability in
`curve25519-dalek`'s `Scalar29::sub`/`Scalar52::sub`) against the shipped
macOS binary. The path is
`boringtun 0.6.0 -> x25519-dalek 2.0.0-rc.3 -> curve25519-dalek 4.0.0-rc.3`,
via `astar-wireguard`, which `astar-console` and `astar-iax` depend on
unconditionally — so the code ships whether or not a WireGuard transport is
ever used. Note those are release *candidates* from 2023, which is its own
reason to move.

`cargo update` cannot fix it: the rc versions are pinned by boringtun 0.6.0's
own requirements. **boringtun 0.7.1 exists** — bump and re-audit.

Deliberately not blocking the 0.1.1beta release: a timing side-channel needs an
attacker able to measure precisely against a transport that is beta and
largely unexercised. Revisit before anything is described as production-ready.

Also outstanding from the same scan, all lower: `ring 0.16.20` unmaintained
(NOT in the macOS app's graph), the gtk-rs GTK3 stack unmaintained (Linux
client only), and `lru`/`glib` unsoundness warnings.

### astar-discord-tls — the Discord bridge pins rustls 0.22 and rustls-webpki 0.102 (Dependabot)
*P2 medium · security · labels: deps, server, discord, tls, cx:2*

Dependabot alerts #4–#11 (2026-11-01; two high, three moderate, three low) are all
on the Discord bridge's dependency graph, not on the macOS app.
`serenity` 0.12.5 pulls in `tokio-tungstenite` 0.21 → `tokio-rustls` 0.25 →
`rustls` 0.22.4 → `rustls-webpki` 0.102.8, an old line with no patched
release, and the HPKE path pulls in `libcrux-chacha20poly1305` 0.0.7 (fixed
in 0.0.8). The rest of the workspace is already on `rustls` 0.23.46 and
`rustls-webpki` 0.103.15. `cargo update` cannot move them because `serenity`
pins the versions. It reaches the static `astar-server` release through the
`discord` feature, where it is the client side of the bot's connection to
Discord's gateway. The fix is a `serenity` (and `songbird`) release on
tungstenite 0.2x with `rustls` 0.23, or a `[patch]` if upstream lags.
Re-check `cargo tree -p astar-server --features release -i rustls@0.22.4`
and `-i libcrux-chacha20poly1305` after any bump.

### astar-dstar-perens — vendor Bruce Perens' D-Star vocoder as a software option beside the dongle
*P2 medium · feature · labels: dstar, ambe, codec, vendor, licensing, cx:4*

Rob (2026-11-01): vendor Bruce Perens' D-Star library, so D-Star can use either
the ThumbDV/DVstick dongle or Perens' software implementation. This is the case
CLAUDE.md already allows: D-Star's AMBE 3600×2400 is out of patent, and a
software backend is permitted if its licence is AGPL-compatible.

Confirm before writing any code:

- **Which code, exactly.** Rob: the implementation is in one of Bruce
  Perens' GitHub repositories (an open-hams-style repo), and it isn't
  labelled "dstar". Find it there, and confirm it implements D-Star's AMBE
  3600×2400 and what state it's in.
- **Licence**: must be AGPL-3.0-compatible. Record it in
  `LICENSE-EXCEPTIONS.md`, following the `codec2` entry.
- **Provenance**: it must not derive from `rcludwick/ambe`, which astar never
  references or vendors.

The rules it has to follow, from CLAUDE.md "D-Star / AMBE":

- an opt-in cargo feature, off by default, vendored as its own licence island
  under `vendor/` with a `VENDORED.md` (hard-coded licence and version in its
  `Cargo.toml`, never `.workspace = true`);
- the dongle stays the default;
- the software/hardware choice is **per mode, D-Star only**: it must never reach
  DMR, NXDN or YSF (AMBE+2, US8359197 until 2028-05-20), and a UI that greys
  those out says why;
- vocoder trust (`iax-vtrust`) is unaffected, because a software backend opens
  no port.

Then: a vocoder choice for D-Star in the macOS app's Settings → Vocoders, the
same in `node.toml` for astar-server, a `docs/FEATURES.md` row, and user docs.
Check it against the dongle by decoding the same frames both ways. Fixtures
must come from a source that can be committed, and the hardware half is Rob's
to run.

**Built 2026-11-01 on `work/dstar-perens`, unmerged.** Still open for the
hardware half.

- Found: `BrucePerens/hams_open`, `daemons/ham_digital_modes/src/ambe/`
  (crate `ham_digital_modes`), pinned at
  `f3c12933ff17bcd2a710adc11c64ca407f8f0e12`. Pure Rust, D-Star AMBE
  3600×2400 encode and decode, validated by upstream against a real
  AMBE-3000R. `LGPL-3.0-or-later`; tables from mbelib/DSD (ISC). No sign of
  `rcludwick/ambe`.
- Vendored at `vendor/ham-digital-modes-ambe` without its AMBE+2 directories
  (`VENDORED.md`, `LICENSE-EXCEPTIONS.md` §9, `ci/guard-dstar-soft-ambe.sh`).
- Feature `dstar-soft-ambe` (codec/console/station/sys/server), off by
  default; `AmbeBackend::Software` behind `AmbeStream`, refused for every
  mode but D-Star. macOS Settings → Vocoders picker; `node.toml`
  `[vocoder] dstar`.
- Rob decided 2026-11-01: build it in by default, with a config switch.
  `just xcframework` builds it in (`ASTAR_DSTAR_SOFT_AMBE=0` opts out) and the
  server `release` set includes it; the cargo feature stays explicit. The
  dongle stays the default choice; Settings → Vocoders and a refused D-Star
  connect offer Software one click away.
- Rob still owes: decode the same received frames through the dongle and the
  software vocoder and compare by ear; transmit through each to a parrot and
  compare.

### astar-rec-echolink-beat — an EchoLink recording had a beat about once a second
*P2 medium · bug · labels: recording, echolink, audio, cx:2*

Rob (2026-11-01): one of two app recordings of the same EchoLink station has a
regular beat, roughly every second, which sounds like a buffering problem. Both
are in `~/Music/astar Recordings/` on Rob's Mac. They aren't committed, because
they hold other people's audio.

- `2026-11-01 15-57-14 EchoLink 592473.flac` is the **good** one (a voice with
  FM noise in it).
- `2026-11-01 16-03-39 EchoLink 592473.flac` has the **beat**.

Things to check:

- EchoLink frames carry four GSM 06.10 frames, 80 ms of audio per packet. Find
  out whether the recorder's receive tap writes per packet or per playback
  callback, and whether a late or duplicated packet leaves a gap or a repeat.
- How the jitter buffer's output and the recorder's clock relate on EchoLink,
  whose packets carry no sender clock the way IAX2's do.
- A gap or a doubled block at a fixed interval in the bad file. Its spectrogram
  and the sample-level timing between bursts should show which.
- Whether the receive and transmit channels in the stereo layout drift
  against each other.

Write a test that feeds the recorder EchoLink-shaped packets with realistic
jitter and checks the output has no gaps and no repeats.

**Fixed on `work/rec-echolink-beat` (2026-11-01), pending Rob's listen.**

What the bad file shows: both files are 16 kHz stereo, with the far station
on the left and the right channel all zeros. In the bad file the left
channel is exactly 1280 samples of voice (80 ms, one EchoLink packet), then
84 zero samples, then 1280 of voice, then 511 zeros, over and over: 101 gaps
of each, a 197 ms cycle for every 160 ms of audio, and 4.2 s of digital
silence in 21.2 s. About 30 times, every 0.63 to 0.83 s, the pattern breaks
with a short segment or a 117 ms one. The good file has no zero run
longer than 17 samples during the voice.

The cause: the heard tap at the end of `OutputBus::read` copied the whole
buffer it was handed, zeroed tail included. But `fill_output` asks for more
than one callback needs (the callback's share at the bus rate plus 512
samples of headroom on a resampled device) and passes only the first `n`
samples to the device. So the tail is request headroom that nobody heard.
An EchoLink lane is bare PCM with no jitter buffer, and four frames land at
once, so it hovers near empty and most reads come up short. Each short read
put its unplayed tail into the recording: 511 or 84 zeros a packet at
48 kHz with 512-frame callbacks, which is the pattern above. That made the
receive tap run about 23% faster than real time. The recorder's aligner
then kept hitting its 200 ms backlog cap and throwing 140 ms of queued audio
away. At 23% too fast, 140 ms builds up every 0.61 s, which matches the
breaks in the file and the beat Rob heard. The speaker never had
the gaps. IAX2 lanes are filled to the full request by the jitter buffer,
so they were rarely short. The good recording most likely had a standing
cushion in the lane, so no read came up short, but that part isn't
verified.

The fix: the tap offers only `out[..n]`, and nothing on a dry read. The
aligner already fills real underruns and the quiet between overs with
silence on its own 20 ms clock. Tests:
`stream::heard_tap_timing::an_echolink_call_records_without_gaps_or_repeats_*`
drive the real `Mixer`, `OutputBus`, `fill_output` and resampler with 80 ms
packets (no jitter, and up to 30 ms) on 48 kHz/512, 48 kHz/480, 44.1 kHz/512
and 16 kHz/320 devices. Each run feeds the heard tap through the recorder's
`Aligner`, then asserts three things: the tap equals what the device was
fed, the recording has no gap the speaker didn't play, and nothing in it
was lost or repeated. Both tests failed before the fix (the tap carried
4.1 to 5.8 s of extra audio for 6 s of talk) and pass after it. The same
bug would have hit any bare-PCM lane (M17, D-Star, DMR, YSF, NXDN) on a
resampled device.

Still to do: Rob records an EchoLink call and listens.

### astar-el-hollow — EchoLink receive audio turns hollow and reverby during a call (delay creep fixed; hollowness unconfirmed)
*P2 medium · bug · labels: echolink, audio, cx:3*

Rob (2026-11-02): on an EchoLink call in the macOS app the received audio
develops a hollow, reverby sound. It isn't a growing delay, his own voice
coming back, or discrete repeats. It gets worse as the call goes on, and
hanging up and calling again clears it. A sound like that usually means
comb filtering: the audio added to a copy of itself a few milliseconds
later, with the gap between the two copies growing.

**What has been ruled out (work/el-echo, 2026-11-02).** In the engine, the
received stream reaches the speaker exactly once.

- The test
  `session::tests::echolink_wiring::echolink_receive_reaches_the_speaker_exactly_once`
  drives the real wiring on a 16 kHz station: the link thread on 127.0.0.1,
  the GSM decoder, the 8 to 16 kHz rate bridge, the mixer lane and the output
  bus. It sends 80 ms packets of non-periodic audio and checks what the bus
  fed the device against the same frames decoded offline. They match sample
  for sample. Making the link send each frame twice makes the test fail (it
  sees 64,000 samples where 32,000 were received). A 10 s version of the
  same run also matched exactly.
- The rate bridge (`RateBridge`, 8 to 16 kHz) stays exactly 320 samples out
  for each 160 in, with a constant delay and an empty FIFO over 30,000
  frames (10 minutes).
- The bus-to-device path (`Mixer`, `OutputBus`, `fill_output`, resampler)
  plays every sample once and in order (`stream::heard_tap_timing`). The
  recording tap only reads the audio and can't change what is played.
- There is no second voice route or monitor lane. `open_voice_route`
  refuses a second one, the Swift `connectEchoLink` calls
  `echolinkConnect` once per dial, and EchoLink's sequence-number gate
  drops a duplicate packet.
- Rob's two recordings from 2026-11-01 (22 s each, made before the symptom)
  show no stable autocorrelation peak outside the pitch range (r of 0.1 or
  less), so they hold no delayed copy of the audio.

**The delay creep, fixed (work/el-echo, 2026-11-02).** An EchoLink lane was
plain PCM with no jitter buffer, so its queue never shrank while the far end
kept sending: each late packet added its lateness to the delay for good, and
a sender clock faster than the sound card's added to it as well. A busy
conference or repeater, which is what Rob was on, never pauses long enough
for it to empty. Three changes:

- `echolink/rx_clock.rs` (`RxTimeline`) stamps every received frame on a
  sender clock built from the packet sequence number (80 ms a packet, 20 ms a
  frame), anchored afresh at each over. EchoLink's RTP timestamp field cannot
  be relied on (astar itself sends zero). The station mixer then plays the
  lane out of the same adaptive jitter buffer an AllStarLink call uses, with
  the same switch and Min/Max. The 16 kHz rate bridge now carries the stamp
  through (`voice_route::a_16k_bridge_keeps_the_sender_clock`).
- The jitter buffer's port had dropped `jitterbuf.c`'s "shrink one frame per
  500 ms while frames keep coming" branch (`iax-9c62`, noted in
  `harness/jitter_parity/README.md`). Without it a stream that never pauses
  could only deepen. Restored; pinned by
  `jitter::tests::a_fast_sender_that_never_pauses_is_shrunk_by_dropping_frames`.
  This changes AllStarLink playout too, towards Asterisk's own behaviour.
- After a stall `jitterbuf.c` resumes on the oldest frame it holds and
  expects the caller to drain the backlog at once; the mixer plays at the
  device's pace, so the stall's length stayed as delay for the rest of the
  call. The mixer now skips frames more than 60 ms (plus one callback)
  overdue (`OVERDUE_MS`; `mixer::tests::a_stall_and_a_burst_do_not_leave_the_lane_behind`:
  600 ms behind without it, within 310 ms with it). Also affects AllStarLink.
  Skipped and shrink-dropped frames are counted in `rx_frames_lost`.

The simulation (`echolink::rx_clock::tests`, 10 minutes of back-to-back
80 ms packets, 30 to 70 ms of network delay, a 250 ms spike every ~32 s and
one 600 ms outage at minute 3) ends 562 to 719 ms behind on the old path
(±200 ppm and exact sender) and 182 to 220 ms behind with the fix, never
repeating or reordering a frame and dropping 0.4 to 0.5% of them. The
EchoLink call now fills `rx_jitter_ms`, `rx_jb_depth_ms`, `rx_frames_*` and
`rx_underruns`, and the macOS call-quality line shows on EchoLink calls.

**Still unproven.** Whether this is what Rob heard. A growing delay on its
own does not sound hollow; it would if a second copy of the same audio was
also reaching him, and the code has no such path (below). The other
explanations stay open: comb filtering added after the app (the USB
interface's own hardware monitor or sidetone, macOS input monitoring, a
radio on the same net), or present in the far end's audio. Not changed: the
node leg (astar-server conference members) still hands the conference bare
frames; the conference mixes on its own tick.

**USB radio interface, ruled out in code.** No path plays captured audio, or
astar's own output, back to the speaker, or replays it to EchoLink: there is
no monitor, sidetone or local-radio lane (every output-bus lane is network
RX); the mic lane's only destination is the call's TX channel; the record and
analyzer taps only read; `set_echo` is the test fake's; and the EchoLink link
drains captured frames whenever PTT is not applied. One gap found, which is
about transmitting, not hearing: on macOS a serial PTT source's COS (CTS)
edge keys EchoLink through `pttSourceTick` with no half-duplex "not while
receiving" check, unlike VOX (`CallSession.swift` ~1491), and the link does
not refuse TX while the far end talks. Whether COS should be blocked while
receiving is Rob's call (on a radio link it may be wanted).

**What we need from Rob.**

1. Listen to a busy conference for 10+ minutes on a build with this fix. The
   delay should stay put, and the call-quality line should show a buffer
   between Min and Max with `lost` rising slowly. If it still goes hollow,
   record the call (the recording captures exactly what the speaker was fed):
   a comb in the file is in the engine or the far end, a clean file means it
   is added after the app.
2. Check the USB interface for a hardware monitor/sidetone (mixer setting,
   Audio MIDI Setup "Play Through"), and whether a radio nearby carries the
   same net.
3. Whether RX compression is on.

### astar-feature-matrix — DONE 2026-11-01: builds outside `just ci`'s feature set didn't compile or pass
*P3 low · chore · labels: ci, build, cli, cx:2*

**Closed** on `work/feature-matrix`. `just ci` builds the workspace once, and
Cargo unifies features across a workspace build, so a crate that forgot to ask
for a feature it uses still compiled there because a neighbour asked for it.
Each break below was of that shape, or in code only an opt-in feature compiles:

- **astar-cli `dstar`/`ysf`/`nxdn`/`dmr`** called `Station::new` (the real
  `CpalBackend`, gated on `astar-station/device-audio`) while depending on
  astar-station with `default-features = false`. A listen tool plays to the
  default output device, so each listen feature now turns on
  `astar-station/device-audio` (no new dependency: astar-cli already links
  cpal through astar-audio). Eight `assert!(x.is_empty())` in those files'
  tests tripped clippy's `assert_is_empty`; now `assert_eq!`.
- **astar-console `serde`** didn't forward to `astar-audio/serde`, so
  `DenoiseStatus` had no `Serialize` built alone. It does now.
- **astar-server without `stream`**: `/stream.ws` answers 503 "Live audio
  isn't built into this server" there, but the golden fixtures
  `ws_no_upgrade_400`, `ws_full_503` and `public_stream_429_fixture` assumed
  the stream path. They are `stream`-only now, and a no-`stream` build checks
  a new `ws_not_built_503` fixture instead.
- **D-Star flake** `losing_the_link_while_keyed_forces_an_unkey_and_stops_transmitting`
  (4 in 100 runs): `key_up` publishes `ptt = false` before the flush and the
  EOT, on purpose (a UI must not show "keyed" during the flush), and the test
  read the last frame the instant `ptt` dropped. It now waits, bounded, for
  the EOT frame. 0 in 200 idle and 0 in 100 under CPU load after.
- Found by the matrix: astar-station's no-`echolink`/no-`m17` tests and
  astar-sys's no-`echolink` FFI test (`similar_names`) failed clippy.

`just feature-matrix` (`ci/feature-matrix.sh`, and the `feature-matrix` CI
job) runs 103 steps: clippy per crate with `--all-features` and with each
feature alone (features read from `cargo metadata`, so new ones join by
themselves), full tests for astar-cli, astar-console and astar-server under
`--all-features` and for astar-server without `stream`, and astar-server's
`--no-default-features --features release`. Exclusions and their reasons are
in the script header. Not in `just ci`.

Left over: `m17_session_connects_via_localhost_to_a_dual_stack_reflector`
(astar-console `tests/m17_session.rs`) failed once in a matrix run, its v4
client's 200 ms read timing out waiting for ACKN. It did not recur in 470
runs (alone, whole binary, six binaries at once under CPU load), so its cause
is unproven; see `astar-m17-dualstack-flake`.

### astar-m17-dualstack-flake — M17 dual-stack test timed out once
*P3 low · bug · labels: m17, test, flaky, cx:1*

`m17_session_connects_via_localhost_to_a_dual_stack_reflector` failed once on
2026-11-01 during `just feature-matrix` (`tests astar-console
--all-features`): "v4 client must be ACKN'd by the same dual-stack reflector:
WouldBlock", the far v4 client's 200 ms `recv_from` getting nothing. Not
reproduced in 470 runs afterwards. Two candidates, neither shown: a stall
longer than 200 ms (the reflector replies to CONN at once, and the rest of the
test allows 2 s), or a parallel test's `127.0.0.1:0` socket sharing the
dual-stack reflector's port on macOS and taking the v4 datagram. Next time it
fails, keep the run's output and check which.

### astar-acme-ca-change — switching ACME from staging to production keeps the staging certificate
*P3 low · bug · labels: server, acme, tls, cx:1*

Found 2026-11-02 while preparing the node's public page. astar-server keeps a
stored certificate whichever CA issued it, and renews only 30 days before
expiry or when the hostnames change. So turning `acme_staging` from true to
false keeps serving the untrusted staging certificate for about two months.
`issued.json`, astar's own count of three orders a week, also counts staging
orders against production.

The gh-runners role works around it by moving `state/acme/cert-bundle.json`,
`issued.json` and the legacy certificate files aside when the CA changes.
The fix belongs in astar: record which CA issued the stored certificate,
order a new one when `acme_staging` (or the directory URL) no longer matches,
and keep the weekly count per CA. A test pins it: issue against a staging
fake, flip the setting, and expect a fresh order on start.

### astar-glib-unsound — glib 0.18 is unsound (Dependabot alert #2), Linux client only
*P3 low · security · labels: deps, gui, linux, cx:1*

Dependabot alert #2: `glib` 0.18's `VariantStrIter` is unsound; the fix is in
`glib` 0.20. It reaches the workspace only through `tray-icon` → `gtk` 0.18 in
the Iced client (`apps/gui`), which is paused and not published, so nothing
astar ships today contains it. The macOS app and `astar-server` do not link
it. The fix is a `tray-icon` release built on gtk-rs 0.20+, or dropping the
tray icon on Linux. It goes away with `astar-uiclone` if that lands first.
Until then, dismissing the alert as "vulnerable code not used" is accurate.

### iax-c7e1 — Finish removing the CH34x dext: ONE HARDWARE STEP LEFT
*P1 high · task · labels: ptt, hardware, macos, docs, cx:1*

BLOCKS GOING PUBLIC (Rob, 2026-08-10): "before we make this public, I want to
remove the serial driver."

STATUS 2026-08-11 — steps 1 and 3 are DONE; only step 2 (Rob's hardware) is
left, and it no longer blocks the publish:

  - **Step 1 done.** `SerialConfig.transport` now defaults to `.usb`
    (`bindings/swift-serial/.../SerialClient.swift`). All three fallback sites
    inherit that single default, so `SerialLineSpec+Config` and
    `SerialSettings` needed only their comments corrected. Decision on existing
    configs: a config that NAMES a transport keeps it — nobody is migrated off
    `.tty` — only absent/unmapped falls through to `.usb`.
    `testDefaultTransportIsUsb` pins it. The Rust FFI doc was corrected too: it
    no longer calls `Tty` "the default", it calls it the zero value and warns
    that zero-init lands there. The C enum was NOT renumbered — that is an ABI
    break, and `Transport`'s Swift raw values mirror it.
  - **Step 3 done.** The cask is gone from the first-run path everywhere: the
    in-app onboarding checklist (`SerialView.swift`) no longer mentions a
    driver, `docs/site/macos/hardware.md` presents raw USB as the default and
    the tty path as a manual opt-in, and README + CLAUDE.md/AGENTS.md say the
    same. `guard-distribution-claims.sh` still allows the one cask because the
    tty path legitimately needs it.
  - **Step 2 still open** — see below. It is a verification, not a code change,
    and the shipped default no longer depends on its outcome.

Corrected state — an earlier version of this item claimed the dext was still
required. That was read off the CODE default and was wrong in practice:

  - astar's UCI150 hardware preset ALREADY selects raw-USB
    (`HardwareProfile.swift`: `transportRaw: 1  // raw-USB`, astar-f772).
  - Rob's own persisted config is ALREADY on it: `defaults read` shows
    `"serial.transport" = 1`.
  - So the running client does not use the tty path, and the dext
    (`cn.wch.CH34xVCPDriver 1.0/1`, at
    `/Library/SystemExtensions/697C0C39-.../`) is installed but unused by
    astar. `/dev/cu.wchusbserial5B210098201` exists only because the dext
    publishes it.

What is actually left:
  1. Flip the FALLBACK default from `.tty` to `.usb` so a fresh install never
     touches the tty path. Three places keep `.tty` for back-compatibility:
     `SerialClient.swift` (`var transport: Transport = .tty`),
     `SerialLineSpec+Config.swift` (nil/unmapped → .tty),
     `SerialSettings.swift` (absent persisted key → .tty). The Rust FFI
     (`astar-serial-sys/src/ffi.rs`) documents `Tty` as its default too.
     Decide whether existing `.tty` configs should be migrated or left alone.
  2. Uninstall the dext and VERIFY PTT still keys over raw-USB. This matters
     because `uci150_usb.rs`'s own module doc says "only the WCH *dext* must
     be absent" — yet Rob has been running raw-USB WITH the dext present. One
     of those two is wrong, and which one decides whether the uninstall is a
     no-op or a fix. Rob's hardware, Rob keys.
  3. Strip the CH34x cask from install docs, CLAUDE.md, and the new monorepo.

CAVEAT before uninstalling: the dext serves ALL WCH parts, not just the
UCI150's CH343. CH340/CH341 cables — the common radio-programming cable — are
not CDC-ACM compliant and need this vendor driver. Removing it may break
CHIRP-style programming cables. The CH343 in the UCI150 is CDC-ACM capable,
which is why raw-USB works for it. This is a per-machine trade, not a
consequence of astar's code.

### iax-3f7a — Flaky: m17_session dual-stack reflector test fails under parallel load
*P2 medium · bug · labels: test, m17, flaky, cx:2*

Observed 2026-08-11 during a `just ci-full` run on the Mac:

```
m17_session_connects_via_localhost_to_a_dual_stack_reflector
  panicked at crates/astar-console/tests/m17_session.rs:677:
  v4 client must be ACKN'd by the same dual-stack reflector:
  Os { code: 35, kind: WouldBlock, message: "Resource temporarily unavailable" }
```

Reproduction rate: **intermittent**. The same test passed 3/3 when run alone
immediately afterwards, and the full gate had passed three times earlier in the
same session. It fails only inside a full `cargo test --workspace` run, i.e.
under parallel load.

EAGAIN/`WouldBlock` on a UDP loopback read is a read timeout being hit, not a
protocol failure — the reflector's ACKN almost certainly arrived late rather
than never, because the machine was busy compiling and running the rest of the
workspace. So the bug is in the test's timing assumption, not in `astar-m17`.

Fix direction: give the receive a bounded retry loop against a deadline instead
of a single read with a fixed timeout, so a late ACKN still passes and a truly
missing one still fails. Check whether the other loopback/parrot tests in that
file share the same pattern — if so, fix them together.

Why it matters now: this is exactly the failure that will turn a public repo's
CI red at random and teach everyone to re-run the pipeline instead of reading
it.

### iax-9e4c — Confirm the hot-unplug hang fix against real hardware
*P1 high · task · labels: audio, hardware, cx:1*

iax-8d21 bounded every wait between a caller and the cpal stream thread,
which by code inspection accounts for all three symptoms Rob reported with
the UCI150 (dial hangs, Quit hangs, UI still paints). The mechanism has NOT
been confirmed against the physical failure — no sample of the hung process
was captured before the fix went in.

Confirm: unplug the UCI150 mid-session, replug, dial. Expect a dial that
fails within ~5s with a "disabled" device error instead of hanging, and a
Quit that always completes. If it still hangs, capture `sample astar 5` and
find which thread is blocked — the wedge may be somewhere the four bounded
waits don't cover (the serial worker, or CoreAudio enumeration inside
`CpalBackend::enumerate`, which is still unbounded by design since it does
not cross a thread boundary).

### iax-b8c1 — Surface vocoder TX health in the D-Star snapshot
*P2 medium · task · labels: dstar, ambe, cx:1*

Fallout from iax-2f6b (review minor 11, only partially fixed). If the
`HwAmbeStream` worker thread is gone, `submit_encode` now logs a warning but
still silently drops every frame handed to it: the operator sees PTT engage,
hears their own sidetone-free silence, and transmits nothing. The same hole
exists on the RX submit path.

Fix: add a health/`tx_capable` signal to `DstarSnapshotState` (and whatever
the RX side needs to match), fed by the worker's liveness, so a UI can grey
out PTT rather than offering a key that cannot produce audio. This is a
snapshot-contract change — new field, new semantics for something astar
reads to decide whether to offer PTT — so it wants its own design pass
rather than being bolted onto a review-fix round.

### iax-d9f4 — Enforce (not just document) the astar-server D-Star keying hazard
*P2 medium · task · labels: dstar, safety, cx:1*

`Station::set_ptt` is the single entry point that can key a transmitter and
is network-agnostic by design. `astar-server` exposes it remotely via
`POST /key` and a TUI keystroke. Today its `astar-station` dependency
takes default features only (no `dstar`), so D-Star cannot be keyed through
it — but Cargo feature unification means any workspace build that enables
`astar-station/dstar` for another crate turns `POST /key` into a remote
D-Star transmit trigger. iax-2f6b documented this in `station.rs` and
`astar-server/Cargo.toml`; a comment is not a guard.

Fix: make it structural. Options worth weighing — a runtime refusal in the
node's key path when the active session is D-Star, a `compile_error!`
tripwire, or splitting the remote-keyable surface away from `Station`.
Practical exposure is currently low (the node has no path to
`dstar_connect`, so no D-Star session can be active there), which is why
this is P2 and not urgent — but the hazard should not depend on that
staying true.

### iax-e5b2 — TX RPT1/RPT2 derivation unverified against a live reflector
*P2 medium · task · labels: dstar, protocol, cx:1*

iax-2f6b fills the TX RF header's repeater fields as `RPT2 = "<REF>
<module>"` (destination) and `RPT1 = "<REF> G"` (gateway), derived from the
host's first DNS label when `DstarConfig::reflector_callsign` is `None`.
That shape matches the captured XLX458 header and what xlxd's DPlus
`IsValidModule(rpt2.GetModule())` gates on, but no transmission has been
made to a live reflector to confirm it is accepted and attributed
correctly.

Since then the derivation itself was fixed and plumbed: an `XLX###`
reflector is addressed by its DExtra `XRF###` alias (the form the XLX458
capture carries), the suffix may be alphanumeric (`XLXARG`), and the
destination callsign now reaches `Station::dstar_connect` and the C-ABI's
`iax_station_connect_dstar` as an optional argument — so a bare-IP connect
can name the reflector instead of transmitting blank fields. What remains
open here is only the on-air confirmation.

Verify on air (Rob keys, never an agent): confirm the reflector dashboard
shows the transmission attributed to AJ7HR on the right module. Record the
result in `docs/research/research-dstar.md` §8 alongside the RX findings.

### iax-3a5c — Loopback reflector should mimic real reflector laxity (zero header CRC)
*P2 medium · task · labels: dstar, test, cx:1*

Lesson from iax-7c19. Our loopback/parrot `Reflector` in astar-dstar
computes a correct RF-header CRC when it replays a stream. Real reflectors
do not: XLX458 (KC-Wide) sends `0x0000` in that field on every forwarded
header. Because our fixture was politer than reality, all 52 protocol tests
passed while the client could not decode a single live transmission — the
bug only surfaced on air.

Fix: have the parrot/relay path forward headers with a zeroed CRC by
default (optionally a flag to compute one), so the fixture reproduces the
peer behaviour clients actually meet. Add a regression test asserting the
replayed header's CRC trailer is zero and that the client still accepts it.
Generalise where cheap: a test reflector should reproduce real peers'
laxity, not its own idea of correctness.

### iax-c4f2 — D-Star reflector daemon (M1)
*P2 medium · feature · labels: dstar, reflector, cx:2*

Promote `astar-dstar`'s loopback/parrot `Reflector` (test-only today)
into a runnable daemon: a binary with host/port/module args and a `just
dstar-reflector` recipe, mirroring the M17 mrefd parrot dev loop. Gives
astar and `dstar-listen` a local reflector to test against without touching
live networks. Blocked on nothing; spec when picked up. Roadmap context:
`../astar/docs/superpowers/specs/2026-08-08-thumbdv-realtime-dstar-design.md`.

### iax-d5a1 — D-Star reflector: multi-client, multi-module (M2)
*P2 medium · feature · labels: dstar, reflector, cx:3*

Grow the M1 daemon (iax-c4f2) into a real linkable reflector: several
clients per module, modules A-Z, link/callsign policy, status output,
eviction and capacity limits. Spec when picked up. Roadmap context:
`../astar/docs/superpowers/specs/2026-08-08-thumbdv-realtime-dstar-design.md`.

### iax-e7c3 — D-Star transcoding bridge via ThumbDV (M3)
*P2 medium · feature · labels: dstar, reflector, thumbdv, m17, cx:4*

The ThumbDV payoff: bridge D-Star into M17/AllStar — ThumbDV decodes AMBE,
the engine re-encodes to Codec 2 / G.711, and back. One dongle suffices
because D-Star conferences are half-duplex (one talker per module), BUT the
budget is tight: pipelined decode plus encode is ~15 ms of the 20 ms frame,
so one bridged stream per dongle is the design limit. M3 must measure this
before committing to a shape. Blocked on iax-b3e7 (pipelined vocoder) and
iax-d5a1. Roadmap context:
`../astar/docs/superpowers/specs/2026-08-08-thumbdv-realtime-dstar-design.md`.

### iax-5562 — Unbuffered SSE: flush /events per frame (status page lags ~3 s)
*P1 high · bug · labels: node, http, cx:2*

Found during iax-24e2's Task 5 smoke and confirmed by its final review:
`tiny_http`'s default `chunked_transfer::Encoder` (8192-byte buffer, no
per-write flush) batches the 33 ms / ~90-byte SSE heartbeat frames on
`GET /events`, so browsers see ~3 s of blank page before the first live
update and every event (including the `rx_active` "talking" pill) lags up
to ~3 s behind the wire. Predates iax-24e2 (the SSE endpoint shipped with
iax-d829.1); the status page is its first real consumer. Fix shape (from
the Task 5 diagnosis): serve `/events` in `astar-server`'s server layer
via `Request::into_writer()` / `upgrade()` writing `sse_frame` output with
an explicit `.flush()` per frame, instead of wrapping `SseReader` in
`tiny_http::Response`'s `Read` adapter. Test: bound-server integration
test asserting the first `data:` frame arrives within a sub-second
deadline. Immediate next item after iax-24e2 per its final review ruling
(the buffering undercuts the live premise of the status page). Detail in
git history: iax-24e2 Task 5 report (SDD workspace, now deleted).

### iax-c1d5 — Error transparency: carry engine error detail across the ABI; fix lossy M17 error mapping
*P2 med · task · labels: ffi, ux, cx:3*

Rob 2026-08-04, after a useless "astarstation error -8: iax error" from an
M17 localhost dial: (1) the engine's rich error detail (connection refused,
resolve failure with hostname, dial-stage context) never crosses the C ABI —
only the numeric code does. Add a last-error-text accessor
(`iax_station_last_error_text(st, buf, cap)` using the fill_buf pattern;
secret-free by construction) populated on every failed call, plus Swift/
Python surfacing, so UIs can show the real reason. (2) Fix the lossy M17
error mapping found reproducing this: an M17 connect resolve failure maps to
`StationError::Iax("resolve")` (-8) instead of `Resolve` (-6) — audit
`map_console_err`/the m17 arms so every ConsoleError variant maps to its
matching StationError. Pairs with astar-0217 (friendly connect-failure
messages), which should consume the new text. Also fold in (review-parked 2026-08-04): the m17 connect candidate-iteration loop discards the real io::Error on the all-candidates-fail path (m17.rs connect_udp_socket), synthesizing a misleading could-not-resolve message — surface the last real failure instead.

PARTIALLY ADDRESSED by the node's M17 monitor work: the fold-in is fixed.
`connect_udp_socket` (now in `crates/astar-console/src/m17/rx.rs`) returns
the resolver's own error, else the last real bind/connect error, instead of a
synthesized "could not resolve", and the monitor reports it as
`could not resolve <host>: <error>`. Still open: (1) the last-error-text
accessor across the C ABI, and (2) the `ConsoleError::Resolve` →
`StationError::Iax("resolve")` mapping in `map_console_err`.

### iax-a4e7 — Output AGC: auto gain for quiet stations on RX
*P2 med · feature · labels: audio, dsp, cx:3*

Rob 2026-08-03, live on M17-KCW: one participant far quieter than the rest —
wants automatic gain on the OUTPUT/RX path so quiet stations get lifted
toward a target level (slow attack/release AGC or loudness normalization,
per-bus in the AudioRouter output path, network-agnostic so AllStar + M17
both benefit). Surface an on/off + target-level knob through
Station/FFI/bindings; default OFF (byte-identical rule).

PHASE 1 (Rob 2026-08-04, do first) SHIPPED 2026-08-04 on the Rust/FFI/Swift
side: reuses the mic-path `Compressor` (makeup gain included) on the
`OutputBus`, applied BEFORE the bus gain multiply in `OutputBus::read` so the
4x output-gain range amplifies the leveled signal — `rx_compress`/
`rx_compress_level` threaded AudioRouter → Manager → ConsoleSession (standing
prefs, re-pushed on (re)connect, now a 10-pref re-push) → M17Session (prefs +
live setters + `applied_rx_compress`/`applied_rx_compress_level` readback) →
Station (`set_rx_compression`/`set_rx_compression_level`) → FFI
(`iax_station_set_rx_compression`/`_level`) → Swift
(`setRxCompression`/`setRxCompressionLevel`). Default OFF everywhere
(byte-identical rule verified by a router-level regression test). Python
bindings intentionally NOT extended (matches existing lockstep gap: Python
already lacks `tx_trim`/`vox_preroll_ms`/`spectrum_decay` setters too).
STILL OPEN from Phase 1: Quick settings UI on both platforms (astar's
`QuickConfigView.swift` speaker card, gui-rs's `speaker_card` in
`gui-rs/src/view/config.rs`) — the Rust/FFI/bindings plumbing is ready for it
but no UI wiring was done (out of scope for the engine-only task that shipped
this). PHASE 2 (original ask): full auto-target AGC. Design pass needed
(attack/release constants, clipping guard, interaction with the existing
output gain + half-duplex mute).

RE-RAISED by Rob 2026-08-08, same symptom on M17/KC-Wide ("some signals are
several dB lower than what should be transmitted"), which confirms Phase 1's
fixed-ratio compressor is not enough on its own — it lifts a quiet talker
only within its threshold/makeup envelope and does not converge different
talkers onto a common target. Phase 1's UI has since landed on astar
(QuickConfigView.swift:159 "RX compression" toggle + level slider), so the
interim mitigation is available; Phase 2 is what actually equalises talker-
to-talker loudness. Wants its own brainstorm/spec cycle: per-talker vs
per-bus adaptation (M17 stream boundaries give natural per-talker resets),
target loudness units (LUFS-style vs peak), attack/release, clipping guard,
and how it composes with rx_compress rather than fighting it.

DATA POINT (Rob, 2026-08-09, listening to a live KC-Wide QSO on D-Star via
the ThumbDV): D-Star RX "sounded really close to M17" on the same network.
The D-Star decode path has NO compressor or gain stage in front of it at
all — raw AMBE out of the vocoder straight to the output bus — so a mode
that is entirely unprocessed lands at about the same perceived level as the
processed M17 path. That argues the problem Phase 2 must solve is
TALKER-TO-TALKER variance, not a per-mode level offset, and that a
per-talker adapting target (reset on stream boundaries, which both M17 and
D-Star provide cleanly) is the right shape. It also means D-Star is a
useful control when evaluating any AGC: unprocessed audio on the same
network, same listener, same session.

### iax-e2c8 — M17 client hardening (post-KC-Wide follow-ups)
*P3 low · task · labels: m17, cx:3*

Consolidated from the iax-f2b8 final review + task-review deferrals: (1)
RX jitter/reorder stage (JitterBuf<[u8;16]> or minimal drop-older-than-
last-played guard) — top item before heavy real-Internet use; (2) TX burst
pacing (~50 ms recv-blocked bursts vs smooth 40 ms cadence); (3)
`encode_callsign` post-uppercase length/overflow guards (ß→SS expansion);
(4) `M17Config::default` / public DEFAULT_KEEPALIVE const; (5) evaluate two
codec instances for duplex (likely fine as-is); (6) mirror `input_level_db`
while M17 active so VOX can drive M17 PTT (astar UI half rides
astar-6f1b); (7) distinguish NACK-refused vs link-timeout in status text;
(8) key-while-Connecting sends pre-link stream packets (note); (9) m17 teardown joins the run-loop thread under the session lock on the disconnect hot path (bounded ~50-100 ms; split off-lock like the IAX2 detach/hangup convention — parked from the fix-wave re-review 2026-08-03).

Item (1) is now user-visible: astar-server's M17 monitor
(`crates/astar-console/src/m17/monitor.rs`) shares the client's receive path,
so a Discord channel bridged to a reflector hears late and out-of-order
packets in the order they arrive, with no reorder stage there either. The
operator docs say so. Item (7) is done for the monitor only: its end reason
tells `reflector refused the link` apart from `no reply for 30 s`, while
`M17Session` still does not. (10) adopt `StreamGate`
(`crates/astar-console/src/m17/rx.rs`, the monitor's flood guard: one stream
at a time, paced to real time with at most 200 ms lead) in `M17Session`'s
receive path, which today decodes every stream packet the reflector relays.

### iax-d3b6 — M17 reflector daemon hardening
*P4 backlog · task · labels: m17, cx:3*

The in-engine reflector is test-harness-grade (spec §7 scope-out). Before
any standalone daemon use: configurable reflector callsign (hard-coded
"M17-REF"), re-CONN liveness edge, single-parse of control packets, the two
racy client_count test asserts, dashboard/logging, LSTN (listen-only)
support. (The stale "(no I/O)" Cargo.toml description was fixed alongside
iax-91f4's parrot mode.)

### iax-b5e1 — astarserial.h cbindgen drift (pre-existing, iax-239a era)
*P3 low · bug · labels: serial, cx:1*

Found during iax-f2b8 Task 5: `check-cbindgen-serial` is red on master —
`astar-serial-sys`'s committed header has drifted from its Rust source
since the iax-239a worker-thread work. Regen, diff-audit (ABI-affecting or
comment-only?), and add the serial check to the ci recipe if it isn't there.

### iax-9a3c — Verifiable builds: prove a released binary comes from the published source
*P3 low · task · labels: build, release, licensing, cx:4*

Rob 2026-08-17, deferred deliberately from the 0.1.4beta licensing work. Goal:
someone holding a released astar binary should be able to rebuild it from the
tagged source and confirm they got the same thing. Serves LGPL-2.1 s6's relink
requirement (see `LICENSE-EXCEPTIONS.md`) and is plain good practice for a
signed binary people install.

Rob also wants the **static Codec 2 library and its source shipped as release
assets**, alongside astar's source, not just referenced.

ALREADY TRUE — do not redo this:

* Codec 2 is vendored at `vendor/codec2` (`vendor-codec2`, 2026-10-12), so the
  exact library source is in the tagged tree itself. Its `VENDORED.md` records
  the crates.io checksum it was taken from
  (`3c3c8ba70382a99c0b5ebadf6ad50e7d98fb6eb9bcc4c0e30d422f6321798d99`); a path
  dependency has no checksum in `Cargo.lock`, so that file is the link now.
* Every release is tagged, so `git checkout v<version>` yields the exact tree.
* MEASURED 2026-08-17: the `codec2` rlib built `--release` is **byte-identical
  across two separate `CARGO_TARGET_DIR`s** on one machine
  (`9fb6566117d5c5e234daf78d7ce8c9618d4899739e5b4800777b5e8fa1070480`). So the
  Rust side is already deterministic under the easy conditions.

OPEN QUESTIONS, in the order worth answering:

1. **Different source path.** Untested — this is the case a third-party verifier
   actually has. Absolute paths leak into artifacts via debug info and panic
   messages; `--remap-path-prefix` (or `-Z trim-paths` / the stable
   `trim-paths` profile option) is the mitigation. Test before promising
   anything.
2. **The toolchain is not pinned.** `rust-toolchain.toml` says
   `channel = "stable"`, which floats — two people on different rustc releases
   will not match. Reproducibility needs an exact version (`1.97.1` today).
   Weigh against the cost of manually bumping it.
3. **What can honestly be claimed about the signed artifact.** A notarized
   Mach-O cannot be byte-compared against a local rebuild: signing rewrites
   `__LINKEDIT`, and the App Store re-signs on top. The achievable claim is
   about the *unsigned* binary — publish its SHA-256 and a documented
   `just verify-build` that rebuilds and compares. Do not advertise
   "reproducible builds" beyond what is actually true of the delivered file.
4. Release assets to add: source tarball (`git archive` of the tag), the static
   Codec 2 artifact, and a `SHA256SUMS` covering every asset including the DMG.

Related: the App Store exception and the LGPL written offer in
`LICENSE-EXCEPTIONS.md`; `iax-e5d9` for why Codec 2's licence matters on iOS.

### vendor-codec2 — DONE 2026-10-12: Codec 2's source lives in this repository
*P3 low · chore · labels: licensing, codec, m17, vendor, cx:2*

**Closed.** Done in `work/vendor-codec2`. Rob decided 2026-10-12 that the LGPL
source the macOS app links statically (`codec2-static`) should be in astar's
own repo, not only a crates.io reference. The published `codec2` 0.3.1 crate is
vendored at `vendor/codec2`, the same way `vendor/ambe-thumbdv` is an island:
source byte-for-byte upstream, manifest keys hard-coded to upstream's values
(never `.workspace = true`), provenance in its `VENDORED.md`. Upstream ships no
licence file, so astar added the GNU LGPL-2.1 text beside it.

The workspace dependency is now `codec2 = { path = "vendor/codec2", version =
"0.3.1" }`, and the crate is in the workspace `exclude` so `fmt` and `clippy`
never touch it. Feature gating is unchanged: `cargo tree -p astar-codec` shows
no codec2, `--features codec2-static` shows the path copy, and
`ci/guard-codec2-licensing.sh` still holds. `ci/guard-no-git-deps.sh` now also
fails if the vendored crate, its licence text or its hard-coded licence goes
missing. `LICENSE-EXCEPTIONS.md` §2, `README.md` and the site's licence page
say the source ships here.

### iax-e5d9 — Clean-room Rust Codec 2 (permissively licensed) — the iOS M17 gate
*P4 backlog · feature · labels: m17, codec, cx:5*

Ports of libcodec2 (incl. the Rust codec2 crate) are derivative → LGPL, and
LGPL is effectively incompatible with App Store distribution — so M17 on
iOS needs a clean-room Codec 2 implemented from David Rowe's papers/blog
DSP descriptions, NOT the C source, validated by golden vectors +
cross-decode interop against libcodec2 both directions + listening tests.
3200 mode first. Slots in behind iax-f2b8's CodecProvider seam as a drop-in.
Does not block anything else; parallelizable.

### iax-d7e3 — D-Star engine backend: XLX/XRF reflector linking (DExtra/DCS)
*P4 backlog · feature · labels: dstar, protocol, cx:5*

Per Rob 2026-08-03 (astar-d5a7). HARD GATE FIRST: D-Star voice is AMBE —
no freely-licensable software vocoder exists (options: AMBE hardware
dongle via USB, or mbelib which is patent-encumbered/legally gray). The
design must resolve the vocoder question before any protocol work.
Protocol side is tractable: DExtra/DCS linking to XLX/XRF reflectors
(KC-Wide: XLX458/XRF458 module A), behind the CodecProvider + Station
capability pattern from iax-f2b8.

### iax-e8a4 — YSF engine backend: YSFReflector protocol (C4FM)
*P4 backlog · feature · labels: ysf, protocol, cx:5*

Per Rob 2026-08-03 (astar-e7b3). Same AMBE+2 vocoder gate as iax-d7e3 —
resolve first. Protocol: YSFReflector UDP (KC-Wide: US-KCWIDE YSF32453 /
US-XLX458 YSF28054); also the practical bridge into Wires-X rooms (native
Wires-X is closed — see astar-a3c9, which deliberately has no engine item).
CodecProvider + capability pattern from iax-f2b8.

**Recipe:** `docs/design/adding-a-network.md` — the fourteen layers a network
touches, derived from M17 and D-Star. The vocoder is not the expensive layer:
AMBE+2 is a RATEP word on the ThumbDV astar already drives, and the directory
already publishes these reflectors with a `dial.kind`.

**Design:** `docs/design/ysf-network.md`.

**Progress 2026-08-29:** `crates/astar-ysf` is built and green — frame and
FICH coding (Golay 24/12, rate-1/2 K=5 convolutional, interleave, CRC), the
`YSFReflector` datagrams, a client link FSM, and a loopback reflector with a
parrot mode, 62 tests, `std` only. The wire numbers were verified against the
deployed reference implementations rather than recalled; see the design doc's
"Where this stands".

**Progress 2026-09-05 — RECEIVE IS BUILT, end to end.** `astar-codec::ysf`
carries the AMBE+2 DN vocoder (both V/D modes, the RATEP word, the chip's bit
order); `AmbeStream` gained a `VocoderMode` so the ThumbDV is initialized for
half rate and fed 49-bit channel packets; `astar-console::ysf` decodes each
payload's five voice frames onto an output bus; and the Station facade, C ABI,
node key guard, Swift binding and `Network.ysf` are all in. VW and data frames
are refused by name into `unsupported_mode`, which reaches the UI.

**Unverified on hardware.** Bytes in and bytes out are proven; that the audio
is intelligible is not, and cannot be without a ThumbDV on a live reflector
(KC-Wide US-KCWIDE YSF32453, US-XLX458 YSF28054). V/D mode 1 especially:
MMDVMHost gives it its own layout, DroidStar runs it through the mode-2 path,
and astar follows MMDVMHost.

**Remaining: transmit — and it is blocked, not merely unwritten.** See
iax-ysftx below.

### iax-wavcadence — `--wav` captures less audio than the session received
*P3 low · bug · labels: cli, audio, dstar, ysf, cx:3*

Measured 2026-09-06 during the YSF receive checkpoint, on live traffic from
KCWide. A `ysf-listen --wav` run of ~302 s wall clock produced a **226 s** WAV
— a 25% shortfall — and the audio inside it came to 843 non-silent 20 ms
frames where the 208 radio frames counted by a second, link-only client over
the same window predict 1040 (81%).

Two shortfalls of similar size, which is why this is filed against the capture
harness rather than the vocoder: `WavBackend`'s output "device" pulls decoded
audio on a background thread "at the stream's own cadence", and if that
cadence runs slower than real time the file simply contains fewer slots than
the session produced. The decoded audio that IS there looks right — speech
levels, peak -10.0 dBFS, no clipping, talkers matching the `▶` lines.

NOT YSF-specific: `crates/astar-cli/src/wav_backend.rs` is shared with
`dstar-listen`, and this was extracted from it unchanged, so any D-Star `--wav`
capture has the same behaviour and always has.

**Unconfirmed as a defect.** The two clients ran in overlapping but not
identical windows, so the frame comparison is indicative rather than exact.
The cheap way to settle it: capture a known-length transmission into a local
`ysf-parrot`/`m17-parrot` and compare the WAV's duration against the wall
clock, with nothing else running. If the cadence is the cause, the fix is to
drive the writer from a real-time clock rather than the pull loop's own pace.

### iax-ysfdch — SHIPPED 2026-09-06: the in-payload data channel on transmit
*P3 low · feature · labels: ysf, protocol, cx:3*

**Built, and the original guess about why it mattered was too mild.**
Investigated and designed in `docs/design/ysf-dch.md`, which carries the
citations.

The original entry said a zeroed data channel "does not stop a transmission
being heard or attributed" because the `YSFD` routing header carries the
callsigns in clear. That is true of a reflector's own last-heard and false of
everything downstream of it. `MMDVMHost`'s `CYSFControl::writeNetwork` calls
`processNetCallsigns` only when `processHeaderData`/`processVDMode2Data`
return true — which needs a data channel whose CRC checks — and the code that
regenerates the DCH for the air is inside the same `if`, so a zeroed channel
was passed to a Yaesu radio unchanged. `processVDMode2Data` returns
`ret && (fn == 0 || fn == 1)` on top of that, and astar pinned FN and FT at
zero, so even a valid channel would never have been read. And
`pYSFReflector3` opens the stream it relays on a frame whose FICH says
Header — astar's header carried voice where the CSD belongs.

**What shipped.** `astar-ysf`'s new `dch` module: the whitening (derived from
its PN9 definition, and now the single copy `astar-codec` uses too), the
CRC-over-whitened-bytes order, the rate-1/2 convolutional code and dibit
interleave at both widths, `write_vd2`/`read_vd2` for a Communications
frame's ten bytes and `write_csd`/`read_csd` for a Header/Terminator's two
twenty-byte callsign blocks, plus `vd2_dch` and `header_csd` for what a
client puts in each. `Fich` grew `call_sign_path` (CS), which was being sent
as zero where a softclient sends 2. `astar-console`'s transmit path now sends
a real Header frame at key-down, cycles FN 0..=6 with FT 6 filling each
frame's data channel, and closes with a real Terminator carrying callsigns
rather than five frames of silence.

**Known fragility, and it is reference parity.** One Header frame opens an
over, and a reflector will not relay a stream it never opened — so a single
lost `YSFD` datagram at key-down drops the whole transmission. `MMDVMHost`
and `DroidStar` have exactly the same exposure and nobody repeats the header;
worth knowing before blaming the DCH if an over goes missing again.

**What still needs Rob's radio.** Whether a Yaesu handheld actually shows the
callsign, and whether KC-Wide's monitor picks the over up. Everything here is
verified against the references and by round-trip, known-answer and
end-to-end tests; none of that can be the same as a radio's display. Reopen
with what the radio shows.

### iax-ysftx — DONE 2026-09-06: YSF transmit
*P3 low · feature · labels: ysf, vendor, cx:3*

**Closed.** The blocker was real and is gone. `vendor/ambe-thumbdv` is
re-vendored at `41c35d817af3`, where `parse_channel` reports the bit count the
device declared instead of requiring 0x48 — the check moved to
`ThumbDv::encode_frame` and to `astar-codec`'s worker, both of which know what
rate they configured. Encode is mode-aware end to end, and the timeout
substitution is `DnFrame::MUTE` for YSF rather than a D-Star null codeword.

`astar-console::ysf` transmits: the station's voice route (shared with every
digital network, since the one-audio-lane refactor), a PTT request the run
loop applies, AMBE+2 half-rate encode, five frames to a payload,
header/communications/terminator with the end flag set. The payload's data
channel followed in `iax-ysfdch`.

Original text follows.

### iax-ysftx — YSF transmit is blocked at the vendored deframer
*P3 low · feature · labels: ysf, vendor, cx:3*

Found 2026-09-05 while building YSF receive. Receive needed nothing from the
vendored crate — a decode reply is `Response::Speech(pcm)`, which carries no
mode. Transmit does, and cannot have it.

`vendor/ambe-thumbdv/src/packet.rs`'s `parse_channel` rejects any `Channel`
response whose bit count is not `0x48` (72 bits) as "rate lost", and there is
a test in that crate asserting a `0x31` (49-bit) response IS an error. A YSF
encode comes back at 49 bits. So every encode request would go unanswered,
time out, and be substituted with `NULL_AMBE_FRAME` — a **D-Star** null
codeword, which on a YSF link is not silence but nine bytes of the wrong
vocoder, transmitted.

`astar-codec::ambe` refuses `submit_encode` outright on a `YsfDn` stream so
nothing downstream can come to depend on it, and `astar-console::ysf` has no
TX path at all.

**The fix is not a patch in place.** `vendor/ambe-thumbdv` is a verbatim copy
of `rcludwick/ambe` (MIT/Apache), and `VENDORED.md` says so; editing it here
forfeits cheap re-vendoring and diverges someone else's licensed code. The
clean path is upstream, then re-vendor: widen `parse_channel` to accept the
bit count the packet declares (and carry it), rather than assuming 72.

**Also needed once the deframer can read it:** `pack_dn` and
`dn_frame_from_channel` already exist and are tested; the encode side of
`AmbeStream` needs the mode plumbed through the way decode's now is, and
`YsfLink` needs its audio from the station's voice route (as it now gets,
since the one-audio-lane refactor). And the node key guard already refuses
YSF (`key_refusal` in `astar-server`), so remote keying stays refused
whatever lands here.

### iax-d4f7 — DMR engine backend: MMDVM/homebrew, TGIF first
*P4 backlog · feature · labels: dmr, protocol, cx:8*

Per Rob 2026-08-29. DMR is a **family** of independently run networks sharing
one protocol, so the network is part of the address, not a detail of it — a
talkgroup number names nothing on its own (TG 91 exists on several of them and
means something different on each).

**Design:** `docs/design/dmr-networks.md`.

**Progress 2026-08-29:** `crates/astar-dmr` exists with `network` only — the
taxonomy (`DmrNetwork`: TGIF, FreeDMR, DMR+, SystemX, AmComm, VKDMR, FreeSTAR,
ADN, BrandMeister; label + stable slug each), the `NetworkClass` split that
groups the independents together and holds BrandMeister apart, and
`dialable(consented)` — the consent gate written once so no call site can
forget it. 7 tests, `std` only, no I/O. The identity question that blocked DMR,
NXDN and P25 alike is settled (astar-c9d2): callsign and radio ID are two
independent fields.

**Progress 2026-09-07 — RECEIVE IS BUILT, end to end.** `astar-dmr` is now a
client of the MMDVM/homebrew protocol: the `RPTL`→salt→`RPTK`→`RPTC` login FSM
with its keepalives and its `MSTNAK`-by-state diagnosis, the 55-byte `DMRD`,
the block codes written from their definitions (BPTC(196,96) and BPTC(128,77),
three Hamming codes, Golay(20,8), QR(16,7,6), RS(12,9), the 5-bit embedded-LC
checksum), the 33-byte burst with sync/EMB/embedded LC/full LC, and a real
bench **master** (`just dmr-parrot`) that binds, handshakes and relays
verbatim without ever dialling anything. Above it: `VocoderMode::Dmr`
(2450 + 1150, nine bytes — NOT YSF's configuration, see the design doc's
vocoder correction), `DmrLink`/`DmrSnapshot` on the one shared audio lane,
`Station::dmr_connect(system, host, port, radio_id, callsign, talkgroup,
timeslot, password)` with the password by value and the consent gate checked
before a socket or a dongle is touched, `IAX_ERR_DMR = -22` across the C ABI /
Swift / Python, `Network.dmr` in the macOS app with the
`system:host[:port]/tg[/ts]` grammar and the directory's 185 rows, and
`astar-cli dmr-listen` (`--features dmr`) reading its password from
`ASTAR_DMR_PASSWORD`, never `argv`.

**Verified as bytes, not as sound.** The crate suites, the loopback master and
the session pipeline prove bytes in and bytes out. Speech needs a `ThumbDV`
and a live master: `just dmr-parrot` then `just dmr-listen`, and that is
Rob's, not an agent's.

**Receive only.** `set_ptt` refuses a key-down while a DMR link is live, the
snapshot's `ptt` is always false, and `dmr-listen` has no PTT reader at all.

**Owed, in order:** (1) a **human read of BrandMeister's three wiki pages** —
Task 2's fetch met an anti-bot interstitial, the finding is outcome (b), and
the engine refuses BrandMeister unconditionally (`BRANDMEISTER_CONSENTED`, a
`const false`) until somebody has read their own wording (iax-e1d8 threads the
in-arg once that read stands); (2) **transmit**, Tasks 12–13 of the plan,
fenced until Rob confirms a clean YSF parrot round trip, then the parrot on
127.0.0.1 before TGIF and nothing else (iax-f4c1 lists what transmit owes);
(3) talkgroup lists for the networks whose directory rows carry none, and for
TGIF, which publishes no server rows at all.

**Fixed 2026-09-07 (whole-branch review):** the two `system` vocabularies met
at the ABI with no bridge on the Rust side, so `Station::dmr_connect` refused
every one of the app's 185 directory rows as an "unknown DMR network" — DVRef
names servers (111 distinct `system` values), `DmrNetwork` names families, and
not one server name equals a family slug. `dmr_connect` now takes any
non-empty `system`, resolving the family through `DmrNetwork::from_slug` then
the new `DmrNetwork::from_system_slug` (the twin of the app's
`DmrDial.family(ofSystem:)`, same table, same order) and dialing with
`family: None` when neither answers; BrandMeister is refused on the resolved
family AND on the raw spelling. `DmrConfig` carries `system: String` beside
`family: Option<DmrNetwork>`. The stale "directory rows" doc comment on
`DmrNetwork::slug` went with it.

**Not here, deliberately:** master hostnames, ports and passwords. Passwords
are per-network secrets — connect-time in-args only. Endpoints are directory
data that moves, and a hostname compiled into the engine goes stale in a
shipped binary.

### iax-e1d8 — BrandMeister consent has no way in: thread the in-arg through
*P3 low · feature · labels: dmr, api, abi, app, cx:3*
**Blocked by:** a human read of BrandMeister's wiki (iax-d4f7's owed item 1)

The gate is built and closed. `astar_dmr::dialable(consented)` decides, and
every call site asks it — but the only caller passes `BRANDMEISTER_CONSENTED`
(`crates/astar-station/src/station.rs`), a `const false` with no preference,
no config field and no C ABI in-arg behind it. The macOS app shows no
consent control at all: `dmr.brandmeisterConsent` persists, defaults off,
nothing sets it, and the directory hides BrandMeister's masters while it is
off. (A checkbox shipped briefly on 2026-09-07 and was pulled the same day —
it recorded an intent the build could not honour.)

That is the deliberate 2026-09-07 state, not an oversight: Task 2's check
(`docs/design/dmr-brandmeister-position.md`) is outcome (b) — no published
BrandMeister position astar could confirm, because their wiki answered every
fetch with an anti-bot interstitial, while third-party accounts point toward
(a), a network that asks softclients to stay off the Homebrew login. **A human
with a browser settles that in about a minute; nobody has.**

If the read confirms (b): thread a `brandmeister_consented` in-arg through
`Station::dmr_connect` → the C ABI → the Swift binding → `CallSession`, bring
the consent control back in `DmrSettingsView` (the copy is in
`docs/design/dmr-networks.md` §"What the gate looks like"), and make the box
the operator ticked the value the gate reads, with no default anywhere `true`. If it confirms (a): leave BrandMeister listed and refused
and write down why, which is the answer `p25-network.md` gives for IMBE. Do
not open the gate on an inference either way.

### iax-a7b2 — `*_can_connect` says yes where `open_voice_route` says no
*P3 low · bug · labels: api, dstar, ysf, nxdn, dmr, cx:1*

`dstar_can_connect` / `ysf_can_connect` / `nxdn_can_connect` /
`dmr_can_connect` (`crates/astar-console/src/session.rs`) exist so an embedder
can grey a button out before it tries. All four check `active`, the other
networks and `voice_route` — and **none of them checks the IAX2 link count**,
which `can_open_voice_route` does through `Manager::call_count() > 0`. Links
live in the Manager's call table and never set `active`, so an embedder
holding a Transceive link is told a D-Star connect would succeed and then
handed `AlreadyConnected` when it tries.

One line each, or better: have all four delegate to `can_open_voice_route`
plus their own "is my link already up" test, so the shared half cannot drift
again. Pin it with a test that opens an IAX2 link and asserts every
`*_can_connect` refuses.

### iax-f4c1 — what DMR transmit owes before it can go on the air
*P3 low · task · labels: dmr, transmit, cx:3*
**Blocked by:** the YSF parrot round-trip confirmation (Tasks 12–13 fence)

Collected while landing DMR receive, so the transmit task starts from a list
rather than rediscovering them:

* **A real DMR silence frame.** `null_frame(VocoderMode::Dmr)` in
  `crates/astar-codec/src/ambe.rs` substitutes D-Star's null codeword — nine
  bytes of the wrong vocoder configuration, tolerable ONLY because nothing
  transmits DMR. The reference pattern is `MMDVMHost/DMRDefines.h`'s silence;
  the hardware session is the arbiter if the two disagree.
* **`EmbeddedLcAssembler`, one per timeslot.** A single assembler shared
  across TS1 and TS2 interleaves two streams' LC fragments into one wrong
  answer. Receive today joins one slot, so it cannot happen; transmit and any
  future dual-slot receive can.
* **`discard_rx` on the key-down edge**, as `crates/astar-console/src/ysf.rs`
  does. One AMBE-3000 means a keyed station must drop in-flight decodes rather
  than let them surface as garble after the over.
* **The parrot's capture cap.** `crates/astar-dmr/src/master.rs` grows a
  capture until the terminator or the stale window; a peer that keys and never
  unkeys grows it without bound. Dev-tool grade today because nothing keys
  into it — that changes the day transmit lands.

### iax-b9c2 — NXDN engine backend: NXDNReflector protocol
*P4 backlog · feature · labels: nxdn, protocol, cx:4*

Per Rob 2026-08-03 (astar-b8e4). Same AMBE+2 vocoder gate. Protocol:
NXDNReflector UDP talkgroups (KC-Wide: TG 31313). CodecProvider +
capability pattern from iax-f2b8.

**Recipe:** `docs/design/adding-a-network.md` — the fourteen layers a network
touches, derived from M17 and D-Star. The vocoder is not the expensive layer:
AMBE+2 is a RATEP word on the ThumbDV astar already drives, and the directory
already publishes these reflectors with a `dial.kind`.

**Design:** `docs/design/nxdn-network.md`.

**Progress 2026-09-07 — RECEIVE IS BUILT.** `crates/astar-nxdn` carries the
protocol (framing, link FSM, a loopback reflector with a parrot mode);
`astar-codec` reuses `VocoderMode::YsfDn` rather than adding an
`NxdnDn` — `DroidStar` and `MMDVMHost` both configure the AMBE-3000 for NXDN
exactly as they do for YSF DN, so the frame packing is shared rather than
duplicated (see the design doc's "The vocoder" section for the citation).
`astar-console::nxdn` decodes onto the station's shared audio lane; the
Station facade, C ABI, node key guard, Swift binding and `Network.nxdn` are
all in, receive-only, per `astar-b8e4`. `astar-cli nxdn-listen` is the
hardware checkpoint, mirroring `ysf-listen`.

**Verified: loopback and unit tests, bytes proven. Unverified: audio.** The
protocol crate's own loopback reflector and the feature-gated suites
(`just nxdn-test`) all pass — framing, the link FSM, and frame packing are
proven bytes-in-bytes-out. Whether the AMBE+2 decode is *intelligible speech*
needs a `ThumbDV` on a live reflector and has not been run; that is Rob's
checkpoint, not an agent's. KC-Wide's NXDN TG 31313 is the local target.

**Remaining: transmit, and the identity range-check placement.** Transmit is
Tasks 9–10 of `docs/superpowers/plans/2026-09-07-nxdn-network.md`, fenced
until Rob confirms a clean YSF parrot round trip on the fixed build (the DN
encode/packing path is shared between the two networks). Separately, where
the reserved NXDN-id-range check lives (app-only today, not pushed into the
engine) is an open, deliberately un-decided question — see "The identity
question is not closed" in the design doc.

### iax-c9f4 — Hams Over IP engine backend: SIP/RTP client (G.711)
*P3 low · feature · labels: hoip, protocol, cx:4*

Per Rob 2026-08-03 (astar-f1c6). SIP registration + RTP audio with G.711 —
the engine already speaks G.711, so NO vocoder gate: after M17 this is
likely the cheapest new network. Needs a SIP stack decision (rust crate vs
minimal hand-rolled REGISTER/INVITE for the HoIP use-case), account
credentials via the SecretResolver pattern, Station capability flag.
Target: KC-Wide extension 15135.

### iax-b3d7 — SvxReflector client ("hamlink"): TCP control + UDP/Opus audio behind the Station contract
*P3 low · feature · labels: hamlink, protocol, cx:5*

astar's hamlink network (astar-9b3e; spec in
astar/docs/superpowers/specs/2026-08-02-network-switcher-design.md): a native
SvxReflector protocol client so the phone/desktop client can join the same
reflector a SvxLink-driven HF or VHF/UHF repeater sits on. TCP control
channel (auth, talkgroup select) + UDP audio (Opus), surfaced through
Station/ConsoleSession behind the existing poll + snapshot contract — no
callbacks — plus a capability flag the astar UI reads to make the `hamlink`
network available (`Network.available`). One active connection at a time,
shared PTT/meters. Long-term both this native path AND an AllStar
bridge-node path stay on the table (Rob, 2026-08-02); M17/DMR/D-Star
reflector families would follow the same capability pattern. Undesigned
beyond the seam — full protocol design when this item is picked up.

### iax-9c41 — In-band DTMF reports success when the tone can never transmit
*P1 high · bug · labels: dtmf, audio, announce, cx:2*

Diagnosed live 2026-08-02 while chasing "`*355553` does nothing from
astar". `Station::send_dtmf` (in-band mode) plans the tone and hands it to
the announce path, which returns `Ok(handle)` — but the announce TX lane is
clocked by MICROPHONE CAPTURE CALLBACKS. With no capture running (a
`NullBackend` station, a muted/denied/absent input device, or a call with
no mic routed), the tone sits queued forever and ZERO voice frames reach
the wire, while every layer up to the UI reports the digit as sent. Proven
with a frame-observer probe against the live hub: `announce() -> Ok`,
outbound voice frames during the whole announce window = 0; swapping the
same client to `CpalBackend` made the identical digit sequence arrive and
drive the hub's `*3` link command.

This silently breaks EVERY in-band DTMF consumer (astar's keypad, the
iax-4b7a sequencer) whenever capture isn't live, and it cost hours of
mis-attributed debugging — the failure is indistinguishable from a
far-end decode problem.

WHAT:
1. Fail loudly: `send_dtmf`/`send_dtmf_for`/`send_dtmf_string` should
   return an error (extend `AnnounceUnavailable`, or a new
   `StationError::NoCaptureClock`) when the active call has no routed mic
   or the capture stream is not delivering — rather than `Ok(())`.
2. Consider decoupling: drive announce-queue drain from a timer/pump tick
   instead of the capture callback so a tone can transmit on a
   capture-less station (a node daemon injecting audio has the same
   problem). Keep byte-identical behavior when capture IS live.
3. Surface it: a snapshot/telemetry bit ("tx lane not clocked") so a UI can
   show why nothing is going out.

VERIFY: a station with a silent/absent input device gets an Err from
`send_dtmf` (unit test with the existing `NullBackend`); with a clocked
backend the tone still transmits byte-identically (frame-count test).
Related: iax-8d2f (the pitch half of this bug, fixed 2026-08-02).

### iax-7e10 — Manager emits no LinkEvent for one-shot links (SSE link stream silent)
*P2 med · bug · labels: linking, node, cx:2*

During the live iax-d829.1 `/link` exercise (2026-08-02, VPS hub → local
Asterisk echo service) a link went connecting → up → disconnected and the
node's SSE stream carried ONLY 1/s snapshots — zero
`{"event":"link", ...}` edges. The consumer path is fine (the node's
`drain_link_events` → `link_event_to_node_events` → broadcast is
unit-tested); the gap is on the producer side: `LinkEvent::Connected` /
`Disconnected` are only emitted by permanent-link supervision in
`Manager::tick` (which the node doesn't call), and `Keyed` only by
`key_link`/`unkey_link`. A plain one-shot link reaching Active — or ending
— emits nothing. Emit Connected when a link's call transitions to Active
and Disconnected (with the hangup reason) when it ends, for ALL links, in
whatever pump path the Manager already owns; then a node SSE integration
test asserting the edges arrive around a fake link's lifecycle. Roster
polling via snapshots masks this today, which is why /status watchers see
links but event subscribers don't.

### iax-3f34 — Expose live-call RX + TX spectrum over the C-ABI + Swift binding (rxSpectrum/txSpectrum) — unblocks astar FFT
*P1 high · task · labels: binding, cx:1*

**Design:** WHY
astar's in-call FFT dropdown needs the live-call RX + TX spectrum, but the Station C-ABI + Swift binding only exposes micSpectrum (the no-call mic monitor). The Rust engine ALREADY has the methods from iax-2b09 — Station::tx_spectrum(&self, out: &mut [f32]) -> usize (crates/astar-station/src/station.rs:839) and Station::rx_spectrum (station.rs:848), tapping the active network call. They just aren't surfaced through the binding astar consumes (pp-c2bb). This is the focused binding-exposure slice (analogous to iax-0e9b for DTMF).

WHAT — mirror the existing mic_spectrum plumbing for tx + rx:
1. C-ABI (crates/astar-sys/src/ffi.rs): add iax_station_tx_spectrum and iax_station_rx_spectrum, mirroring iax_station_mic_spectrum exactly (same float *out + uintptr_t cap, returns count, IAX_SPECTRUM_BINS contract, NULL/panic guards). Wire to Station::tx_spectrum / rx_spectrum.
2. Swift wrapper (bindings/swift/Sources/AstarStation/Station.swift): add public func txSpectrum() throws -> [Float] and rxSpectrum() throws -> [Float], mirroring micSpectrum() (same buffer alloc + IAX_SPECTRUM_BINS + status->error mapping). Poll-only; no callbacks; honors pp-1df2.
3. Regenerate the cbindgen header (crates/astar-sys/include/astar.h) via scripts/check-cbindgen.sh / cbindgen so header-drift passes.
4. Tests: mirror the mic_spectrum FFI + station tests for tx/rx — on an active call the spectrum copies bins; idle/no-active-call returns 0/empty gracefully (no error).

VERIFY
- just ci green (fmt, clippy -D warnings, tests, cbindgen).
- After merge + astar re-vendor (just vendor + rebuilt xcframework), Station.swift exposes txSpectrum()/rxSpectrum().

DELIVERABLE FOR ASTAR: a vendored binding with public func txSpectrum()/rxSpectrum() throws -> [Float] returning the live-call dBFS bins (same format as micSpectrum).

### iax-5088 — astar-inspect: protocol inspector / conformance harness with expected-vs-actual frame diffing
*P1 high · feature · labels: cx:2*

Build a web/human + agent testing framework that drives the IAX2 FSM and shows every frame on the wire annotated with expected-vs-actual verdicts. Seeded by the throwaway crates/astar-iax/examples/probe.rs. Primary near-term payoff: it IS the debugger for iax-3fca (the ASL3 web-transceiver connection gap) -- first scenario is the WT handshake.

LOCKED DECISIONS (brainstorm 2026-06-10):
- Q1 intent: value-first -- ship a thin tracing core + verdict engine that immediately debugs iax-3fca, designed so web UI and agent interface layer on top. Not a throwaway, not a big-bang suite.
- Q2 frame sources: live UDP, recorded replay, and in-process fake-peer are CO-EQUAL, chosen per-run (--source live|replay|fake-peer). Live is real traffic against a real node (e.g. parrot 55553) -- deliberate, not constant.
- Q3 agent interface: CLI emitting JSON. Command shape: "iax-inspect run --source ... --scenario ... --json" prints the full trace to stdout; agents shell out + parse. Plus a --pretty human terminal mode (promoted probe.rs).
- Q4 human web view: static self-contained HTML report NOW (--report out.html bakes timeline+hex+decode+verdicts into one file); live local web server LATER (deferred phase).

ARCHITECTURE: new tooling crate named astar-inspect (NOT in astar-iax-core -- keeps core dependency-free). The new crate may use serde/serde_json freely; NOTE there is currently NO serde anywhere in the workspace, so the inspector defines small OWNED TraceEvent mirror types because the core frame types borrow &str/&[u8] and cannot derive Serialize. Three layers over one trace model:
1. Tracing core: wraps the existing Fsm+Reliability drive loop (same handle->dispatch->on_frame_in->tick cycle as run_loop/probe.rs); records Vec<TraceEvent>{seq, t_ms, direction Tx/Rx, raw_hex, decoded(frame+IEs), fsm_state_before/after, actions, verdict}.
2. Expectation engine: per FSM state classify each frame Expected | Unexpected | Divergent{field, expected, actual}. Outbound diffs vs the DroidStar web-transceiver golden (the shape we PROVED works against 55553); inbound vs the FSM state-transition table.
3. Front-ends over the trace: CLI/JSON (agents), --pretty terminal (humans now), static HTML report (humans now), live web server (later).

REUSE: frame.rs (Frame/FullFrame/MiniFrame, parse/parse_lenient/encode -- all Debug), ie.rs Ies (~60 typed fields), subclass.rs enums. Drive-loop reference: client.rs run_loop + astar-conformance driver.rs + examples/probe.rs.

PROPOSED PLAN DECOMPOSITION: (1) tracing core + owned model + fake-peer source; (2) expectation engine; (3) CLI/JSON + pretty output; (4) live + replay sources; (5) static HTML report; (6) ASL3 WT scenario wired end-to-end. Spec to be written to docs/superpowers/specs/.

### iax-64f0 — Session FSM parity vs C iax.c (pcap-driven)
*P1 high · task · labels: cx:3, tests, tests,session,protocol*
**Blocked by:** iax-7022 (closed)

**Design:** Validate the Rust session FSM emits the same outbound frame sequence as the vendored C iax.c, given the same inbound frame sequence. Uses the pcap fixtures captured for iax-7022. For each recorded scenario (register, NEW+CALLTOKEN+AUTHREQ+AUTHREP+ACCEPT, hangup, …): replay the peer→client frames into the Rust FSM, assert the FSM's emitted client→peer frames match the originals in the pcap modulo non-deterministic fields (call numbers, timestamps, MD5 challenge response is determined by challenge so structurally check). Anything that diverges is either a Rust bug or an intentional deviation that gets documented.

### iax-b1fe — RX noise reduction: adaptive NR on the output bus (post-mix, toggle)
*P1 high · feature · labels: cx:3*

**Design:** WHAT: RX noise reduction - one adaptive NoiseReducer on the mixed output path, toggle default OFF, mirroring the mic-NR plumbing (set_noise_reduction) at every layer.
PLACEMENT: OutputBus (crates/astar-audio/src/router.rs ~line 820) owns nr: NoiseReducer (bus sample rate) + rx_denoise: Arc<AtomicBool> (default false) + accessor. Processing order in OutputBus::read: mix -> NR -> rx_gain -> rx_peak meter -> rx_spectrum (NR before gain so volume does not disturb the noise estimate; meter/spectrum after so they show what is heard).
PLUMBING: Router::set_output_denoise (setter only); console session persists the flag across reconnects + re-pushes on connect (pattern: compression level / tx trim, see iax-750a); Station::set_rx_noise_reduction(bool); C-ABI iax_station_set_rx_noise_reduction (pattern: iax_station_set_noise_reduction) + cbindgen header regen (scripts/check-cbindgen.sh).
FUTURE (deferred by Rob, do NOT build now): per-call NoiseReducer before the mixer if multi-call monitoring shows blended-floor artifacts - keep the control plumbing shaped so that variant can slot in behind the same single user toggle.
Design doc: astar repo docs/superpowers/specs/2026-07-06-rx-noise-reduction-design.md.
VERIFY: TDD. Tests: NR measurably reduces a synthetic noise floor post-mix; toggle off = bit-exact passthrough; rx_peak reads post-NR level; session flag survives reconnect. cargo test/clippy/fmt + header check green. Consumed by astar-70b9.

### iax-d829 — EPIC: Node link-control parity with AllStar (DTMF *3/*2/*1 + node-to-node linking)
*P1 high · epic · labels: astar, cx:2, dtmf, linking, node*

**Design:** Make astar-server link to/from other AllStar nodes the way a real AllStar node does, driven by AllStar's ilink DTMF control surface.

GOAL / ACCEPTANCE
- *3<node> connects (full two-way), *2<node> monitors (RX-only, mic withheld), *1<node> disconnects.
- Driven BOTH via the node's HTTP control channel AND via real in-band DTMF decoded from radio/mic audio.
- CALLTOKEN + per-node link auth verified (token requested on the link dial path, dest_call=0 on resend; inbound link issues/validates token + auth).
- Each child merges on harness/conformance proof (astar-conformance + local Asterisk/ASL3 podman); a final live-AllStar bring-up over the WireGuard tunnel closes the epic.

WHAT ALREADY EXISTS (do not rebuild)
- Library link substrate is implemented in Manager: connect_link / disconnect_link / link_roster / link_events, transceive<->monitor (crates/astar-iax/src/{manager,link,link_control}.rs; design iax-42e9).
- The GAP is at the node-daemon edge: NodeCommand + control HTTP only do single-call Dial/Hangup; no link control, no DTMF->link mapping, no in-band decode wired in, no link-auth conformance, no live bring-up.

DECOMPOSITION (children, depend-on existing Station/relay/reachability nuggets rather than duplicate):
 1. Node link-control over HTTP (the testable MVP spine) -- START HERE.
 2. DTMF -> link command mapper (site-configurable ilink map).
 3. In-band DTMF decode into the node (integrates iax-d111 + iax-3746).
 4. CALLTOKEN + node-to-node link auth conformance (astar-conformance).
 5. Harness: link FSM + *3/*2/*1 transition tests vs local Asterisk/ASL3.
 6. Live AllStar bring-up over the WireGuard tunnel (absorbs iax-be48) -- closes the epic.

Linked deps (not children): iax-3746, iax-d111 (feed #3); iax-42ce (N-way relay, after 1:1); iax-be48 (feeds #6); iax-b764 POKE (optional self-check for #6).

Reference: docs/allstar-interop.md (DNS resolution, IAX2 exchange + dest_call=0 rule, link modes & DTMF table, NAT/CGNAT).

### iax-dd42 — astar-console + web operator console (Tauri-reusable call engine)
*P1 high · feature · labels: astar, console, cx:8, ui*

Reusable astar-console library + web operator console harness. Design: docs/superpowers/specs/2026-06-10-astar-console-operator-console-design.md

PRIMARY DRIVER: the downstream astar app is a Tauri app (Rust backend + webview). Its call engine is this console core; its webview is this meter/PTT/status surface. So the core must be a clean, reusable, runtime-agnostic (no-tokio) library, and the web frontend must lift into the Tauri webview.

Crate split: (1) crates/astar-console LIB (reusable, dep-light) — ConsoleSession (connect WT call, set_ptt, disconnect, list_devices, snapshot), MeteringBackend audio-decorator (TX+RX RMS to dBFS via Arc<AtomicU32>, zero core changes), ConsoleConfig, ConsoleState (single source of truth), serde feature-gated off by default. astar depends only on this. (2) crates/astar-inspect BIN (harness) — tiny_http + SSE web server bound to 127.0.0.1, vanilla HTML/CSS/JS frontend split into transport-agnostic render(state)/sendCommand + a thin SSE/fetch adapter (Tauri swaps invoke/listen). 

First build = operator console only: node/secret/input/output fields, live TX+RX meters, red/green PTT light (press-and-hold), call status (Answered/rtt/Hangup). Built on the iax-3fca WT call shape.

Phasing: P1 console lib (fully tested vs the existing fake WT peer + null backend, offline), P2 web front-end, P3 (later) TUI ratatui, P4 (later) frame inspector = the original iax-5088 expected-vs-actual diffing.

New deps (gated): tiny_http, serde_json (inspect bin); serde optional (console lib). None in core/iaxclient. Tests offline/deterministic; live parrot run is operator-driven. Lineage: evolved from iax-5088; unblocked by iax-3fca (done).

### iax-f56c — uci150_usb backend hard-codes active-low; CTS/active-high setups read inverted
*P1 high · bug · labels: cx:2, ptt*

**Design:** The raw-USB UCI150 PTT backend (crates/astar-ptt/src/uci150_usb.rs) assumes ALL modem-status lines are ACTIVE-LOW:

    fn decode_status(raw, line) -> bool { (!raw) & bit != 0 }   // asserted when bit==0

This was correct for the BENCH unit (DCD active-low: idle 0xff -> keyed 0xf7). But the line AND its polarity are MicPTT-Dest-switch / wiring dependent. A common AllStar UCI150 config drives PTT on CTS, ACTIVE-HIGH — which this backend would read INVERTED (keyed reported as idle and vice-versa).

FIX: make polarity selectable alongside the existing KeyLine selection.
- Add an enum/flag (e.g. Polarity::ActiveHigh | ActiveLow) to Uci150Usb::open_with and the ModemPort/decode path.
- decode: ActiveHigh -> (raw & bit != 0); ActiveLow -> (!raw & bit != 0).
- Keep the bit map fixed (CTS=bit0, DSR=bit1, RI=bit2, DCD=bit3 — CH341 status register, req 0x95 / wValue 0x0706).
- Default: pick the AllStar-common default (CTS / active-high) OR keep DCD/active-low but make both trivially overridable; document that the operator must match their MicPTT Dest switch.
- Consider a tiny diagnostic (print the live raw byte) so users can determine line+polarity empirically.

Surfaced while writing the astar implementation guide (astar-fc4e): operator confirmed their AllStar box is CTS / active-high, opposite the bench DCD / active-low. Same correction is needed in astar`s Swift reimpl.

### iax-031c — Standalone audio-DSP library binding: C-ABI + PyO3/numpy (IAX2-free) exposing DTMF/CTCSS/NR/compression/mic-profile
*P2 medium · feature · labels: audio, cx:2, dsp, ffi, python*
**Blocked by:** iax-dedb (closed), iax-e549

**Design:** Package astar-audio + the DSP parts of astar-codec (DTMF) behind their own binding so non-IAX2 consumers (Python, etc.) use the DSP on raw PCM buffers, independent of the call engine. astar-audio and astar-codec already have ZERO dependency on iaxclient/-core; a CI check must keep them call-engine-free.
Binding tech DECIDED: C-ABI core (uniform with iax-dedb's Station FFI strategy; serves C/Swift/.NET) PLUS a thin PyO3/numpy wrapper over the same Rust core for an idiomatic pip-installable Python module with zero-copy numpy arrays. This is safe here (unlike the Station FFI) because the DSP path is pure buffer-in/buffer-out: no callbacks, no long-lived managed runtime.
Surface over PCM (f32/i16) buffers: DTMF generate (digits->samples) + Goertzel decode (samples->digits); CTCSS encode/decode (from the CTCSS ticket); noise reduction; dynamic-range compression; mic characterization -> MicProfile. Config structs (e.g. MicProfile) cross as secret-free JSON, reusing the existing serde models.

### iax-08d8 — Harness: codec negotiation view + select (show negotiated/offered, force preferred)
*P2 medium · task · labels: cx:2*

**Design:** Add codec visibility + selection to the inspect harness. Show which audio codec the call negotiated and what the peer offered; allow forcing/preferring a codec before/at connect (e.g. uLaw/GSM/Speex per build).

SCOPE:
- Display negotiated codec + offered/allowed caps for the active call (from the core capability negotiation).
- Connect-form control to set preferred/allowed codec mask before dialing.
- Reflect mid-call codec if it changes.
SOURCE: astar-codec + core capability/format fields. Verify what the FSM exposes for negotiated format and allowed caps; thread into Station snapshot + the connect path.
UI: codec shown in call status; selector on the connect form (and/or a Codec sub-panel).

OPEN: confirm the core exposes negotiated format + cap mask; if not, add accessors. Coordinate with vendor-neutral IAX2 stance (codec is a call param, not AllStar policy).

### iax-12fb — Node inbound authentication (auth=Required + per-caller secret map): the deployed hub runs listener auth=off …
*P2 medium · task · labels: cx:2, node, security*

STATUS 2026-09-27 — **advanced, not closed.** Per-caller inbound secrets now
have a durable, secret-free-config home: a bare-username key in
`credentials.toml` (`"55553" = "…"`, `allstar-public = "…"`), loaded under
`[secrets] source = "file"` and settable from the settings page as
`peer:<user>` (`work/astar-slash`). What remains is the item itself: wire
`auth = "required"` to that map on the deployed hub and flip it from `off`.

Node inbound authentication (auth=Required + per-caller secret map): the deployed hub runs listener auth=off. iax-8baf implemented server-side MD5 challenge/validate, but node.toml has no credentials-map wiring and the estate default is open. Add a [listener] credentials/secret surface to node config (secret-free: runtime/vault only, never in the config struct/export/logs), wire it to IncomingCallPolicy auth=Required, and flip the gh-runners hub to authenticated. This is the last unbuilt piece of the iax-6461 asterisk-lite MVP (D2).

### iax-1ea2 — Harness: per-stream audio level/VU meters + gain/AGC controls
*P2 medium · task · labels: cx:2*

**Design:** Expose per-stream audio level/VU meters and gain/AGC controls in the inspect harness. The harness already has coarse rx_level_db/tx meters; this adds finer per-stream metering and input/output gain + AGC control surfaced from the audio path.

SCOPE:
- Input (mic) and output (speaker) VU/peak meters, separate from the call's rx/tx level.
- Gain sliders for input and output; AGC on/off if the audio backend supports it.
- Persist/restore via the existing harness config where sensible.
SOURCE: astar-audio (CpalBackend) — verify what level/gain/AGC hooks exist; some may need adding to the audio backend trait.
UI: a Levels/Audio panel (or fold into an existing tab); live meters via the existing poll/SSE.

OPEN: does CpalBackend expose gain/AGC today? If not, scope the backend additions separately. Relates to UCI150 mic-input/monitor-output device selection ([[project_uci150_dry_run]]).

### iax-2047 — Harness: surface call/network quality stats (jitter buffer, loss, RTT, OOO) in Network tab
*P2 medium · task · labels: cx:2*

**Design:** Populate the inspect harness Network tab with live call/network quality stats from the iax library. Currently the harness shows TX/RX level meters and call phase, but not transport quality.

SCOPE — surface (read-only) per active call:
- ~~jitter buffer depth / target, jitter estimate~~ — **done (iax-rxjb)**: the
  RX path has an adaptive jitter buffer on the mixer lane, and the snapshot
  carries `rx_jb_depth_ms`, `rx_jitter_ms`, `rx_frames_lost`,
  `rx_frames_late`, `rx_frames_ooo` and a cumulative `rx_underruns`, plus the
  effective config (`rx_jb_enabled` / `rx_jb_min_ms` / `rx_jb_max_ms`). All the
  way through the C ABI, Swift and Python. See `docs/design/rx-jitter-buffer.md`.
- packet loss (rx/tx), out-of-order count, dropped/late frames — the RX half is
  covered by the counters above; the TX half is still open
- round-trip time / ping
- frames sent/received, bytes
SOURCE: the receive-side stats come off the mixer lane's jitter buffer
(`astar-audio`), not the core session — verify what remains against
`Call::snapshot` before adding a second source of truth.
UI: Network tab table, polled or via SSE alongside the existing meters.

PREREQ/OPEN: the remaining fields (TX loss, RTT, frame/byte totals) still need
an audit of astar-iax-core for an existing netstats struct; if absent, add a
stats accessor on the session and thread it through Station. What is left here
is the harness UI plus those TX-side numbers.

### iax-21c8 — astar-sys: C-ABI compatibility shim for astar drop-in
*P2 medium · feature · labels: api, cx:5, ffi, migration*
**Blocked by:** iax-802e (closed)

**Design:** Thin C-ABI layer that re-exports the iaxc_* symbols astar already calls, so astar/crates/astar-astar-sys can swap its build.rs to point at our static lib without astar-core changing.

ABI surface (from astar/crates/astar-core/src/client.rs — full audit needed before locking):
  - iaxc_initialize / iaxc_shutdown
  - iaxc_register
  - iaxc_call / iaxc_hangup
  - iaxc_input_level_set / iaxc_output_level_set
  - iaxc_send_dtmf
  - iaxc_set_audio_prefs
  - iaxc_set_event_callback
  - iaxc_audio_devices_get
  - (plus the IAXC_CALL_STATE_* constants and event struct layouts)

NOT in scope: every iaxc_* symbol upstream defines. Only what astar actually links — keep surface small.

Acceptance:
  - astar/crates/astar-astar-sys/build.rs points at our libiaxclient.a
  - astar-app builds + runs without changes to astar-core
  - All astar features that worked with patched C continue to work

References:
  - astar/crates/astar-astar-sys/wrapper.h
  - astar/crates/astar-core/src/client.rs + ffi_glue.rs

### iax-28b3 — iaxclient-devices: interface/handset capability model + catalog + RigConfig resolver
*P2 medium · feature · labels: cx:5, device, handset, station*
**Blocked by:** iax-dedb (closed)

**Design:** New crate `iaxclient-devices`. Guided two-step device/handset selection that resolves to a secret-free config Station consumes. Brainstorm 2026-06-14.

Model (interface = transports, handset = signals):
- Transport: audio-in, audio-out, serial port, CM108 HID line.
- InterfaceProfile: name, detection signature, transports provided, default audio in/out matchers, PTT transport.
- HandsetProfile: name, DTMF source (InBandAudio | Serial | SoftwareDialpad), PTT trigger (serial CTS | VOX | software), required transports.
- Rig/RigConfig: resolved (interface + handset + overrides).

Two-step flow: pick interface -> compatible_handsets(iface) (filtered by required transports) -> resolve(iface, handset, overrides) -> RigConfig.

Resolution/merge:
- audio in/out: interface device matchers, overridable.
- PTT: interface supplies the backend (Uci150Serial / Cm108Hid / none, from astar-ptt); handset supplies the trigger over the existing PttBridge.
- DTMF source: handset-driven -> InBandAudio activates the mic-capture decoder (iax-d111), Serial activates the serial DTMF source (iax-5a5c) on the interface serial port, SoftwareDialpad -> UI dialpad only.

Config in/out (NOT persistence): RigConfig is serde-serializable. The crate INGESTS a config (deserialize -> resolve) and EMITS one (serialize). No disk I/O; the UI owns storage. SECRET-FREE: no credentials in RigConfig; building a StationConfig fills only non-secret fields, the UI injects portal/secret straight into the Station.

Initial catalog: interfaces = AllScan UCI150 (USB audio + WCH/CH340 serial PTT), generic CM108 dongle (HID PTT), built-in/default audio, Custom. Handsets = software dialpad, plain mic, generic in-band-DTMF mic (-> Alinco EMS-79 once confirmed), generic serial mic (-> ICOM once confirmed).

Depends on crates astar-audio (device enumeration) + astar-ptt (backends); integrates iax-dedb (Station/StationConfig). Auto-detect is a follow-up. Unit-tested: compatibility filter, resolve shapes, RigConfig serde round-trip.

### iax-31e9 — Harness: link FSM + *3/*2/*1 transition conformance (vs local Asterisk/ASL3)
*P2 medium · task · labels: cx:3, linking, protocol, test, tests*

Deterministic connect/monitor/disconnect transition tests for the link state machine against a local Asterisk/ASL3 podman. Merge gate for the link children. Builds on iax-c648 live-parrot harness + astar-conformance.

### iax-3448 — Add AGPL-3.0 SPDX license headers to all astar-lib source files
*P2 medium · task · labels: cx:2, licensing*

**Design:** WHY
astar-lib was relicensed MIT -> AGPL-3.0-only. Each source file should carry a per-file SPDX header so the license is unambiguous at the file level. NOTE (superseded in part): this repo is now AGPL-3.0-only end to end — the app is no longer proprietary and there is no open-core dual license, so the only per-file header anywhere is the AGPL one. The single exception is `vendor/ambe-thumbdv/`, which keeps its own MIT/Apache-2.0 notices and must NOT be given AGPL headers.

TIMING
Do this AFTER the planned astar-lib history reset / WT-Node refactor (Rob: 'iaxclient after reset'), since that churns file locations. Until then this nugget just tracks the intent.

WHAT
- Add to the TOP of every hand-written source file (~172 .rs across crates/, plus build/helper shell + python scripts):
    // Copyright (c) 2026 Rob Ludwick
    // SPDX-License-Identifier: AGPL-3.0-only
  * .rs: place ABOVE the leading //! module-doc comments (regular // comments may precede //!).
  * shell/python: '#' comment, AFTER the '#!' shebang.
- NO 'no-AI-training' clause anywhere; AGPL-3.0 forbids further restrictions (Rob: 'The AGPL3 code is fine'). (Originally this clause was scoped to the then-proprietary app; the app is AGPL now, so it applies nowhere.)
- Idempotent: skip files already containing Copyright/SPDX.
- SKIP: target/, .worktrees/, vendored/third-party, and anything generated. For the cbindgen-generated C header (crates/astar-sys/include/astar.h), prefer configuring cbindgen's 'header' preamble (cbindgen.toml) to emit the SPDX line rather than hand-editing the generated file.

VERIFY
- Every .rs under crates/ (excl target/.worktrees) contains 'SPDX-License-Identifier: AGPL-3.0-only'.
- cargo build + clippy still pass (headers are comments).
- cbindgen header carries the notice if the preamble is configured; check-cbindgen stays green.

### iax-3746 — Station DTMF router + sinks (out-of-band / in-band / sidetone)
*P2 medium · feature · labels: cx:5, dtmf, station*
**Blocked by:** iax-68eb (closed), iax-dedb (closed)

**Design:** DTMF orchestration in the Station/console layer. Source -> router(policy) -> sink model (brainstorm 2026-06-14).

Normalized event: `DtmfDigit(char)`. A router applies a user-configurable policy mapping each source's digits to one or more sinks.

Sinks:
- out-of-band: emit IAX2 DTMF frames (reuse iax-be21).
- in-band: synthesize the tone (codec generator, iax-68eb) into the OUTGOING audio.
- local sidetone: play the tone to the OUTPUT/playback path only, never the wire; can accompany either sink.

Sources covered here: the UI dialpad (a `station.send_dtmf(digit)` call). Audio and serial sources land in follow-up tickets.

Policy: one global default (default OUT-OF-BAND — the correct AllStar path) with optional per-source override. RX-decoded digits surface as `StationEvent::DtmfDetected(char)` regardless of policy.

Station API (sketch): `play_dtmf`, `send_dtmf` (policy-routed), explicit `send_dtmf_inband`, plus `DtmfDetected` events and last-digit in ConsoleState.

Depends on iax-68eb (codec primitive) and iax-dedb (Station library).

### iax-3ec5 — Resolve announcement phrases off the session lock (TTS no-stall)
*P2 medium · chore · labels: audio, cx:2, node, refactor*

Final-review I1 residual: synth_via_piper now enforces cfg.timeout but still runs UNDER the Station session mutex, so a slow/large TTS synth holds the lock (stalls pump/SSE) for up to the timeout. Resolve the Phrase→PCM (incl. TTS subprocess) BEFORE taking the session lock; lock only to enqueue/play. Removes the control-plane stall entirely.

### iax-42ba — Fix queued (non-started) announcement placeholder handle
*P2 medium · bug · labels: audio, cx:2*

Final-review M1: the non-preempting queued path returns AnnounceHandle::new_placeholder() which is already is_done()==true while the real announcement plays later (real handle discarded in poll_announcements). Benign now (every caller drops the handle) but a latent API-correctness bug: a caller that polls the queued handle sees 'done' immediately. Store the resolved request + return the real handle when it starts, or redesign the queued-handle contract.

### iax-4af7 — Investigate selectable denoise filters: front-end-choosable strategies to clean a noisy line
*P2 medium · task · labels: audio, cx:3, dsp, research, ux*

Research + prototype a menu of noise-reduction strategies an operator can pick per-line, since one filter doesn't fit every noisy condition. Survey: spectral subtraction, Wiener/adaptive filtering, RNNoise-style ML denoise, tonal notch/comb for whine (already have calibrated MicProfile notches, iax-fb8d), adaptive noise gate + hum filter (current generic NoiseReducer), multiband compression. Goal: a front-end picker ('clean a noisy line' presets) backed by a DspStage chain (relates iax-0465) and the per-mic router lane (iax-64b6 P1 surfaced gain/denoise/compress/profile cells). Out: a recommendation on which 2-4 options to ship + how to expose them. Relates iax-267f (spectral NS), iax-d50d (NR+compression).

### iax-6520 — FFI: inbound codec_policy on IaxNodeConfig (bidirectional wideband for node mode)
*P2 medium · task · labels: cx:2*

**Design:** iax-3e53 exposed codec_policy on IaxConfig (outbound + engine rate) and negotiated_format on IaxState, but IaxNodeConfig has no codec_policy field — an FFI/Swift Node-mode station cannot request slin16 for INBOUND calls (IncomingCallPolicy stays UlawOnly). Mismatches don't break (capped_to_rate downgrades with only a tracing::warn, invisible over FFI; the negotiated_format snapshot lets the UI observe what actually landed), but bidirectional wideband node-to-node needs the inbound knob. Scope: codec_policy string field on IaxNodeConfig -> IncomingCallPolicy.codec_policy (same parse/error conventions as IaxConfig), header regen, Swift NodeConfig mirror, tests. While there: (a) add VoiceFormat to astar-station prelude re-exports (lib.rs:65 gap); (b) note the Swift VoiceFormat enum decodes unknown bits as nil — indistinguishable from no-call; whoever adds the next CodecPolicy variant must extend the Swift enum (leave a comment breadcrumb at both sites).

### iax-66ba — FFI: device catalog + rig resolve/apply over the C-ABI (JSON, secret-free)
*P2 medium · feature · labels: cross-platform, cx:3, device, ffi*
**Blocked by:** iax-28b3, iax-dedb (closed)

**Design:** Expose iaxclient-devices over the astar-sys C-ABI so cross-language native clients (C / Swift / Python / .NET; Linux / Windows / Mac / iOS) drive the two-step selection. Decision: catalog + RigConfig cross as JSON strings (one serde model serving UI rendering, the UI save/load, and the FFI). Live call state stays #[repr(C)] poll/snapshot.

C-ABI (sketch):
  iax_devices_catalog_json(buf,len)                  -> catalog JSON
  iax_devices_compatible_handsets(iface_id, buf,len) -> filtered handsets JSON
  iax_rig_resolve(json_selection, buf,len)           -> resolved RigConfig JSON
  iax_station_apply_rig(station, json_config)        -> apply non-secret config

SECRET-FREE across the boundary: the UI injects portal/secret directly into the Station, never through the rig JSON. Caller-buffer + truncation rules as in the existing Station C-ABI. Swift/Python/C examples that list the catalog, resolve a UCI150 + handset selection, and apply it.

Depends on iaxclient-devices (iax-28b3) and iax-dedb (Station C-ABI in astar-sys).

### iax-72df — In-band DTMF decode wired into the node (RX/mic -> mapper)
*P2 medium · feature · labels: audio, cx:3, dtmf, node*
**Blocked by:** iax-d111, iax-3746

Feed real in-band DTMF (RX + mic, Goertzel) through the child #2 mapper inside the daemon. Largely INTEGRATION of existing iax-d111 (DTMF audio sources) + iax-3746 (Station DTMF router + sinks) into astar-server. Depends on those two.

### iax-74be — uci150_usb: adopt nusb 0.2.5 macOS unconfigured-device guidance in open_wch
*P2 medium · task · labels: cx:2*

**Design:** nusb 0.2.5 documents (commit df0ef13) that macOS only auto-configures composite-class devices or those with a known driver; vendor-class devices may be unconfigured after open. Our CH343 works because Apple's CDC-ACM driver attaches (known driver -> configured), but CH340/341 variants are vendor-class 0xFF and would be unconfigured. Add the documented pattern to open_wch() right after open(): if device.active_configuration().is_err() { device.set_configuration(1).wait() } (CH34x have exactly one configuration; when the CDC driver holds iface 0 the check short-circuits before the exclusive-open requirement matters). Unit-testable only by inspection/hardware; keep it minimal + doc comment citing the nusb quirk note.

### iax-7b9d — Cross-platform device auto-detection (DeviceDetector trait + macOS)
*P2 medium · feature · labels: cross-platform, cx:3, device*
**Blocked by:** iax-28b3

**Design:** A `DeviceDetector` abstraction that scans connected hardware, matches it to catalog InterfaceProfiles, ranks candidates, and pre-selects the best; always falls back to Custom. macOS impl first (CoreAudio device names + IOKit/serial enumeration; serial via the serialport crate, CM108 via hidapi). Linux/Windows impls are separate follow-up tickets.

Seeds the existing UCI150 autodetect (WCH cu.wchusbserial port). Output feeds the iaxclient-devices two-step picker (pre-selected interface).

Depends on iaxclient-devices (iax-28b3). Cross-platform note: serial + HID libs are portable; only audio-device + USB enumeration is per-OS.

### iax-809f — Test gaps from 2026-06-05 review
*P2 medium · task · labels: cx:3, review-2026-06-05, tests*

Test coverage gaps surfaced by the 2026-06-05 review:

- Sequence-number wrap-around (Reliability and FSM). The test that would have caught iax-b47c. Add a proptest that drives 1000+ frames through Reliability with random duplicates and OOO arrival, asserting accurate ISeqno tracking.
- Fuzz targets fuzz_parse_full and fuzz_parse_ies (fuzz/ dir exists but excluded from scope). Parser's adversarial-input resistance is currently asserted only by hand-picked truncation tests at tests/fixtures.rs:261-275.
- Malformed-IE-inside-AUTHREQ regression: fsm.rs:263 does Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty()) so a malformed AUTHREQ silently loses the challenge and FSM produces empty MD5 response. No test pins this.
- text.rs proptest only generates valid K-status payloads; add a strategy with arbitrary \x00-containing or extremely long inputs to confirm the Raw fallback never panics.
- Reliability::tick interleaved with peer ACK mid-tick. Race-free by single-threaded design but FSM-test combination doesn't pin that.
- RxOutcome::GaveUp → Event::DeliveryFailed path: FSM has no arm, falls through to LogInvalid (silent failure). Add a test that drives Reliability to retransmit exhaustion and asserts the FSM surfaces a typed error to the app.

### iax-85c9 — Human-phrased event-table + voice-ID announcements
*P2 medium · feature · labels: cx:2, node*

Final-review M4: maybe_fire_event_announcement currently synthesizes the raw config KEY (e.g. 'incoming_call'/'hangup') to air via Phrase::Text(key); id_request voice mode uses 'node {id}'. Map events to human phrases (e.g. 'node {id} connected') or named samples, per spec §Triggers examples.

### iax-8b0a — Asterisk harness: unhappy-path scenarios (REJECT, timeout, expired CALLTOKEN, malformed frames)
*P2 medium · task · labels: cx:2, infra, test, tests*

**Design:** Once iax-6813 happy-path harness lands, extend it with adversarial scenarios:

Goals:
  - Bad password -> AUTHREQ -> AUTHREP -> REJECT -> Failed
  - CALLTOKEN expired (delay >10s between server CALLTOKEN reply and client re-NEW) -> server rejects
  - Server unreachable (no Asterisk listening) -> NewSent retry exhausted -> Failed
  - Malformed inbound frame (truncated header, bogus IE length) -> Fsm rejects without panic
  - Asterisk-initiated HANGUP mid-voice (dialplan timeout) -> Active->Closed via inbound HANGUP
  - Network jitter / packet loss simulated via tc/netem in the bridge -> Reliability retransmit kicks in

Each scenario produces a sanitized pcap fixture so replay.rs catches regressions without spinning up the harness.

References:
  - iax-6813 (parent harness, happy-path v1)
  - iax-c333 plan follow-up: 'peer-initiated Hangup ACK' test gap
  - iax-7022 (real ASL3 captures, complementary)

### iax-9450 — ENCKEY (IE 44): add length/format guard so malformed payloads can propagate in lenient mode
*P2 medium · task · labels: cx:2, protocol, security*

iax-1741 made CHALLENGE/MD5_RESULT/RSA_RESULT/PASSWORD/ENCRYPTION strict in lenient mode. ENCKEY (id 44) was NOT added: it is parsed as opaque Some(payload) with no malformed-payload error path, so there is nothing to propagate and listing it in is_strict_in_lenient_mode() would be inert/untestable. If the AES/encryption work (iax-6c64) lands, ENCKEY should get a validation guard in apply_ie (size/format per the key schedule) so a malformed key errors instead of silently downgrading; then add it to the strict list with the two-test pattern.

### iax-9a71 — iOS audio: AVAudioSession ownership + RemoteIO capture/playback verification (cpal 0.15 on iOS)
*P2 medium · task · labels: astar, audio, cx:3, ios*

**Design:** Heads-up from the astar iOS feasibility review (astar docs/design/ios-client.md section 3; astar M1 spike astar-44c3). The audio backend is cpal 0.15 then CoreAudio/AudioUnit RemoteIO on Apple (astar-audio/Cargo.toml:9, stream.rs:204-212). NOTHING in astar-lib configures or activates an AVAudioSession (repo-wide grep of crates/bindings/scripts for AVAudioSession, RemoteIO, set_active = zero hits). On iOS the OS will not grant the mic to RemoteIO until an app has a .playAndRecord AVAudioSession setActive(true) — by Apple design that is the responsibility of the app, so astar will own the session in its Swift layer first (no Rust change needed; that is the M1 spike).

This ticket is the engine-side follow-up, CONTINGENT on the astar M1 result: first, confirm cpal RemoteIO capture and playback work against an app-activated AVAudioSession on a real device; second, if RemoteIO misbehaves vs a live session (sample-rate or buffer-size negotiation, route changes, Bluetooth SCO, simulator vs device), provide a first-class iOS audio path — a session-aware cpal configuration or an explicit AVAudioEngine/RemoteIO backend. Filing now for visibility; do not start until the astar M1 spike reports whether the app-layer session is sufficient.

### iax-9edf — Emit AnnouncementFinished/failed SSE lifecycle events
*P2 medium · feature · labels: cx:2, node*

Final-review spec gap: only AnnouncementStarted fires in production; AnnouncementFinished exists (TUI/serde) but is never emitted, and 'failed' is absent. SSE 'ANNOUNCING…' UI lights but never clears. Have the controller observe Manager announcement completion (poll_announcements) and broadcast Finished/failed. Spec §Triggers wants started/finished/failed.

### iax-be48 — SP-2: reachable node delta — asl3 register-host helper + ops/reachability doc + live AllStar bring-up
*P2 medium · task · labels: asl3, astar, cx:3, node*
**Blocked by:** iax-a1fb (closed)

Follow-on to the always-on node (SP-1). Register-as-node is ~80% built (iax-bc14 Registrar + iax-64b6 P7 wiring + refresh cadence). Remaining: (1) asl3 helper to resolve/provide AllStar's REGISTRATION host (today RegisterConfig.peer is a raw SocketAddr); (2) ops/reachability doc (UDP 4569 port-forward, public/DDNS IP); (3) live bring-up: register a real node number+secret and confirm an inbound call lands (iax-6461 steps 3 & 5). Depends on SP-1 for always-on-while-dialing.

### iax-c0bc — flaky: announce_service tests race with NoActiveCall (auto-unkey / queue-drain)
*P2 medium · task · labels: cx:2*

**Design:** crates/astar-iax/tests/announce_service.rs is flaky on clean master (observed 2026-07-06 during iax-2f2c gates): finished_to_air_announcement_auto_unkeys_when_not_operator_keyed and queue_drain_preserves_was_operator_keyed_across_fifo_chain intermittently panic with 'called Result::unwrap() on an Err value: NoActiveCall' (announce_service.rs:47 / :154). ~2-4 failures per 10 runs, different test each time — a timing race in test setup (announce issued before the call is active). Unrelated to iax-2f2c (reproduced with its diff stashed).

### iax-c308 — C-iaxclient call scenarios: capture against real ASL3 hub
*P2 medium · task · labels: cx:2, fixtures, rfc-audit, test*

**Design:** Sub-ticket of iax-7022: capture call_notoken, call_token, call_ulaw, and peer_hangup scenarios from the patched-C iaxclient against a real ASL3 hub (not a local Asterisk-in-Podman). Required because the patched-C iaxclient's CALLTOKEN-resend flow does not reset oseqno after dcallno=0 reset; our local strict Asterisk emits VNAK to the resent NEW and call setup stalls. Real ASL3 apparently tolerates this. Prereq: hub access (private throwaway node or write a tolerance test against a known-good hub). Output: 4 .pcap files under crates/astar-conformance/fixtures/c-iaxclient/ plus companion .md describing each flow.

### iax-c648 — live-parrot integration test (backgrounded against ASL3 podman)
*P2 medium · task · labels: cx:3, integration, live, test, tests*
**Blocked by:** iax-bbc6 (closed), iax-6813 (closed)

**Design:** End-to-end live test: ASL3 server in podman ↔ astar-cli ↔ default audio devices ↔ parrot.

Script: scripts/live-parrot.sh
  1. Start podman ASL3 (iax-6813 harness) in background
  2. Wait for IAX registration port + parrot extension to be answerable
  3. Launch astar-cli parrot subcommand in background, feeding test tone or mic
  4. Sample meter/jitter stats for N seconds
  5. SIGTERM the CLI, gather logs, tear down podman
  6. Report PASS/FAIL based on round-trip audio energy threshold

Runs MANUALLY (not in CI by default). Always backgrounded — the live session is long-running.

Acceptance:
  - Single-command invocation produces measurable round-trip audio
  - All processes (podman, CLI) cleanly terminate on exit / Ctrl-C
  - Logs land in target/live-parrot/<timestamp>/

### iax-cb69 — Cutover plan: astar swaps from patched C iaxclient to Rust implementation
*P2 medium · task · labels: cx:3, migration, strategy*

**Design:** Coordinate the actual swap once astar-sys is feature-complete enough for astar.

Phases:
  1. Side-by-side: iax-probe crate (already in astar workspace) gets a feature flag to run our stack vs vendored C. Compare wire traces.
  2. Soft cutover: astar-astar-sys depends on our static lib in a branch. Internal dogfood for 1-2 weeks.
  3. Hard cutover: vendor/iaxclient deleted from astar. astar-astar-sys → thin wrapper around iaxclient (high-level) crate, or deleted entirely.
  4. Pure Rust API migration: astar-core moves from astar-sys to iaxclient (Option C from strategy ticket).

Risk gates:
  - Phase 2 entry: 24h harness soak passes
  - Phase 3 entry: side-by-side audio quality A/B blinded acceptable
  - Phase 4 entry: astar-core refactor reviewed

References:
  - astar nugget astar-4e91 (long-term pure-Rust IAX2 — this IS that nugget's execution)
  - astar/crates/iax-probe (already exists — A/B test home)

### iax-d111 — DTMF audio sources: mic-capture + RX in-band decode -> router
*P2 medium · feature · labels: audio, cx:3, dtmf, station*
**Blocked by:** iax-3746, iax-68eb (closed)

**Design:** Wire the Goertzel decoder (iax-68eb) into two audio taps feeding the Station DTMF router (iax-3746).

1. Mic-capture in-band decode: some handset mics generate DTMF tones directly into the capture stream. Decode them off the mic input and feed the router. Tone squelch: when a mic-audio digit is routed OUT-OF-BAND, notch/suppress the tone from TX so it is not double-sent as both tone and frame. When routed IN-BAND, pass the original tone through (lowest latency, no regen).

2. RX in-band decode: decode tones the remote node sends in the decoded RX audio (the AllStar in-band *-command case) -> StationEvent::DtmfDetected.

Both decoders run in the call/audio thread at 8 kHz (before resample).

Tests: piped fixture audio with embedded tones decodes to the right digits; squelch removes the tone from TX when out-of-band; no false positives on voice.

Depends on iax-68eb + iax-3746.

### iax-d396 — CALLTOKEN-bearing register pcap fixtures (register_token.pcap, register_reject_token.pcap)
*P2 medium · task · labels: cx:2, test,pcap,calltoken,registration, tests*

**Design:** Capture ground-truth pcap fixtures for the CALLTOKEN-bearing registration handshake against an ASL3 Asterisk peer.

# Why deferred
iax-bc14 (Registration FSM) implementation plan defers these fixtures because they require harness extension to drive CALLTOKEN-on-REGREQ. The base iax-bc14 plan ships state-progression tests against the existing `register.pcap` and `register_reject.pcap` fixtures (no token); CALLTOKEN-bearing variants need their own capture cycle, following the iax-7022 pattern.

# Scope
- Extend the AllStar-in-Podman harness (iax-6813) to provoke a CALLTOKEN challenge on REGREQ.
- Capture `register_token.pcap`: client sends REGREQ → server replies REGAUTH with CALLTOKEN IE → client sends REGREQ#2 echoing the token → server completes with REGACK.
- Capture `register_reject_token.pcap`: same handshake but server emits REGREJ after the tokened REGREQ#2 (e.g., bad auth response).
- Place fixtures under `crates/astar-iax-core/tests/fixtures/` following existing conventions.
- Add replay tests in the bc14 FSM that drive these new pcaps end-to-end.

# Acceptance
- Both pcaps committed and parseable by the existing pcap-replay harness.
- New replay tests assert state-progression through the CALLTOKEN path (no byte-equality on MD5).
- `cargo test --workspace` green.

# References
- iax-bc14: registration FSM (state-progression assertions for the non-token path)
- iax-7022: prior ground-truth pcap capture pattern (the model for this work)
- iax-6813: AllStar-in-Podman harness
- RFC 5456 §8.6: CALLTOKEN handshake

### iax-e13e — Live AllStar bring-up: connect my node to another node over the tunnel (closes epic)
*P2 medium · task · labels: astar, cx:3, linking, node*
**Blocked by:** iax-be48

End-to-end: *3<peer> connects, two-way audio passes, *1 disconnects against a REAL AllStar peer over the WireGuard tunnel (iax-99ae). Absorbs iax-be48 reachability bring-up; optional POKE self-check (iax-b764). Closing nugget for iax-d829.

### iax-e549 — CTCSS encode/decode: sub-audible PL tone detect (Goertzel) + tone inject in astar-audio
*P2 medium · feature · labels: audio, ctcss, cx:2, dsp*

**Design:** New DSP primitive in astar-audio (alongside denoise/filter/dynamics). Decode: detect presence of a sub-audible CTCSS/PL tone via Goertzel at a configured tone frequency (the standard EIA set ~67.0-254.1 Hz) for receiver access gating, emitting a tone-present signal. Encode: inject the configured PL tone into TX audio.
Mechanism only: the ACCESS POLICY (carrier-only vs CTCSS-only vs AND/OR combining) lives in the consumer/app; this ticket exposes the detector + a simple tone-present output and the encoder. Surfaced to other languages by Ticket 1's binding.

### iax-fd14 — Wire local radio into the conference when include_local_radio=true
*P2 medium · feature · labels: audio, conference, cx:2, node*

**Design:** iax-647d plumbed include_local_radio end-to-end as config/state and the Conference engine fully supports + unit-tests local_mic/local_out, BUT the Manager builds the conference with local_mic:None, local_out:None — so include_local_radio=true is currently INERT (no local mic into the mix, no local speaker monitor). Inert at the daemon default (local radio OFF), so no default behavior changed. Follow-on: when include_local_radio=true, wire the AudioRouters local capture (mic) Receiver into Conference.local_mic and a local speaker Sender into Conference.local_out, so a lone connected user can reach the local radio and the operator hears the conference.

### astar-vercheck — "A newer version is available", in the popover footer
*P3 low · feature · labels: app, cx:3, macos, ui*

The published half is done: `https://rcludwick.github.io/astar/api/v1/releases.json`
is generated by `ci/version_manifest.py` on every docs build and carries every
release with a `version`, a `semver`, an ascending `ordinal`, a date, a
changelog anchor and a GitHub release URL. Full design, including the schema
and the reasoning behind each field: `docs/design/version-manifest.md`.

This ticket is the client that reads it — the macOS app first, then the Iced
client and `astar-server`.

**The indicator lives in the popover's bottom bar** — `MenuPopover.swift`,
`private var footer`, beside the `Text(Self.appVersion)` that already shows the
running build's `CFBundleShortVersionString`. The footer already answers "what
am I running"; "and there is a newer one" is the same question's other half, in
the place a user already looks. Mind the layout constraints the existing
comment records: the window is resizable from `minWidth: 310` with a
user-controlled text size, and the version `Text` is `fixedSize()` +
`lineLimit(1)` with the `Spacer` absorbing slack precisely so nothing truncates
or drifts off-centre. Anything added there must survive the same squeeze — a
small fixed-size symbol or a very short label, also `fixedSize()`, never a
sentence, never taking the `Spacer`'s job. Give it an accessibility label
naming the new version, and make it actionable (open `notes_url` or
`release_url`) — an indicator with nowhere to go is a nag.

**The comparison needs no version parsing.** Match
`CFBundleShortVersionString` against `releases[].version`, take that entry's
`ordinal`, and compare with `current.ordinal`. If the running version is not in
the list — a source build between releases, a withdrawn version — do nothing.

**Settings toggle: `updates.checkForNewVersions` (Bool), default on.** A
checkbox in the Settings pane's `List` (`devicesPane`), in its own small
section near the bottom beside `ConfigTransferView` — application housekeeping,
not radio equipment. Stored in `UserDefaults` like `AudioSettings`, in the
`com.aj7hr.astar` domain. The `updates.` prefix is deliberately NOT in
`ConfigArchive.settingsPrefixes` (`audio.`, `m17.`, `serial.`), so the
preference does not travel in an exported `.astarconfig`: importing someone
else's station setup must never switch their machine's outbound requests on or
off. It is not `ui.` either — that slice is window state. **Adding this key
does NOT bump `ConfigVersion.current`**; an absent key reads as the default.
This sentence is here so nobody bumps it by reflex.

**Privacy is a hard constraint.** A bare GET of a static path: no identifiers,
no telemetry, no query parameters (a `?version=` endpoint would be a log of who
runs what — the file carries the whole release list precisely to avoid that),
no custom User-Agent, no server-side logging. Poll at most once per 24 h,
anchored to the last successful check rather than to launch; never on an idle
timer; use `If-None-Match` and never bust the cache.

**Failure is silence, in every mode.** No alert, no error state, no red badge,
no log spam above debug. Unreachable, timed out, 404, non-JSON body, truncated
body, unknown `schema`, missing `current`, empty `releases`, own version absent,
own version newest — all identical to the user: the footer shows nothing extra.
"I cannot tell" and "you are up to date" must look the same, or the absence of
a badge stops meaning anything. Discard a malformed body whole; never salvage a
partial parse, and leave any cached answer alone rather than clearing it into
an error.

Cross-platform (`CLAUDE.md`): the Iced client and `astar-server` read the same
file with the same rules. `astar-server` is headless, so "show an indicator"
probably means a field in the status snapshot and nothing else — certainly not
a log line every 24 hours.

### astar-semver — Make the version strings real SemVer once beta ends
*P3 low · chore · labels: build, cx:2, docs, release*

**DONE 2026-08-29, in `0.1.10-beta`.** Every version string is real SemVer, the
Rust crates included, and the app and workspace spellings have converged on one
identical string. Releases up to `0.1.9beta` are not retrofitted — they are
shipped, tagged and on the releases page, and rewriting a published version is
worse than the inconsistency it fixes.

`0.1.10-beta` was the shape chosen over `0.2.0-beta.1`: it is valid SemVer, and
`0.1.9-beta` < `0.1.10-beta` orders correctly, so nothing needed a minor bump to
escape the old numbering. The glued form was the only actual defect — the
pre-release stuck to the patch number with no separator, so `0.1.10beta` is
newer than `0.1.9beta` while string comparison says the reverse.

**The comparison code needed no change, as predicted.** `normalize()` /
`sort_key()` in `ci/version_manifest.py` already understood both shapes; the
generated manifest puts `0.1.10-beta` at ordinal 11 above `0.1.9beta` at 10,
with legacy entries keeping their display spelling and every entry carrying a
strict `semver` alongside. `normalize()` must keep bridging the old shape
indefinitely: manifests already fetched from the wild contain `0.1.9beta`-shaped
strings forever.

**No schema bump either** — the transition is a content change, not a format
change. A client written against `semver` never has to learn the legacy shape;
one written against `version` still has `ordinal`.

An earlier half of this item is also resolved: the Rust workspace had drifted six
releases to `0.1.3-beta` while the app was on `0.1.9beta`. All five homes —
`apps/macos/project.yml`, `zensical.toml`, `CHANGELOG.md`, the workspace version,
and all 28 `astar-*` path-dep requirements — are now enforced in lockstep by
`ci/version_manifest.py --check`, run by `just ci` and `ci/build-docs.sh`. The
path-deps were the quiet half: Cargo reads a stale requirement as a caret range
the newer crate still satisfies, so resolution succeeds and the wrong number
simply persists.

### iax-0465 — astar-audio: pluggable DSP stage chain (DspStage trait + DspChain)
*P3 low · feature · labels: audio, cx:5, dsp*

**Design:** Phase 2 of the calibration/pluggable-filter idea. Refactor the fixed mic chain (notch bank -> gate -> compressor) into a DspStage trait + an ordered DspChain configured from a profile, so stages can be added / reordered / swapped per mic (future: spectral or ML denoise from iax-267f). Follow-up to the phase-1 characterization (auto-notch + gate). Do once there are enough stages to justify the abstraction.

### iax-09c6 — AstarSerial: manual radio-line assert (test PTT output) for a self-test
*P3 low · task · labels: astar, cx:2, serial*

**Design:** SerialClient today only exposes pttTick(remoteKeyed,rxDb) — the radio output line (RTS) is driven from the debounced operator key input (CTS). For astar serial PTT self-test (astar-9661) this is an OPTIONAL enhancement: a method to assert/release the radio output line DIRECTLY (respecting radioLine + radioActiveHigh) without going through pttTick, so the user can verify the RTS->radio wiring independent of the handset PTT. Add SerialClient.setRadio(on: Bool) (or a timed pulse) over the C-ABI; secret-free; deinit must still fail-safe the line low. NOTE: astar primary self-test needs NO engine change (a guided press-your-handset test that reads the live pttTick key state) — this ticket is the nicer output-only variant. WARNING for consumers: asserting the radio line keys the transceiver = transmits RF. Acceptance: a consumer asserts+releases the radio line for a timed test; release/deinit drops it.

### iax-267f — astar-audio: spectral noise suppression (clean under speech)
*P3 low · feature · labels: audio, cx:5, dsp*

**SUPERSEDED 2026-08-29 by `docs/design/noise-suppression.md`**, which solves the
same problem with a trained network (`nnnoiseless`, a pure-Rust RNNoise port)
instead of a hand-rolled spectral subtractor. Milestones 1–5 and 7 are built;
the evaluation (6) and the two default decisions (8, 9) are open and belong to
that document, not here.

The problem statement was right and is unchanged: the gate only helps in the
pauses, and something has to clean *under* speech. Three things about the
proposed solution were not:

* It is the same problem with a worse tool. Spectral subtraction with a
  minimum-statistics estimator is roughly what RNNoise's ancestors did, and
  RNNoise exists because hand-tuned spectral gain produces musical noise that is
  hard to suppress without also dulling speech. This item names that risk ("a
  floor to limit musical noise") without solving it.
* The tuning burden was the real cost — window length, overshoot factor, gain
  floor, estimator time constants, a dozen judgement calls each needing listening
  tests. RNNoise's equivalents were settled by training on hundreds of hours.
* "Slots into the NoiseReducer chain" is `dsp_quantize`, which runs at 8 kHz.
  Nine of RNNoise's twenty-two band energies sit at or above 4 kHz, so from an
  8 kHz pipeline they measure interpolator output. The stage had to go in the
  capture callback at device rate instead — which is a mistake spectral
  subtraction would have made too, just less catastrophically.

What it was right about and the design keeps: the checkbox does not change, and
the new stage supersedes the gate rather than sitting beside it.

**Original design:** Follow-up to the gate-based noise reduction (iax-a9d7): FFT-based spectral suppression that attenuates the estimated noise floor across the spectrum INCLUDING under speech (the gate only helps in pauses). Overlap-add windowing, noise estimation (minimum-statistics or gate-gated), spectral gain with a floor to limit musical noise. Likely needs an FFT (hand-rolled radix-2 or a small dep). Slots into the NoiseReducer chain ahead of / replacing the gate behind the same checkbox.

### iax-2768 — slin_probe: send full WT NEW (empty CALLTOKEN) first
*P3 low · task · labels: cx:1*

**Design:** Found live vs node 77777 (2026-07-12): the wt-mode probe opens with a minimal NEW carrying only an empty CALLTOKEN IE; peers with requirecalltoken=no (ASL3 default guest path) process that bare NEW and REJECT 'Mandatory IE missing'. The app (builders.rs) sends the full IE set + empty CALLTOKEN up front and works against both token-requiring (55553) and no-token peers. Make the probe match: full WT-shape NEW with token=Some(&[]) first; the existing CALLTOKEN handler already covers the resend. Verified live both ways during the 77777 codec probe.

### iax-2c21 — Device auto-detection: Windows (WASAPI/SetupAPI) detector impl
*P3 low · feature · labels: cross-platform, cx:2, device, windows*
**Blocked by:** iax-7b9d

**Design:** Windows `DeviceDetector` impl: enumerate WASAPI audio endpoints + match serial/USB via SetupAPI (VID:PID). Feeds the iaxclient-devices picker. Depends on the detector trait. Activate when the Windows native client starts.

### iax-40bb — Device auto-detection: Linux (ALSA/udev) detector impl
*P3 low · feature · labels: cross-platform, cx:2, device, linux*
**Blocked by:** iax-7b9d

**Design:** Linux `DeviceDetector` impl: enumerate ALSA audio devices + match serial/USB via udev/sysfs (VID:PID). Feeds the iaxclient-devices picker. Depends on the detector trait. Activate when the Linux native client starts.

### iax-5a5c — DTMF-over-serial source (handset keypad -> router)
*P3 low · feature · labels: cx:3, dtmf, ptt, research, serial*
**Blocked by:** iax-3746

**Design:** Some handsets have no audio keypad — they send DTMF digits over SERIAL (no tones). Convert serial keypad signaling into normalized DtmfDigit events and feed the Station DTMF router (iax-3746), so a serial digit can go out in-band or out-of-band per policy, same as any other source. Natural extension of astar-ptt (the existing UCI150 serial bridge).

NEEDS PROTOCOL RESEARCH FIRST: the serial-DTMF framing is device-specific. Likely first target is the AllScan UCI150 (already used for serial PTT) — but confirm whether/how it emits keypad digits over serial before designing the parser. If no device emits DTMF over serial, this stays parked.

Depends on iax-3746. Low priority until a concrete device + protocol is confirmed.

### iax-6940 — GSM / Speex / iLBC codecs (post-1.0, behind feature flags)
*P3 low · feature · labels: codec, cx:5*

**Design:** Stock iaxclient ships GSM, Speex, iLBC. AllStar uses μ-law by default but nodes can negotiate others. Defer to post-1.0 unless we hit a real-world need.

Options per codec:
  - Pure Rust port (Speex has rust-speex; GSM has 'gsm' crate)
  - FFI to libgsm / libspeex / liblbc (system libs)
  - Skip entirely until measured demand

Scope when picked up:
  - Feature flags: --features codec-gsm, codec-speex, codec-ilbc
  - Codec negotiation in NEW IE handling (IAX_IE_FORMAT, IAX_IE_CAPABILITY)
  - Fall-through to μ-law if negotiation fails

References:
  - vendored iaxclient codec_*.c
  - RFC 5456 §8.7 (capability negotiation)

### iax-6975 — Native engine VOX mode (auto-key the gate from the mic input level)
*P3 low · feature · labels: astar, audio, cx:3, ffi, station, vox*

**Design:** Follow-up to iax-5c30 (which exposes the unkeyed mic input level so a CONSUMER can do VOX). This is the fuller option: the ENGINE owns voice-activity detection and auto-keys the PTT gate from the input level on the audio thread, with proper attack/release + hysteresis, exposed as a station mode over the C-ABI.

WHY (beyond iax-5c30): consumer-side VOX polls the snapshot (~20Hz = ~50ms granularity) and round-trips set_ptt over the FFI, so attack/release timing is coarse and jittery. Native VOX runs in MicLane on the audio thread for tight, smooth keying. Consistent with the source-agnostic PTT design (VOX is just another PTT source, like a button / spacebar / serial line) and benefits ALL consumers, not just astar.

SHAPE (sketch): VoxConfig{ enabled, threshold_db, attack_ms, release_ms, hang_ms } on the mic lane; when enabled, MicLane drives the gate from mic_input_peak (built by iax-5c30) instead of the explicit PTT bool. FFI: iax_station_set_vox(enabled, threshold_db, attack_ms, release_ms, hang_ms) (secret-free; plain numbers). Interplay with explicit set_ptt and with half-duplex etiquette (don't key over active remote PTT) to be designed.

PARKED pending decision (see iax-5c30 open questions Q2): only build if polled consumer-side VOX (after iax-5c30) proves too laggy. Minimal unblock is iax-5c30 alone.

### iax-6c64 — IAX2 encryption (AES key + ENCKEY/ENCRYPTION IEs)
*P3 low · task · labels: cx:2, protocol, rfc-audit, security*

**Design:** RFC 5456 §7.4 / §10 encryption — IAX media encryption via shared-secret AES key derived from auth. Not deployed in astar's network; not required for 1.0. Track here as the catch-all for AES IEs + key rotation if/when needed.

### iax-761b — RFC 5456 audit phase 2: cover §1-3 (intro/conventions/terminology), §10 (IANA), §11+ (refs)
*P3 low · task · labels: cx:2, docs, spec*

**Design:** Extend iax-d649's rfc5456-audit.md to cover the sections excluded from phase 1:

- §1 Introduction (non-normative but flags scope)
- §2 Conventions
- §3 Terminology — useful to map RFC terms to our type names
- §10 IANA Considerations — the IE / subclass registries
- §11+ References, Acknowledgements

These add no wire-affecting requirements but complete the audit so any future RFC update can be diffed cleanly. Defer until phase 1 (iax-d649) lands and stabilizes.

### iax-80c8 — parrot pump calls blocking hangup under the session lock in poll_announcements; move to remove + off-lock hangup
*P3 low · task · labels: cx:1, node*

*(no description recorded)*

### iax-90d1 — Call path optimization (TXREQ family)
*P3 low · task · labels: cx:2, protocol, rfc-audit*

**Design:** RFC 5456 §6.5 call path optimization — TXREQ/TXCNT/TXACC/TXREADY/TXREL/TXMEDIA/TXREJ. Lets two peers cut a middle box out of the media path once both endpoints can reach each other directly. Not needed for ASL3 endpoints (they always terminate calls) but RFC-required to be ignorable. Phase: post-1.0.

### iax-9ec5 — Trunking (RFC §7.1)
*P3 low · task · labels: cx:2, protocol, rfc-audit*

**Design:** RFC 5456 §7.1 trunking — multiplex multiple call legs onto a single UDP stream with shared headers. Single-call ASL3 endpoints do not need this; flag as Won't unless a future use case appears.

### iax-a288 — au: nugget created inside a worktree is invisible to the main checkout until synced
*P3 low · task · labels: cx:2*

**Design:** UX sharp-edge hit while working iax-ceba/iax-d937 (2026-06-22).

REPRO:
1. From inside .worktrees/<id>, run `au add "..."` -> nugget iax-XXXX created (its .au/iax-XXXX.am is written in THAT worktree only).
2. From the main checkout, `au claim iax-XXXX` (or show/list) -> 'Not found.' The nugget is invisible until its .am is committed AND merged/synced into the main checkout's .au.

WHY IT BITES: the CRDT-per-worktree model is correct by design (parallel agents edit nugget state on different branches, git merge reconciles). But `au add` gives no signal that the new nugget is local-to-this-worktree, and `au claim` from another checkout fails with a bare 'Not found.' that looks like the id was mistyped, not 'exists elsewhere, not yet synced'. An agent then can't tell a typo from an unsynced nugget.

SUGGESTED FIXES (pick one+):
- `au add` prints a hint when run inside a worktree: 'created in worktree <id>; run au sync from main (or commit+merge) to surface it elsewhere.'
- `au claim/show <id>` on a miss scans sibling worktrees' .au and, if found, reports 'exists in worktree <path>, not synced' instead of 'Not found.'
- or auto-sync new nuggets to the repo root .au on add.

WORKAROUND USED: stayed in the origin worktree and used `au start` (in-progress, no new worktree) instead of claiming from main.

### iax-b764 — POKE reachability check
*P3 low · task · labels: cx:2, protocol, rfc-audit*

**Design:** RFC 5456 §6.7.1 POKE — lightweight reachability ping that does not establish a call. Useful as a status-check primitive for monitoring. Folds in with iax-a307's PING/PONG work but conceptually separate (POKE happens outside an active call, PING inside one).

### iax-b84f — signal report spoken_codec unknown arm: non-negotiated codecs (G729/GSM/iLBC) would speak 'Codec unknown, 16 bit, 8 kilohertz' (wrong depth+rate); omit the sentence for unknown formats. Latent only — parrot never negotiates them.
*P3 low · task · labels: audio, cx:1*

*(no description recorded)*

### iax-bb01 — RSA authentication (RSA_RESULT IE)
*P3 low · task · labels: cx:2, rfc-audit, security*

**Design:** RFC 5456 §8.6.16 RSA authentication — RSA_RESULT (IE 17) instead of MD5_RESULT. astar uses MD5 + CALLTOKEN exclusively; RSA support is gated behind future need for inter-network federation.

### iax-df6c — FailReason: dedicated DeliveryFailed variant (vs reused Timeout)
*P3 low · chore · labels: cx:2, protocol, refactor*

iax-6c21 made Event::DeliveryFailed terminate the call, but reused FailReason::Timeout { in_state } for retransmit-exhaustion (the no-new-shared-enum-variant guard was in force during the parallel wave). Retransmit exhaustion is arguably distinct from a protocol timeout. Decide whether to mint FailReason::DeliveryFailed { in_state } and route on_delivery_failed to it; update Display (iax-1c24) and any consumers. Pure enum-variant change -> must build --workspace (breaks exhaustive matches).

### iax-e9fb — astar-cli polish + Manager call-state API gaps
*P3 low · task · labels: api, cli, cx:2*

Open questions from iax-bbc6 (astar-cli): (1) Manager::dial returns immediately after spawn with no synchronous 'connecting' confirmation — progress only via CallEvent (Ringing/Answered); consider a connecting/dialing state or a returned handle that can be awaited. (2) Manager::key before Answered: unverified whether the engine buffers or drops pre-answer mic frames; if pre-answer keying should be rejected, expose a Manager-level call-state predicate so the CLI can gate it (currently only snapshot() exposes state). (3) register subcommand is one-shot (deregisters on first Registered); a stay-registered-until-SIGINT mode needs a signal handler + public affordance. (4) note: CallEvent is #[non_exhaustive], RegistrationEvent is not.

### iax-fd95 — live POST /bridge flip across the Parrot boundary doesn't rebuild the conference engine (parrot stays off/on); rebuild or reject
*P3 low · task · labels: cx:1, node*

*(no description recorded)*

### iax-onelane-iax2prefs — Route the IAX2 dial's pref push through push_prefs
*P3 low · chore · labels: audio, console, refactor, cx:2*

Left out of scope by the one-audio-lane refactor (`docs/superpowers/implemented/2026-09-06-one-audio-lane-design.md`). `ConsoleSession::open_voice_route` pushes the standing preference set (gains, DSP toggles, compressor, trim, RX compression, VOX pre-roll, calibrated profile) onto the router through one `fn push_prefs(&self, router, mic, out)`. The IAX2 dial path still does its own eleven-line push at `connect` instead of calling it, because at the point `connect` builds the call the mic/out `MixCallId`s are not yet known the way `open_voice_route`'s caller knows them — the design doc calls this out as "the same eleven-line block `connect` uses for an IAX2 dial, moved into one `fn push_prefs`" but the move only happened on the digital-voice side. Thread the IAX2 dial through `push_prefs` once its call's mic/out are resolved, so there is exactly one place a preference reaches the router, on every path.

### iax-ambe-pipeline — Shared AMBE pending/pacing object for D-Star + YSF
*P3 low · task · labels: dstar, ysf, ambe, refactor, cx:3*

Recorded out of scope in the one-audio-lane design
(`docs/superpowers/implemented/2026-09-06-one-audio-lane-design.md`): "A
shared AMBE pipeline object for D-Star and YSF (they still duplicate
pending-queue/pacing logic; a follow-up once both are stable)." Both
`DstarSession` and `YsfLink` drive the same ThumbDV request/response
protocol — a pending-request queue keyed to the dongle's reply order, plus
timeout/pacing so a stalled dongle degrades to silence instead of blocking
the run loop — and each grew its own copy while D-Star's and YSF's transmit
paths were being built. Now that both are stable (D-Star and YSF transmit
both shipped 2026-09-06), extract one `astar-codec` type both sessions hold
instead of parallel hand-rolled queues, so a pacing fix lands once instead
of twice.

### iax-ratebridge-shared — One `FrameRateBridge` for the 8↔16 kHz edge
*P3 low · chore · labels: audio, codec, refactor, cx:2*

The exact 8↔16 kHz framing recipe now exists twice: `EdgeAudio` in
`crates/astar-iax/src/codec_edge.rs` (bus rate ↔ negotiated wire rate) and
`RateBridge` in `crates/astar-console/src/voice_route.rs` (bus rate ↔ the
digital codecs' fixed 8 kHz). Both are the same four moving parts —
`Resampler1::with_chunk` at one 20 ms frame per pass so an integer ratio
yields one frame out per frame in, a FIFO that pops exactly one output frame
per call (left-padding only through the interpolator's warm-up), a 4th-order
Butterworth anti-alias cascade at the source rate ahead of any downsample, and
the i16↔f32 scaling around them — and each carries its own copy of the `//
iax-62ac` reasoning about why short frames must never reach a fixed cadence.
A drift fix or a filter change has to land in both, and the second copy is
easy to miss.

Hoist it into `astar-audio` as one `FrameRateBridge { from, to }` with a
`convert(&[i16]) -> Vec<i16>` that is exactly one frame in, one frame out, and
have both callers hold one. `astar-iax` is frozen for this, so it wants doing
deliberately rather than opportunistically; the tests that pin the property
(`codec_edge`'s `cross_rate_*_emits_exactly_one_*_frame_*` pair and
`voice_route`'s `a_16k_bridge_frames_exactly_and_keeps_pitch_both_ways`)
should move with it.

### iax-linkfail-idle — a lost reflector link reports Idle, never Failed
*P3 low · bug · labels: m17, dstar, ysf, nxdn, cx:2*

Found 2026-09-07 in the NXDN whole-branch review, and inherited verbatim from
the YSF and D-Star run loops. When the link FSM times out, the run loop breaks
out before `publish()` runs and the post-loop store writes `LinkState::Idle`,
so `Failed` is only ever published from the initial-send failure. The
console maps `Idle | Linking` to `CallStatus::Dialing`, so a reflector that
goes away leaves the popover saying *connecting* with no thread behind it,
and `*-listen`'s `link_state == "failed"` exit path is unreachable. Fix all
three (four) networks together: publish `Failed` on `FsmAction::Timeout`
before leaving the loop, and pin it with a scripted-reflector test that
stops answering polls.

**Update 2026-09-07: DMR is the one that does it right.**
`crates/astar-console/src/dmr.rs` publishes `Failed` on a timeout AND on a
send failure, carrying the step it failed at, and `dmr-listen`'s
`link_state == "failed"` exit path is therefore reachable. That is the shape
to back-port to YSF, NXDN and D-Star — the fix is written, it just lives on
one network.

### astar-nxdn-polish — small NXDN follow-ups from the whole-branch review
*P4 backlog · task · labels: nxdn, app, cx:1*

From the 2026-09-07 review, none blocking: `receiving` is set for data
(non-voice) frames (`nxdn.rs`, store `!end && !packet.data`); a refused
`ptt_request` latches on an audio-less link (settle with the transmit task);
`canDial` and `nxdnTarget` disagree on a directory row whose id is not a
valid NXDN id (`CallSession.swift`, one line); a typed talkgroup only checks
`> 0` where the directory path enforces `1…65519` (`ReflectorAddressDial`);
no UI gate for a missing NXDN ID (Connect enabled, refused on press); no
`connectFailureMessage` arm for `IAX_ERR_NXDN`/`IAX_ERR_YSF` (the engine's
detail does reach the operator, the framing does not); `message_type` in
`astar-nxdn::frame` is returned unmasked where Layer 3 masks `0x3F`;
`read_bit`/`write_bit` duplicated between `astar-codec`'s `ysf` and `nxdn`.

### iax-heard-gate — Heard-history gates enumerate the digital networks by hand
*P4 backlog · task · labels: heard, gui, macos, cx:2*

`CallSession.refreshHeardHistory` (Swift) and `RealConn::snapshot` (Iced)
each OR the five `*_active` flags to decide the link is live, and
`ConsoleSession::heard()` has one `#[cfg]` arm per session
(`crates/astar-console/src/session.rs`). A sixth digital network owes an arm
in all three places or its rows never show, with no compiler error to catch
the omission.

### iax-serde-allfeat — astar-console --all-features does not compile
*P4 backlog · bug · labels: build, ci, cx:1*

`astar-console`'s `serde` feature does not forward `astar-audio/serde`, so
`DenoiseStatus` in `state.rs` lacks `Serialize` under `--all-features`. CI
never runs that combination, so it went unnoticed. Fix is one line in
`crates/astar-console/Cargo.toml`.

### iax-resample1-leadin — `Resampler1::with_chunk`'s doc comment is wrong on the first pass
*P4 backlog · bug · labels: audio, docs, cx:1*

`crates/astar-audio/src/resample.rs` says of `with_chunk` that callers with
fixed-size framing "pass their frame size so one frame in yields exactly one
rate-converted frame out". That is false for the FIRST pass through a freshly
built resampler: rubato's polynomial interpolator has a lead-in, and the first
pass comes up short. Measured while building the Discord bridge's
`FramePipeline`: **948 of 960 samples at 8 kHz → 48 kHz, 954 of 960 at
16 kHz → 48 kHz**, settling to exactly 960 from the second pass on.

The behaviour is fine and the wording is not — it already misled one
implementer into assuming the first Discord frame would be full-length, which
it was not (the fix there is a warm-up pass on silence in
`FramePipeline::new`). Correct the comment to say that steady state is
one-in/one-out and that the first pass is short by the interpolator's lead-in.

Left out of the Discord branch deliberately: `Resampler1` sits on the LIVE IAX
codec edge (iax-62ac), so anything touched there wants its own change and its
own test run, not a drive-by edit inside a feature branch. Nothing about the
codec edge is believed wrong — it primes differently — but that needs checking
before the comment is rewritten to describe it.

### iax-ctlcsrf — DONE 2026-10-12: The control port has no cross-origin defence on any pre-existing route
*P1 high · bug · labels: node, http, security, safety, cx:3*

**Closed.** Step 2 done in `work/server-auth` ("server: the Origin rule on
every mutating control route"): every non-GET route refuses an `Origin`
that does not match `Host` (`Origin: null` included), and a request a
browser sent (`Origin` or `Sec-Fetch-Site` present) unless it is
`application/json`. `curl` sends neither header, so its contract is
unchanged; the release note is in the pending changelog. Tests: every
mutating route refuses a foreign Origin, `null` and the three form content
types; a curl-shaped request still works; over a socket, a cross-site
`text/plain` POST to `/key` is a 415. Original text follows.

STATUS 2026-09-27 — **step 1 is DONE** (`work/astar-slash`, "server: refuse a
foreign Host on every route of the control port"). `host_is_this_node` runs
at the top of `handle_request_with_headers` and in `server.rs::dispatch`
before the `/events` branch, so SSE is covered too; every route answers
`403 {"error":"cross-origin requests are not accepted on this port"}` to a
`Host` that is not `localhost`, a loopback literal or the bound address; the
next commit on the branch ("server: cap request bodies at 64 KiB") caps every
body at 64 KiB. **Step 2 stays open**:
`Content-Type: application/json` (or a token) on the legacy mutating routes —
`/key`, `/unkey`, `/dial`, `/link`, `/hangup`, `/bridge`, `/discord`,
`/secrets`, `/shutdown` — is a contract change for every `curl` client and
still wants its own item and release note. The settings routes shipped
behind `same_origin_json` from day one, so step 2 is only about the routes
that predate them. Original text follows.

`POST /key` (`crates/astar-server/src/http.rs`) is
`execute_and_render(ctrl, NodeCommand::Key)`: it takes an **empty body** and
consults **no header**. So a bare cross-origin form on any page the operator
has open in the same browser keys the transmitter:

```html
<form method="POST" action="http://127.0.0.1:8730/key"><button>click me</button></form>
```

No script, no CORS, no preflight — a form post is a "simple request" and the
browser sends it. The same shape reaches `/unkey`, `/dial`, `/link`,
`/hangup`, `/bridge`, `/discord`, `/secrets` and `/shutdown`; the ones that
want JSON are reachable with `enctype="text/plain"`, which lets a form body
begin with `{`.

Two things limit it and neither closes it. **Reads are not exposed**: no route
ever sends an `Access-Control-Allow-*` header, so the attacker never sees a
response — this is write-only, which for `/key` and `/shutdown` is quite
enough. And **loopback binding is not a boundary**: DNS rebinding points a
name the browser trusts at `127.0.0.1`, after which `Origin` and `Host` agree
with each other and are both the attacker's.

`POST /discord/admin` is the only guarded route (`same_origin_json`, same
file): Host allow-list, `Origin` must match `Host`, and
`Content-Type: application/json`.

**First step, and it breaks no `curl` client:** apply the **Host allow-list**
(`host_is_this_node`, already written and tested in `http.rs`) **port-wide**.
Nothing that posts to this port today sends a `Host` other than `localhost`,
a loopback literal or the node's own bind address, and anything that can set
arbitrary headers could talk to the port directly anyway. That alone closes
rebinding. Requiring `Content-Type: application/json`, or a token, on the
mutating routes is the second step and is a contract change for every existing
client — it wants its own item and its own release note.

### iax-pumpdead — A dead pump thread should take the process down
*P2 medium · bug · labels: node, http, discord, cx:2*

`run_serve` (`crates/astar-server/src/run.rs`) spawns the 50 ms pump ticker and
then drops its `JoinHandle` on the floor — the only use is
`let _ = pump_handle.join();` at shutdown, where a panic is discarded. Nothing
watches the thread while the node is running.

If `pump()` panics, the HTTP accept loop keeps answering: `/status` returns
200, the page looks alive, and **audio, DTMF, announcements and SSE stop
forever**. With the Discord bridge configured it is worse — the gateway's
event channel is drained on the pump, so it grows unbounded with nothing
reading it.

A pump that has died is not a degraded node, it is a node that is lying. Watch
the handle (or a heartbeat the accept loop checks) and exit non-zero so the
supervisor restarts it.

Interim mitigation (2026-09-27): the settings page's **Restart node**
re-execs the daemon, and the restart path spawns a 30 s watchdog that
performs the exec itself if teardown wedges — a `Gateway::drop` that never
returns, say. That gives an operator a way out of a lying node once they
notice it; it does not notice for them. The pump watch this item asks for is
still the fix, and a bounded `Gateway::drop` belongs here too.

### iax-pumpdial — A `/dial` handled on the pump stalls the 50 ms tick
*P2 medium · bug · labels: node, discord, linking, cx:2*

A Discord `/dial` is executed on the pump thread, so the tick blocks for the
whole of the dial: DNS resolution for the target, and an HTTPS round trip to
the AllStarLink portal if a token has to be minted. That is hundreds of
milliseconds at best and a DNS timeout at worst, during which no audio moves.

It is the same shape as the pre-existing DTMF-initiated dial, which blocks the
pump for the same reasons — so this is one fix, not two: move the dial onto a
worker thread and let the pump observe the result. Both paths then stop being
able to stall audio, and the Discord half gets a "dialling…" state it can
already render.

### iax-dischangup — `/hangup` from Discord drops the node's entire link roster
*P2 medium · task · labels: discord, linking, safety, cx:2*

`Commander::hangup_targets` (`crates/astar-discord/src/commander.rs`) asks the
node for `links()` and hangs up **all** of them, falling back to the link it
believes it made only when the roster is empty. The admin page's **Hang up**
button does the same.

So an approved Discord user can drop a link an operator made with `POST /link`
or someone made over the air with DTMF. On a node whose only job is feeding a
Discord channel that is the right behaviour — the roster and "the Discord
link" are the same thing, and a Discord user who can see a link they cannot
clear is worse. On a node that does more, it is a bigger hammer than it looks,
and documented as such in `docs/site/server/discord.md`.

Decide it deliberately: either keep it and say so in the refusal strings too,
or narrow `/hangup` to the link the Commander recorded and give admins a
separate verb for the rest. Hanging up is the safe direction, which is why
this is a design question and not a hole.

Since the M17 monitor landed, the roster `/hangup` drops also includes the
node's M17 leg while it is linked, whether Discord or `POST /m17` started it.

### iax-discphase23 — Discord `/dial m17` and `/dial dstar`, and reflector autocomplete
*P3 low · feature · labels: discord, m17, dstar, cx:5*

`/dial dstar` is registered, parsed and validated, and refuses honestly:
*"D-Star isn't wired into this node's conference yet."* `/dial m17` works.

* **Phase 2 — M17 RX as a conference member: shipped.** The node links to one
  M17 reflector module as a receive-only monitor: `M17Monitor` in
  astar-console, enrolled through `Manager::add_source_member`, which can only
  contribute. `/dial m17` is a second `NodeOps` verb beside
  `dial_allstar_monitor`, receive-only by the same contract, and
  `LinkInfo::network` is a `Network` enum. `POST /m17` is the control-port
  twin. The operator opts in with `[m17]`; see
  `docs/site/server/discord.md`.
* **Phase 3 — D-Star**, still open. It additionally waits on a software AMBE
  decoder: the bridge's natural home is a cloud VPS, and there is no ThumbDV
  on a cloud box. Hardware-only D-Star and a Discord bridge are mutually
  exclusive deployments.

Then **reflector-name autocomplete**, still open. `/dial m17` and
`/dial dstar` take a free-text reflector and a module letter. The node now
carries a bundled M17 snapshot (`crates/astar-server/data/m17-reflectors.json`,
used by `[m17] directory = "bundled"`), which is exactly the list Discord's
autocomplete wants for `/dial m17`; D-Star would need its own. Autocomplete is
a separate interaction type from the command itself, so it is additive — the
commands work without it.

### iax-discarm — Prerequisites for an ARMED Discord bridge, recorded now
*P3 low · task · labels: discord, safety, transmit, cx:5*

Not a proposal to arm anything. These are the conclusions the listen-only
build reached, written down while the reasoning is fresh, so that a later
Discord → radio bridge starts from them instead of rediscovering them.

**The server mute must NOT be the gate.** It is the enforcement mechanism for
the one-open-mic rule today and it is not strong enough to be a safety
boundary: there is a window between a member joining and the mute landing;
nothing is enforced at all while the node is down (whoever held the mic keeps
their unmute and nobody who joins is muted); a moderator can unmute anyone by
hand; and a mute is guild-wide, so it is shared with every other use of that
server.

**The node itself must accept audio only from the current mic holder.** The
gate belongs on the receive side, inside the process that would key the
transmitter, keyed on the Commander's own idea of who holds the mic. Anything
Discord reports is advisory.

**The signal needed for that already exists.** The silence unkey learns "the
holder is sending audio right now" from voice-packet timing per SSRC, and the
holder's SSRC is already tracked. An armed bridge needs the same fact, for a
stricter purpose.

**Forced PTT on the channel was considered and declined** (Rob, 2026-09-19):
denying *Use Voice Activity* on the bridged channel would make Discord's own
client require push-to-talk, which pairs well with an armed bridge but is a
worse experience for a listen-only one. Worth revisiting at arming time, not
before.

### iax-ctlstyle-light — DONE 2026-09-27: the control-port pages follow the system appearance
*P3 low · task · labels: node, http, ui, cx:3*

**Closed.** Done in `work/astar-slash` ("server: light theme for the
control-port pages, plus form controls"), because the settings page was the
first stylesheet change that added form controls and a third page, and a
retrofit after it would have been strictly larger. Every literal colour in
`crates/astar-server/src/assets/style.css` is a custom property on `:root`,
redefined under `prefers-color-scheme: light`; `color-scheme` is
`light dark`; `/`, `/discord/admin` and `/settings` were checked together in
both appearances. A unit test in `crates/astar-server/src/http.rs`
(`style_css_carries_a_light_theme_on_root_tokens`) pins the media query,
the `color-scheme` value and the no-literal-colours rule so the branch
cannot quietly disappear.

Original text follows.

### iax-ctlstyle-light — The control-port pages are dark-only
*P3 low · task · labels: node, http, ui, cx:3*

`crates/astar-server/src/assets/style.css` is a single dark theme, shared by
the status page and the Discord admin page. There is no
`prefers-color-scheme: light` branch and no palette indirection to add one to
— colours are literals at their use sites.

So this is a whole-stylesheet job for both pages at once: lift the colours to
custom properties on `:root`, redefine them under the light media query, and
check both pages. It is not a bug — a dark tool page is a defensible choice —
but "the node's web UI ignores my system setting" is the kind of thing that
reads as unfinished, and both pages have to move together or they will not
match.

### iax-panelrate — Panel redraws are coalesced but have no minimum interval
*P3 low · bug · labels: discord, cx:2*

`crates/astar-discord/src/panel.rs` coalesces redraws correctly: at most one
Discord edit is in flight, and exactly one more afterwards carrying the newest
state. What it does not do is *space them out*. A sustained handover storm —
`/mic`, another `/mic`, `/unmic`, a time-out, repeatedly — produces a
back-to-back edit per settled state, and Discord's edit bucket is roughly
five per five seconds per channel.

Coalescing bounds the burst, not the rate. Past the bucket, serenity's
ratelimiter queues, and the panel shows something that stopped being true
seconds ago — the exact failure coalescing was added to prevent. A minimum
interval between renders (with the trailing render still carrying the latest
state) is the missing half.

### iax-botownid — The bot's own-user-id race
*P3 low · bug · labels: discord, cx:1*

`own_id` in `crates/astar-discord/src/gateway.rs` is learned from READY and is
zero until then. READY precedes `GUILD_CREATE` on the wire, but serenity
0.12.5 dispatches every event handler on its own tokio task, so the
`guild_create` handler that READS `own_id` is not ordered after the `ready`
handler that STORES it: it can see zero for a guild whose voice states have
already arrived.

The consequence is bounded and the reasoning is written out at `is_own_bot`.
It bites only for **this bot's own** voice state, delivered in that window,
with no member resolvable from the event or the guild's member list — a
resolvable member still answers `user.bot`. If it did bite, the node would
server-mute its own bridge, and there is no path back: the bridge is not
approved, cannot be given the mic, and the mute is recorded as the node's own.

Both obvious closes are worse: there is no serenity `cache` in this build (on
purpose — a cache is a copy of a guild this node has no business keeping), and
`get_current_user()` is a REST round trip that would sit between a reconnect
and the `Ready` the Commander rebuilds from. The honest fix is to **gate the
`Ready` emission on `own_id` being non-zero**, so the rule engine never sees an
event it cannot classify.

### iax-discdeadaudio — An audio-only gateway has no channel to report a dead track through
*P3 low · bug · labels: discord, cx:1*

A dead audio track is now detected and surfaced: the channel is told the feed
stopped, and the reason goes to the admin view and `/status`.

Except the channel half. On a bridge with **no `approvers`** — the audio-only
configuration — there is no rule engine and no Commander to call `announce`,
so `TrackSupervisor::events` is `None` and no line reaches Discord. The
failure is still recorded: one `eprintln!` to the daemon's stderr and
`AudioHealth`, which `/status` and `POST /discord` render. But the people in
the channel hear silence and are told nothing, and an operator who is not
watching the log or polling `/status` finds out when somebody asks.

The gateway can post to the channel without a Commander — it already posts
join / leave / failed and the hourly "still connected" that way — so the fix is
to route the dead-track notice through that path when there is no rule engine,
rather than to make a failure notice depend on `approvers`.

### iax-gwidlepoll — An idle persistent gateway that dies is never replaced
*P2 medium · bug · labels: discord, node, cx:2*

`ensure_discord_gateway` replaces a gateway whose runtime thread has died —
but only when something calls it, and the only callers are a `Join` and a
`ProvideSecret`. A persistent gateway exists precisely so the bot can sit in
the channel answering slash commands with nobody joining anything, so on an
idle node a dead gateway is never noticed: the bot is offline, `/status`
reports `commands: false`, and it stays that way until the daemon restarts or
somebody dials.

The pump already runs every 50 ms and already knows whether the gateway is
persistent. Poll `Gateway::is_failed()` there and reconnect, with backoff —
seconds at first, minutes after repeated failures — so a node that cannot
reach Discord at all does not spend its life spawning runtime threads. The
backoff state is one `Option<Instant>` plus a counter beside the existing
`discord_*_due` timers. Take `discord_op` for the reconnect itself, the way
`start_discord_gateway` does, and never block the pump on it.

### iax-gwsplit — `gateway.rs` is ~4200 lines and wants splitting
*P3 low · chore · labels: discord, cx:3*

One file holds the runtime thread, the serenity event handler, the panel's
REST bookkeeping, the join/leave machinery and the track supervision. It is
about 3000 lines of production code and the rest tests, and every one of those
five concerns is separately testable already.

A mechanical split — `runtime` (the command loop and teardown), `handler` (the
serenity `EventHandler`), `panel_rest`, `join`, `tracks` — moves code and
nothing else. It was deliberately NOT done before this branch merged: the file
holds the listen-only glue, and a 3000-line move in the same diff as the
behaviour changes would have made the review that mattered unreadable. Do it
first thing afterwards, as its own commit with no behaviour change in it.

### iax-sbeventstore — songbird re-registers a track with an empty event store
*P3 low · bug · labels: discord, audio, upstream, cx:2*

`TrackWatch` is attached to `Track.events` before `play_only`, which is what
makes track supervision survive songbird's own handling — the earlier form,
`TrackHandle::add_event` after the fact, was lost whenever songbird rebuilt
the track's registration.

One case still loses it. When the driver rebuilds its interconnect
(`rebuild_tracks` in songbird 0.6.0's `src/driver/tasks/mixer/mod.rs`), it
re-registers every live track with a **fresh, empty** `EventStore` — literally
`let evts = EventStore::default();` per track — so handlers attached to the
track go the same way `add_event`'s did. A track dying after an
interconnect rebuild is therefore unsupervised: no `AudioFailed`, no line in
the channel, no `audio_error`.

It needs an upstream fix or a workaround that re-attaches after a rebuild is
observed; nothing in this crate can see the rebuild happen today. Recorded so
the pre-attach is not mistaken for complete coverage.

### iax-adminaudio — The admin page does not render `audio_ok`
*P4 backlog · task · labels: discord, http, ui, cx:1*

`/status` and the `AdminView` both carry the audio fault — `audio_error` wins
over `last_error` in the view for exactly this reason — but the admin page has
no element for it. An operator looking at the page sees approvals, tiers, the
mic and the link, and nothing that says the feed into the channel is dead.

One indicator beside the link state, reading the same field the channel's
"the audio feed into this channel stopped" line reads.

### iax-mutescope — Other bots are never muted, and a moderator's mute is yielded to
*P4 backlog · task · labels: discord, cx:1*

Two deliberate limits of the one-mic rule, written down so nobody "fixes"
them without deciding to.

**Other bots in the voice channel are never muted.** Every voice event with
`is_bot` set is dropped before the rule engine sees it — the bridge's own mute
echoes come back that way, and a bot that reacted to them would fight itself.
So a music bot in the same channel plays over the net unmuted. The one-mic
rule is about people.

**A moderator's mute of the mic holder is yielded to, not fought.** A holder
who arrives `server_muted: true` is read as "a moderator took the mic away by
hand": the node clears the holder rather than re-issuing an unmute, and never
records that mute as its own, so it never lifts it either. A node that argued
with a moderator would be worse than one that steps back.

### iax-gwevent-exh — `GatewayEvent` is public and not `#[non_exhaustive]`
*P4 backlog · chore · labels: discord, api, cx:1*

`astar_discord::GatewayEvent` is re-exported from the crate root and is a
plain enum. Every consumer that matches on it is in this workspace today, so
adding a variant is a compile error we would see immediately — but the type is
part of a published crate's surface, and the next variant (an M17 dial, an
armed-bridge event) is a semver break for anyone outside it.

`#[non_exhaustive]` now costs one line and a wildcard arm in the two matches
that exist. Later it costs a major version.

### astar-discord — A Discord voice channel as a conference member

*P3 low · feature · labels: server, audio, discord, unamblify, safety, cx:8*

A Discord voice channel hears the net. Designed 2026-09-13, narrowed and
settled 2026-09-17; full design in
`docs/superpowers/specs/2026-09-17-astar-discord-design.md` (gitignored, so the
reasoning below is the durable record).

**v1 is listen-only.** Discord hears the net; the net never hears Discord — not
by configuration, because no transmit path is built. Two-way was the original
design and is deferred for one specific reason: Discord has never documented
voice *receive*, songbird implements it by reverse engineering, and it is the
half a two-way bridge depends on. Sending is the documented half, which is why
listen-only is not merely a subset but the low-risk three-quarters.

**It runs inside astar-server**, as a `[discord]` section in `node.toml` behind
a `discord` feature off by default — not as a standalone binary. A standalone
binary dialling in over IAX2 was designed first and rejected: astar-server
already runs on the node VPS, so a second container is more to deploy, not
less.

**The blast-radius argument was originally "v1 never executes the
reverse-engineered code path". That stopped being true with the silence
unkey, and the decision was re-taken on it.** songbird's `receive` feature is
on, so its UDP receive task runs **in-process**, in every `DecodeMode`: it
demuxes and parses incoming RTP and RTCP straight off the network and runs a
per-SSRC jitter buffer over them. `DecodeMode::Pass` skips the cipher and the
Opus decoder — it does not skip the parser. Packets from the network are
parsed inside the node daemon, and no configuration makes that not so.

Rob accepted that on 2026-09-19/20, for the silence unkey, with the cost
stated: it is memory-safe Rust with no `unsafe` parsing surface of our own;
the traffic only arrives on an established Discord voice session, so it is not
an unauthenticated path from the open internet; songbird's threads cannot
unwind into the node, so the realistic worst case is a contained panic that
takes the bridge down and leaves the node on the air; and the Commander's hard
time-out timer is a separate clock that depends on none of it, so a receive
path that dies does not leave a mic open. The cost of *not* accepting it is
that there is no other signal at all for "the holder stopped talking" —
Discord publishes no such event.

**The attachment point is already there.** `Conference::add_member(rx, tx)`
(`crates/astar-audio/src/conference.rs`) takes a pair of PCM channels — nothing
about a member is IAX — and `set_member_relay(id, false, true)` is exactly
listen-only: contributes nothing, receives everything. Mix-minus and
`announce_to_member` need no change, which is why this is a `[discord]` section
rather than a fourth `[bridge] mode`: a Discord bridge *plus* a local radio
would otherwise need a mode name per pairing.

**Shape:**

* New `crates/astar-discord`, library only, the only crate in the workspace
  that knows about tokio: it owns a runtime on its own thread and speaks
  `std::sync::mpsc` to everything else. Inside, songbird for the gateway /
  voice WS / AEAD / RTP behind a `VoiceLink` trait, so tests drive PCM fixtures
  and CI never touches Discord.
* The bridge core is written against a `NetLink` trait — a channel pair, 20 ms
  mono i16 frames — not against the conference. A conference member's
  `(Sender<RxFrame>, Receiver<Vec<i16>>)` and `RawDial`'s `rx_frames`/
  `tx_frames` are the same shape, so an IAX-peer host (which would bridge ASL3
  and HamVoIP nodes too, unlike the in-process one) stays a small later
  addition. Note `raw_dial` has no callers in the workspace yet.
* Audio out: 20 ms mono i16 at 8 kHz → post-filter seam → `rubato` 8k→48k →
  mono duplicated to stereo → 1920 interleaved i16 → Opus.
* `[discord]` in `node.toml` — `guild_id`, `channel_id`, `join_at_start`,
  `announce_on_join` — and **no token**: it arrives as `DISCORD_BOT_TOKEN` or
  over `POST /secrets` under a `discord` username, the namespacing
  `link:<node>` already uses. No `arm`, `key`, `transmit` or `contributes` key
  exists, so the TOML offers nothing that could turn on a transmit path.
* `POST /discord {"action":"join"|"leave"}`, mirroring `/bridge`, with state in
  `/status`, SSE and the TUI. No arm/disarm verbs — there is nothing to arm.
* Identity reuses `[announce]`: `id_interval_secs` drives a periodic ID spoken
  into the Discord channel only, via `Manager::announce_to_member`.
* Supervised tokio thread: a songbird panic marks the link failed and surfaces
  in `/status`; it must not take the node down.

**unamblify gets its first home here.** A `PostFilter` trait shaped to
`~/dev/unamblify`'s `docs/design/realtime.md` — `push_frame` / `pull_frame`,
ONNX, `ll5`/`ll20` lookahead — with **bypass as the only shipped
implementation**, because that project's real-time inference is still
"planned. Nothing built." Running an unproven AMBE post-filter here means it
cannot degrade on-air audio; only Discord listeners hear it. `output_rate()` is
on the trait from the start because the `full` profile emits 16 kHz from 8 kHz
input, and retrofitting that would mean touching the resampler.

**Disclosure is a requirement, not a nicety.** Relaying a radio net into a
voice channel is disclosure-and-consent territory under Discord's developer
policy as well as third-party-traffic courtesy: the bot description and channel
topic have to say so, the bridge speaks a notice into the channel when it
joins, and it posts its own state — connected, disconnected, failed, and an
hourly "still connected" — as text in the voice channel's built-in chat, so
the disclosure persists for anyone scrolling back instead of being heard once.
The bot is registered with **no privileged gateway intents**, so Discord never
sends it message content and it receives no message events at all — which
keeps that disclosure a small claim. Five permissions since slash commands
landed: View Channel, Send Messages, Connect, Speak, Mute Members
(`permissions=7343104`). The one thing it reads is message *ids*: at most once
a minute it fetches the ids of the newest 20 messages in the voice channel's
chat to see whether its own PTT panel is still among them, comparing ids and
never reading, storing or logging content. That needs Read Message History,
usually inherited from `@everyone`; without it the check is skipped and the
panel is simply not re-posted.

Licensing is clear: libopus is BSD-3-Clause, songbird is ISC, tokio is MIT
(rows in `LICENSE-EXCEPTIONS.md` §4 beside `nnnoiseless`). It is not ALL
permissive — seven crates are MPL-2.0 (hpke-rs and symphonia with their
sub-crates; `symphonia-codec-pcm` is asked for directly, for its `pcm` feature)
— but MPL-2.0 §3.3 explicitly allows
distribution in a Larger Work under a secondary licence such as AGPL, so there
is no conflict and nothing to relink.
No LGPL, so none of the relink obligation `codec2` carries. A default-features
build pulls none of it — pin that with a test the way `codec2` is pinned —
but a released server container is built with `--features discord` and so ships
tokio and songbird to operators who never configure a channel, accepted
deliberately on the `codec2-static` precedent.

**Deferred, deliberately:** Discord → radio audio (the undocumented half);
Twitch and YouTube streaming (both need a video track, so that is a rendered
visualisation plus H.264 plus RTMP — a subsystem, not a variation on this);
multi-node fan-out.

### iax-bodydrain — DONE 2026-10-14: an oversize `Content-Length` no longer costs an allocation of that size
*P2 medium · bug · labels: node, http, security, cx:2*

**Closed.** Every HTTP server in the workspace now runs on `crates/astar-http`,
which refuses an announced body over the cap before reading a byte of it and
drains nothing beyond its lingering close's bounded read. tiny_http is out of
the lockfile, and `ci/guard-no-tiny-http.sh` keeps it out. The original report
follows.

The control port caps request bodies at `MAX_BODY_BYTES` (64 KiB): `dispatch`
answers 413 to a `Content-Length` over the cap before reading a byte of body,
reads an unannounced (chunked) body at most one byte past it with
`Read::take`, and the router refuses anything over it. That bounds what
`astar-server` holds. It does not bound what tiny_http does with the unread
remainder.

A body announced with a `Content-Length` over 1024 bytes is read through an
`EqualReader` (`tiny_http-0.12.0/src/request.rs:215`). When the `Request`
drops — inside `respond`, after the response is written — `impl Drop for
EqualReader` (`src/util/equal_reader.rs:62-84`) drains the remainder, and it
starts with `vec![0; remaining_to_read]`: a zeroed buffer the size of
everything announced and not yet read. So:

- every 413 on an announced body, and every 403 on a foreign `Host` that
  carried one, allocates the announced remainder on the worker thread;
- `Content-Length: 9000000000000000000` asks for more than the allocator can
  give, and a failed allocation aborts the daemon — from one unauthenticated
  request.

This predates the cap: an unbounded `read_to_end` whose client closed early
dropped the same reader with the same remainder. The cap made the path
routine; it did not create it.

Candidate directions, none tried yet: a tiny_http whose drain reads through a
fixed-size buffer (a newer release, or a patch offered upstream with a
reproducer first); or refusing an oversize `Content-Length` before tiny_http
builds the `Request`, which means a listener in front of it. Either way the
test is the one `serve_wiring` already has for the 413 — announce a large
body, send none of it — plus a check that the worker's peak allocation does
not scale with the announcement.

### record-echolink-gsm — wire EchoLink GSM frame taps to CaptureMode::Gsm

`CaptureMode::Gsm` (`"gsm"`, 33-byte frames) exists in `astar-record`, but nothing emits it. Whichever of `work/recording` / `work/echolink` merges second wires the EchoLink session to emit `Start/Frame/Pcm/End` frame events for it.

### clock-dedupe — fold nodebench's civil_from_days into astar_record::clock

`harness/nodebench` `time.rs` carries its own copy of Howard Hinnant's `civil_from_days`, and `astar-server`'s `acme.rs` has the inverse (`days_from_civil`). `astar_record::clock` now holds both directions plus RFC 3339 formatting and parsing, and Task 17's logger uses it. Fold the nodebench copy (and the `acme.rs` inverse) into it so the workspace has one civil-time implementation.

### astar-features-list — fill docs/FEATURES.md with every shipped feature
*P2 · docs · labels: docs, cx:3*

`docs/FEATURES.md` was created by the recording work (`work/recording`) with
only its own two rows: recording in the macOS app and recording on the node.
Every other feature astar ships today still needs a row, one each, with its
platforms marked (Windows/Linux "paused" for client features, per CLAUDE.md)
and a link to its page under `docs/site/`. The listener and sign-in rows are
spelled out in `astar-listener-features`. `work/echolink` added its EchoLink
rows when it merged main (2026-10-30).

### dstar-unkey-flake — `losing_the_link_while_keyed_forces_an_unkey_and_stops_transmitting` fails about 1 run in 60
*P2 · bug · labels: dstar, test, flaky, cx:2*

Pre-existing on main, seen while building recording (Task 9).
`crates/astar-console/tests/dstar_session_pipeline.rs` fails about once in
60 runs: `key_up` clears `ptt` before the end-of-transmission frame goes out,
so the test can observe the unkey and the EOT in the wrong order. Decide
which order is right, make the session keep it, and loop the test a few
hundred times to prove the flake is gone.

### console-all-features — `cargo build -p astar-console --all-features` fails
*P3 · bug · labels: console, build, cx:1*

Pre-existing on main. With every feature on, `astar-console`'s `state.rs`
serialises a struct holding `astar_audio::DenoiseStatus`, which does not
derive `Serialize`. `just ci` never builds that combination, so it went
unnoticed. Derive `Serialize` on `DenoiseStatus` (behind the same feature)
or skip the field, and add the combination to a check.

### server-auth-without-stream — the auth-only build fails two ws golden fixtures
*P3 · test · labels: server, auth, stream, cx:1*

Pre-existing (seen at 201757e). `cargo test -p astar-server
--no-default-features --features auth` fails `ws_stream_429` and
`ws_no_upgrade_400` in `golden_tests.rs`: without the `stream` feature
`/stream.ws` answers 503, and the fixtures expect the stream build's 429 and
400. Gate those two cases on `feature = "stream"`, or give the auth-only
build its own fixtures.

### record-disconnect-join — `disconnect()` joins the recorder on the caller's thread
*P3 · task · labels: record, station, macos, cx:2*

`Station::disconnect()` stops a running recording, which joins the recorder
thread and syncs its files. The macOS app calls `disconnect()` from the main
thread, so on a slow folder (a network volume, a sleeping disk) hang-up can
stall the UI while the file is flushed. Move that stop off the caller's
thread (the app's recording queue already exists), or have `disconnect()`
hand the finish to a background thread.

### record-contended-late — contended-tap counters are read only when a recording starts
*P3 · task · labels: record, station, cx:2*

`Station::record_start` gathers `record_contended_counters()` once. A tap
slot opened later in the recording (a new output bus, a mic lane that
appears mid-call) has its contended drops left out of `dropped_frames`. Read
the counters through the slots on each state read, or register new slots
with the running recorder.

### record-folder-toctou — the capture-folder checks can be raced by a folder swap
*P3 · task · labels: record, server, security, cx:3*

The node logger and the tarball refuse symlinked capture folders and open
files with `O_NOFOLLOW|O_NONBLOCK` (`open_regular` in
`crates/astar-record/src/tarball.rs`), but they check the folder path and
then open by path. Someone who can write to the recordings folder could
swap a checked folder for a symlink in between. Close it by walking with
`openat` from a held directory descriptor. Also: duplicate ledger rows
produce duplicate tar entries.

### record-buffer-pool — a full recording tap still allocates on the audio thread
*P3 · task · labels: record, audio, performance, cx:2*

`OutputBus.heard` and `MicLane.sent` copy each callback's samples into a
fresh `Vec` before `try_send`, so a full tap allocates and then drops the
chunk on every callback. Recycle chunks through a small pool that the
recorder returns them to, so a full channel costs nothing on the audio
thread.

### record-minors — smaller follow-ups from the recording work
*P4 · task · labels: record, cx:3*

Deferred during review of `work/recording`; each is small and none is known
to lose a recording:

- `iax_station_record_state_json` falls back to `"{}"` if serialising fails
  (only reachable with a non-UTF-8 file path). Emit a full idle state with
  an error instead, so Swift decodes it.
- `LayoutWriter::write_tick` checks the two directions' lengths with
  `debug_assert` only; in release a mismatch could desync Two mono files.
- A `FrameWriter::finish` failure at stop is logged but not counted in
  `dropped_frames`.
- The PCM aligner measures a gap from the real arrival time, rounded per
  frame, so jitter can add or omit a silent frame. A nominal cursor would
  fix it.
- A partial priming cushion (under three frames) left queued plays stale at
  the next key-up.
- A recorder abandoned in its start window can leave a near-empty file, and
  a stop inside that window can return a stale final state.
- `naming::unique_base` checks then creates (a documented race);
  `index::drop_torn_tail` reads the whole manifest on every append.
- `sanitize_label`'s doc still says 64 characters; it keeps 120.
- The settings page's **Keep at most** row has no inline size check; a bad
  value is caught only when the whole file is validated, as with `[stream]`.
- Tests not yet written: FLAC mid-stream encode failure, drop after
  priming, the 480–500 ms splitter boundary, DMR rotation flush and
  `pump_tx` trim, a later-lane sent tap.
