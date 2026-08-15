# IAX2 / AllStar client landscape — survey

Last updated: 2026-05-30. Tracker: au nugget iax-99b5.

## Executive summary

- The open-source IAX2 client landscape is **shallow and aging**. The original `iaxclient` (libiax2) hasn't had a meaningful upstream commit since the 2010-ish SourceForge SVN era and predates CALLTOKEN entirely. Every active downstream is either a fork bolting on workarounds or a small from-scratch implementation in a modern language.
- The only **modern, AllStar-aware** client with a public, hackable reference implementation of the CALLTOKEN handshake is **DroidStar** (`iax.cpp` rev `1faf794`, May 2025). That commit alone is more useful as a fixture source than any libiax2 fork.
- The only **clean, idiomatic, MIT-licensed** IAX2 library with first-class CALLTOKEN, KEY/UNKEY subclasses, and named frame constants is **`jaracil/iax`** (Go). It is the single best protocol-shape reference for a Rust rewrite — its `frame.go` constants are essentially a ready-made `enum`.
- GitLab is a near-empty quarter for this work. The only relevant hits are **flamesgroup/jiax2** (Java, dormant since 2018-ish), a vestigial **savoirfairelinux/jami-daemon** `libs/iax2/` tree (license-incompatible, removed from active use ~2015), and infrastructure repos (Docker containers, the Debian `pkg-voip-team` Asterisk mirror, GitLab-hosted nmap `iax2-brute.nse`). **No client lives on GitLab that the C/Go/Qt sources don't already cover better.**
- The most reliable docs on the **AllStar-specific** dialect (PTT control, K-status TEXT, *70, NNX) live in the **`AllStarLink/app_rpt` source** plus the now-archived AllStarLink wiki "IAX Text Protocol" page. Treat those as the spec; treat the C source as ground truth.

## Active clients

### DroidStar (nostar/DroidStar)
- **Repo**: <https://github.com/nostar/DroidStar>
- **License**: Not explicitly listed in repo metadata (DroidStar README implies GPLv3-style; verify before borrowing code verbatim).
- **Last commit (IAX-relevant)**: `be37092`, 2025-10-19 — *"Remove vocoder plugin system and skip ASL web authentication when using IAX direct"*. CALLTOKEN itself was added in `1faf794`, 2025-05-13 — *"Add calltoken validation to IAX. This fixes connection problems to many nodes."*
- **Language**: C++ / Qt (71% C++, 17% C, 6% Java for Android shim, 5% QML).
- **AllStar/IAX2 features**:
  - **CALLTOKEN (outgoing NEW)**: Yes, since May 2025. `send_calltoken_request()` (iax.cpp ~298–313) emits `IAX_COMMAND_NEW` with an empty `IAX_IE_CALLTOKEN` IE; on receipt of `IAX_COMMAND_CALLTOKEN` (~856–867) it stores the opaque blob into `m_calltoken`, ACKs, and resends NEW with the populated IE. The `m_regreq` flag (~835) routes the resend either to `send_registration(0)` (REGREQ path) or `send_call()` (NEW path). Citation: <https://github.com/nostar/DroidStar/commit/1faf794>.
  - **REGREQ with CALLTOKEN**: Yes. Registration retries are timer-driven (iax.cpp ~732). New constants `IAX_COMMAND_REGREL = 17`, `IAX_IE_CALLTOKEN = 54`, `IAX_COMMAND_CALLTOKEN = 40` live in `iaxdefines.h`.
  - **PTT KEY/UNKEY**: Yes. `send_radio_key(bool key)` (iax.cpp ~537–560) emits `AST_FRAME_CONTROL` with `AST_CONTROL_KEY` / `AST_CONTROL_UNKEY` subclasses, wrapped by `start_tx()`/`stop_tx()` at ~1005/1014. This is the reference UI-driven PTT path.
  - **DTMF**: Partial. `send_dtmf()` (~509–533) emits **one `AST_FRAME_DTMF` per digit** with its own timestamp — no explicit BEGIN/CONTINUE/END split. Functionally compatible with app_rpt because Asterisk treats a bare DTMF frame as a logical begin+end, but **not RFC 5456 §6.7 conformant**. Don't borrow this.
  - **TEXT (K-status)**: No real handling. `AST_FRAME_TEXT` is ACK'd and silently dropped (~1046–1053). DroidStar never displays connected-node state or K-state.
  - **Jitter buffer**: None worth speaking of. A `std::deque<uint8_t> m_audioq` is drained in 160-sample blocks (~1028–1036). Jitter / loss / OOO counters are tracked only for PONG reporting (~593–616), not for actual reordering or de-jitter.
  - **Codecs**: ULAW only (`AST_FORMAT_ULAW`, line ~490). μ-law encode/decode is hand-rolled at ~118–165.
- **Worth borrowing**:
  - The CALLTOKEN handshake in `1faf794` is the cleanest standalone example. Replicate the *shape* (empty IE → CALLTOKEN response → resend NEW with populated IE → reset oseqno/iseqno to 0 before resend) in Rust unit tests.
  - The `iaxdefines.h` numeric assignments (`IAX_COMMAND_CALLTOKEN = 40`, `IAX_IE_CALLTOKEN = 54`, `IAX_COMMAND_REGREL = 17`) are authoritative cross-checks.
  - The KEY/UNKEY codepath is the only reference for "what does an IAX client running app_rpt PTT actually look like on the wire". Capture that as a fixture.
- **Pitfalls / what they got wrong**:
  - DTMF without BEGIN/END split is technically out of spec; under high jitter Asterisk can split a single keypress into two events. Don't replicate.
  - Hard-coded ULAW means it doesn't negotiate at all — it just refuses anything else. Fine for an HT-style PTT app, useless as a general client.
  - No real jitter buffer. Audio is played as it arrives; out-of-order frames produce audible glitches.
  - Drops TEXT frames silently. The Rust crate must at minimum surface them to the application layer.

### jaracil/iax
- **Repo**: <https://github.com/jaracil/iax>
- **License**: MIT.
- **Last commit**: 2025-09-05 — *"Add documentation link to README"* (recent activity through 2025; ~61 commits total).
- **Language**: Go (91.6%); small Lua and shell helpers.
- **AllStar/IAX2 features**:
  - **CALLTOKEN**: Yes, peer-configurable via `peer.EnableCallToken`. In `call.go`, `makeDialFrame()` attaches `StringIE(IECallToken, callToken)` (empty on first attempt). On receipt of `IAXCtlCallToken` (`0x28`), the response token is stored, `iseqno`/`oseqno` reset to 0, and the NEW is re-dialed; the loop terminates on `IAXCtlAccept`, `IAXCtlAuthReq`, or `IAXCtlReject`.
  - **REGREQ with CALLTOKEN**: Same machinery applies to peer registration.
  - **PTT KEY/UNKEY**: Frame subclasses defined: `CtlKey = 0x0c`, `CtlUnkey = 0x0d` in `frame.go`. No app-level PTT abstraction (it's a generic IAX2 library, not an AllStar client) — but the constants are present and on the wire.
  - **DTMF BEGIN/END**: Spec-conformant — `FrmDTMFBegin = 0x0c` and `FrmDTMFEnd = 0x01` are distinct frame types; the digit rides in the subclass.
  - **TEXT**: `FrmText = 0x07` defined; library exposes text frames to the app layer (no K-status semantic interpretation, but that's the right layer split).
  - **Jitter buffer**: Documentation references "real-time audio streaming" but does not appear to ship a configurable jitter buffer. Verify in `call.go` before relying on this.
  - **Codecs**: ULAW, ALAW, G.729, GSM, Speex per README. Codec negotiation lives in the framing layer; actual encode/decode is left to the caller (which is the right design).
- **Worth borrowing**:
  - `frame.go` constants are an almost line-for-line template for a Rust `#[repr(u8)]` enum module. The naming is consistent and matches RFC 5456 §6 / §8.6.
  - The CALLTOKEN retry loop (reset sequence numbers, resend NEW, terminate on Accept/AuthReq/Reject) is the cleanest distillation I found of the §8.6 state machine. Translate this directly into a state-machine test.
  - MIT license means you can cite and translate without contamination concerns.
- **Pitfalls / what they got wrong**:
  - No AllStar-specific behavior (K-status TEXT, *70, NNX) — it's a generic Asterisk IAX2 client.
  - Jitter buffer story is unclear; treat as "missing" until proven otherwise.
  - DTMF subclass encoding "the actual character rather than separate continue subtypes" matches RFC 5456 but the parent agent should verify against Asterisk's `iax2_parse_frame()` before locking in.

### libiax2 (zlargon/iaxclient, mike-plivo/iaxclient, sisuani/iaxclient, ACSPRI/iaxclient-1)
- **Repo**: <https://github.com/zlargon/iaxclient> (canonical SVN→git mirror), plus forks listed above.
- **License**: LGPL-2.1 (libiax2 itself); iaxclient wrappers are LGPL too.
- **Last commit**: SVN trunk effectively frozen mid-2010s. The `mike-plivo` fork adds mute-frame features but stops well before CALLTOKEN existed in deployed Asterisk; the `sisuani` fork is a Windows cross-compile cleanup. `zlargon` is a straight `git svn clone` snapshot.
- **Language**: C (libiax2), plus C++ wrappers and platform glue.
- **AllStar/IAX2 features**:
  - **CALLTOKEN**: **No**. Confirmed by reading `lib/libiax2/src/iax.c` — there are no references to `IAX_IE_CALLTOKEN` or `IAX_COMMAND_CALLTOKEN`. This is the *root cause* of "iaxRpt doesn't work on ASL3" and is why the AllStarLink iax.conf docs tell ops to set `requirecalltoken=no` for legacy clients (<https://allstarlink.github.io/config/iax_conf/>).
  - **PTT KEY/UNKEY**: The subclass constants are defined in the Asterisk-derived headers; whether the C app driver above the library actually sends them depends on the consumer (iaxRpt does; Kiax doesn't).
  - **DTMF BEGIN/END**: Yes — modern libiax2 carries both frame types.
  - **TEXT**: Yes — framing is plumbed through, but K-status is never interpreted.
  - **Jitter buffer**: Yes — there's a real adaptive jitter buffer implementation. This is the **one** area where the old C code is genuinely worth studying.
  - **Codecs**: G.711 (μ/A), G.723.1, iLBC, GSM, G.729A, Speex, SLINEAR, LPC10, ADPCM, G.726. The format constants in `iax.c` are the canonical numbering — copy these.
- **Worth borrowing**:
  - The adaptive jitter buffer is the single most important borrowable artifact across the whole survey. Specifically, `lib/libiax2/src/jitterbuf.c` (Tilghman Lesher's implementation) — same algorithm Asterisk uses. Don't try to invent a new one; port the math.
  - Codec ID assignments (`AST_FORMAT_*`) in `lib/libiax2/src/iax.c` are stable across the ecosystem.
  - Wire-format unit tests are sparse but `lib/libiax2/src/iax.c` parses every frame type — useful as a "does Rust agree with C on this byte sequence" cross-check.
- **Pitfalls / what they got wrong**:
  - No CALLTOKEN — dead-on-arrival against any ASL3 hub with default config.
  - The SourceForge governance is gone; nothing will ever land upstream. Any fork is a permanent fork.
  - LGPL-2.1 license requires careful linking story for a Rust crate intended to be dual-licensed; copying logic is fine, copying source verbatim is not.
  - The iaxclient layer's audio I/O (portaudio + sox filters) is dated and should not be reused.

### IAXRpt (Xeletec, orphaned)
- **Repo**: No public source. Original `xeletec.com` server offline since 2019-04-04.
- **License**: Freeware binary distribution, unknown source license.
- **Last commit**: Effectively 2010-ish. Last redistributed binary is hosted on hamvoip.org as `iaxrpt-installer.exe`.
- **Language**: Win32 C/C++ over libiax2.
- **AllStar/IAX2 features**: Inherits libiax2's frame set (no CALLTOKEN) plus a real PTT button mapped to AST_CONTROL_KEY/UNKEY. Renders the connected-node table by parsing K-status TEXT frames — the *only* freely-distributable client that does so.
- **Worth borrowing**: Nothing source-level (no source). Worth running under Wireshark against a test ASL3 hub (with `requirecalltoken=no`) to capture the **K-status TEXT frame stream** — these captures are the highest-value fixtures for the Rust crate's TEXT-channel decoder, because no other client both connects and parses K-state.
- **Pitfalls**: AllStarLink wiki explicitly discourages new use. Treat as a fixture-capture target only.

### SharkRF M1KE (proprietary hardware)
- **Repo**: None. Firmware-only, closed-source.
- **License**: Proprietary.
- **Last commit**: User manual v57 dated 2026-04-20; active firmware development.
- **Language**: Embedded C, likely on STM32-class hardware.
- **AllStar/IAX2 features (inferred from manual)**: Implements IAX2 client *and* AllStarLink node-registration mode. Supports IAX2 voice activity detection. Connector docs at <https://manuals.sharkrf.com/m1ke/web/connectors/iax2.html>.
- **Worth borrowing**: Nothing directly — but the manual describes node-list refresh (every 5 minutes) which suggests the device polls the AllStarLink Stats API, not IAX2 itself, for the node directory. That's a useful architectural data point.
- **Pitfalls**: Closed source. Only useful as a peer to test against.

### DVSwitch Mobile (proprietary Android)
- **Repo**: None — closed-source APK.
- **License**: Proprietary, free.
- **Last commit**: Active 2025 on Play Store.
- **Language**: Java/Kotlin Android over a native USRP+IAX2 core.
- **AllStar/IAX2 features (inferred from groups.io setup docs)**: IAX2 client with macro support and 16-key DTMF pad. Acts like iaxRpt — likely uses BEGIN/END DTMF since macros need it. Also speaks USRP for digital-mode bridging (out of scope here).
- **Worth borrowing**: Nothing source-level. Forum threads occasionally describe protocol-level behaviors (e.g., username-as-CallerID for Web Transceiver auth) worth cross-referencing.
- **Pitfalls**: Closed-source; not a CALLTOKEN reference.

### RepeaterPhone (iOS, paid)
- **Repo**: None.
- **License**: Proprietary, paid app.
- **Last update**: 2024–2025 on App Store.
- **AllStar/IAX2 features (inferred)**: IAX2 client with PTT. Anecdotally works against ASL3 hubs which implies CALLTOKEN support is shipping — but no source to verify.
- **Worth borrowing**: Nothing.

## Reference (server-side) implementations

These are not "clients" but are the canonical state machines we're targeting interop with.

### Asterisk `channels/chan_iax2.c`
- **Repo**: <https://github.com/asterisk/asterisk> (`channels/chan_iax2.c`)
- **License**: GPLv2.
- **Why it matters**: The canonical IAX2 implementation. CALLTOKEN handling is in this file: errors like `"Call rejected, CallToken Support required"` and the auto/yes/no policy logic live here. Cite this when in doubt about wire behavior. The `doc/IAX2-security.txt` summary documents the §8.6 handshake in plain prose: NEW with empty CALLTOKEN IE → server CALLTOKEN message with source call number 0 → client resends NEW with populated IE → server validates SHA1(remote IP, port, timestamp, server-startup-random) within a 10-second window.
- **Note**: The libiax2 bundled inside modern Asterisk has diverged ~15 years from the one in `zlargon/iaxclient`. When the two disagree, Asterisk wins — that's what every real hub runs.

### AllStarLink `app_rpt`
- **Repo**: <https://github.com/AllStarLink/app_rpt>
- **License**: GPLv2 (inherited from Asterisk).
- **Last commit / release**: v3.9.3 released 2026-05-28.
- **Why it matters**: This is where the AllStar dialect lives:
  - `AST_CONTROL_RADIO_KEY` / `AST_CONTROL_RADIO_UNKEY` are app_rpt-specific control subclasses (not standard Asterisk) — these are what fly between linked app_rpt nodes and what the Rust crate must emit/parse for transparent linking. Standard AST_CONTROL_KEY/UNKEY (which DroidStar uses) is the *client-style* PTT signaling; app_rpt-internal linking uses the RADIO variants.
  - K-status TEXT frames are emitted via `ast_sendtext()` from app_rpt and the *70 DTMF command triggers a status broadcast.
  - The "IAX Text Protocol" wiki page (now retired into the AllStarLink Manual) was the only public catalog of TEXT subcommands. The page itself documents: "*70 status DTMF command" triggers each connected node to reply with its keyed status; the result rides in `AST_FRAME_TEXT`. The actual K0/K1 byte format is *not* documented anywhere except the app_rpt source — confirmed by the wiki maintainers as having been reverse-engineered from app_rpt + iaxRpt + WebTransceiver observation.
- **Action item**: Grep `app_rpt.c` for `ast_sendtext`, `K0`, `K1`, and `*70` to extract the actual byte-level format. Don't trust the wiki PDF mirror; it's binary-corrupt at the URL I tried.

## GitLab-specific findings

Per the user's request, I cast a wide net on gitlab.com and any self-hosted instances surfaced via web search. The yield is thin:

### flamesgroup/jiax2 (GitLab)
- **Repo**: <https://gitlab.com/flamesgroup/jiax2>
- **License**: Apache-2.0.
- **Last commit**: 18 commits across 4 branches, created 2017-02-10. No tagged releases. Effectively dormant.
- **Language**: Java.
- **Notes**: Predates CALLTOKEN deployment. Interfaces like `ICall.java` exist. Not worth deep study, but the *interface naming* (`ICall`, `IRegistration`) is a reasonable cross-check for whether the Rust API surface is idiomatic.

### savoirfairelinux/jami-daemon (self-hosted GitLab at git.jami.net)
- **Repo**: <https://git.jami.net/savoirfairelinux/jami-daemon> (under Anubis bot-protection; current direct-tree fetches blocked).
- **License**: GPLv3 (the very reason IAX support was dropped — libiax2's LGPL/license history was deemed incompatible during the SFLphone→Ring→Jami transition circa 2015-2016).
- **Last IAX-relevant commit**: pre-2017. Historical `daemon/libs/iax2/iax-client.h` exists at commit `918047165f378caed771fcaf32ff7be3584add02` but the code is dead.
- **Notes**: Not worth borrowing — it's an old libiax2 vendored copy with the same gaps as zlargon's mirror.

### wt0f/allstarlink_container (GitLab)
- **Repo**: <https://gitlab.com/wt0f/allstarlink_container>
- **License**: Not specified on the project page.
- **Last commit**: ~74 commits, project created 2020-12-18.
- **Notes**: Docker packaging of Asterisk + AllStarLink for hub deployment. Not a client. Useful only as a way to spin up a local ASL3 hub for integration testing against the Rust crate.

### HackingLZ/PhantomPhreak (mirrored to GitLab VoIP topic)
- **Repo**: <https://github.com/HackingLZ/PhantomPhreak> (primary on GitHub; listed under GitLab VoIP topic).
- **License**: Check repo.
- **Notes**: "USB Modem/IAX2 War Dialer". Uses IAX2 as a transport to VoIP.MS for security-research dialing. Not an AllStar client and not interesting for protocol details — it's an application of an underlying IAX2 lib.

### Kali nmap `iax2-brute.nse` (GitLab mirror)
- **Repo**: <https://gitlab.com/kalilinux/packages/nmap>, script `scripts/iax2-brute.nse`.
- **Notes**: Wire-level IAX2 packet construction in NSE/Lua. Useful as a tiny, readable reference for the AUTHREQ challenge/response over the wire if RFC 5456 §8.5.1 leaves any ambiguity.

### Debian `pkg-voip-team/asterisk` (Salsa GitLab)
- **Repo**: <https://salsa.debian.org/pkg-voip-team/asterisk>
- **Notes**: Debian's Asterisk packaging. Just a mirror with packaging metadata; no protocol value beyond knowing which patches Debian carries.

**Verdict on GitLab**: nothing on gitlab.com (or git.jami.net, salsa.debian.org) materially changes the picture. The GitHub-side artifacts (DroidStar, jaracil/iax, app_rpt, asterisk) dominate.

## Non-IAX2 ecosystem references (note and move on)

- **AllScan** (<https://github.com/davidgsd/AllScan>) — PHP/JS dashboard that polls the AllStarLink Stats API and AMI. Not IAX2. Useful only for understanding what node operators see in their browser.
- **Supermon-ng**, **Allmon3** — AMI-over-HTTP dashboards. Not IAX2.
- **AllStarLink ASL-IaxJS** (<https://github.com/AllStarLink/ASL-IaxJS>) — IAX2 *registration server* in Node.js (powers the AllStarLink registration backbone). Returned 404 for me at the moment; the existence and purpose are documented in the AllStarLink Manual's "IAX-Based Registration" page. Server-side, not client, but worth studying for REGREQ/REGREP/REGAUTH wire behavior on the auth side.
- **pyIAX-Register** (<https://github.com/Apprpt-Central/pyIAX-Register>) — AGPL-3.0 Python registration server with pluggable flatfile/kafka/aslold backends. Server-side. Notable file: `iax2/` directory contains hand-rolled IAX2 packet parsing — small, readable, useful as a sanity check for REGREQ parsing.
- **hello-asl** (<https://github.com/brucemack/hello-asl>) — Minimal ASL hub in Python (~25 commits, no license file visible). Implements NEW, CALLTOKEN, AUTHREQ, AUTHREP (with RSA signature verification!), ACK, ACCEPT, RINGING, ANSWER, VOICE (full + mini), STOP_SOUNDS, HANGUP. Does **not** implement PING/PONG, LAGRQ/LAGRP, TEXT, DTMF, KEY/UNKEY. The author's stated goal — "a minimal IAX2 ASL node with no Asterisk or Linux dependency" — is exactly parallel to astar-lib's goal, and the README references RFC 5456 and the Asterisk IAX2-security.txt directly. Worth reading end-to-end as a "what's the smallest CALLTOKEN-aware hub look like" reference.

## Recommendations for astar-lib

**Implement first, in this order:**

1. **Frame typing module** (`enum FrameType`, `enum ControlSubclass`, `enum IaxCommand`, `enum InfoElement`) ported directly from `jaracil/iax`'s `frame.go`. Cross-check against DroidStar's `iaxdefines.h` and Asterisk's `channels/iax2.h`. This is rote work but it's the foundation.
2. **CALLTOKEN handshake** as a state machine: empty-IE NEW → expect CALLTOKEN response with src=0 → reset oseqno/iseqno → resend NEW with populated IE → terminate on Accept/AuthReq/Reject. Reference implementations in DroidStar commit `1faf794` and `jaracil/iax` `call.go`. Same retry logic applies to REGREQ.
3. **AUTHREQ MD5 challenge/response and RSA path**. Borrow the RSA verify pattern from `brucemack/hello-asl` — it's the only modern code I found that does RSA-side validation cleanly.
4. **Adaptive jitter buffer**: port libiax2's `jitterbuf.c` math. Don't invent a new algorithm — the Tilghman Lesher implementation is what Asterisk uses on the other end and matching its behavior is the path of least surprise.
5. **DTMF BEGIN/END** as distinct frame types. Do **not** copy DroidStar's single-frame approach.
6. **AST_CONTROL_KEY / AST_CONTROL_UNKEY** as plain control subclasses (0x0c, 0x0d per `jaracil/iax`). Also surface the AllStar-specific `AST_CONTROL_RADIO_KEY` / `AST_CONTROL_RADIO_UNKEY` variants from `app_rpt` as separate enum values.
7. **TEXT frame plumbing**: surface to the app layer, *don't* drop. The K-status parser is its own layer above the protocol.

**Test fixtures to capture and commit to `tests/fixtures/`:**

- **Wireshark captures of DroidStar v1faf794+ against an ASL3 hub** doing a full CALLTOKEN exchange + NEW + ACCEPT + VOICE + UNKEY + HANGUP. This is the gold-standard wire trace for the happy path.
- **Wireshark captures of IAXRpt against a hub with `requirecalltoken=no`** showing the K-status TEXT broadcast in response to *70. IAXRpt is the only client that both connects and processes these — capture them while it still works.
- **A `hello-asl` server log** of a single CALLTOKEN-validated NEW from a real Asterisk client, for AUTHREQ negotiation cross-checking.
- **Asterisk `chan_iax2.c` constants** (frame types, subclasses, IE numbers) snapshotted with file+line citations into a `docs/protocol/constants.md` so future contributors can re-verify against upstream.

**Avoid:**

- Don't copy libiax2 code verbatim — LGPL-2.1 friction plus a 15-year-stale codebase. Read it, understand the jitter buffer, rewrite.
- Don't replicate DroidStar's "DTMF as a single frame per digit". Use BEGIN/END.
- Don't bother with anything older than `jaracil/iax` for protocol-shape inspiration. Older C code reflects an older spec.
- Don't trust the AllStarLink wiki for K-status byte format — the wiki itself flags it as "observed in the wild" and the canonical page has been retired. Read `app_rpt.c` directly.

## Citations index

- DroidStar CALLTOKEN commit: <https://github.com/nostar/DroidStar/commit/1faf794>
- DroidStar `iax.cpp`: <https://github.com/nostar/DroidStar/blob/main/iax.cpp>
- DroidStar `iax.h`: <https://github.com/nostar/DroidStar/blob/main/iax.h>
- jaracil/iax repo: <https://github.com/jaracil/iax/>
- jaracil/iax `frame.go`: <https://github.com/jaracil/iax/blob/main/frame.go>
- jaracil/iax `call.go`: <https://github.com/jaracil/iax/blob/main/call.go>
- zlargon/iaxclient (libiax2 SVN mirror): <https://github.com/zlargon/iaxclient>
- libiax2 `iax.c`: <https://github.com/zlargon/iaxclient/blob/master/lib/libiax2/src/iax.c>
- Asterisk `chan_iax2.c`: <https://github.com/asterisk/asterisk/blob/master/channels/chan_iax2.c>
- Asterisk `doc/IAX2-security.txt`: <https://github.com/asterisk/asterisk/blob/master/doc/IAX2-security.txt>
- AllStarLink `app_rpt`: <https://github.com/AllStarLink/app_rpt>
- AllStarLink iax.conf manual (CALLTOKEN config): <https://allstarlink.github.io/config/iax_conf/>
- AllStarLink IAX-based registration: <https://allstarlink.github.io/adv-topics/iaxreg/>
- AllStarLink IAX Text Protocol (retired wiki): <https://wiki.allstarlink.org/wiki/IAX_Text_Protocol>
- pyIAX-Register: <https://github.com/Apprpt-Central/pyIAX-Register>
- hello-asl: <https://github.com/brucemack/hello-asl>
- ASL-IaxJS: <https://github.com/AllStarLink/ASL-IaxJS>
- flamesgroup/jiax2 (GitLab): <https://gitlab.com/flamesgroup/jiax2>
- wt0f/allstarlink_container (GitLab): <https://gitlab.com/wt0f/allstarlink_container>
- savoirfairelinux/jami-daemon historic IAX2 tree: <https://git.jami.net/savoirfairelinux/jami-daemon>
- nmap `iax2-brute.nse` (GitLab): <https://gitlab.com/kalilinux/packages/nmap>
- SharkRF M1KE IAX2 connector docs: <https://manuals.sharkrf.com/m1ke/web/connectors/iax2.html>
- RFC 5456 (IAX2): <https://www.rfc-editor.org/rfc/rfc5456.html>
