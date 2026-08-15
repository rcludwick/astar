# RFC 5456 conformance audit

Tracker: au nugget iax-d649.

This audit maps every normative requirement in RFC 5456 onto our
implementation status. See `docs/superpowers/specs/2026-06-04-iax-d649-rfc5456-audit-design.md`
for the methodology (ID scheme, status definitions, evidence format, verification).

Phase-1 scope was agreed as "§4, §6, §7, §8, §9" before we inspected the RFC.
The real RFC sections covering the same semantic content are §6 Peer Behavior,
§7 Message Transport, §8 Message Encoding, §9 Example Flows, and §10 Security —
those are the headers used below.

Phase 2 (au:iax-761b) extends coverage to the remaining sections, in RFC source
order: §1 Introduction, §2 IAX Terminology, §3 Overview of IAX Protocol,
§4 Naming Conventions, §5 IAX Uniform Resource Identifiers, §11 IANA
Considerations, §12 Implementation Notes, §13 Acknowledgments, §14 References.
Phase-2 ticket originally said "§10 IANA" — the actual RFC ToC puts IANA at §11
(§10 is Security, already covered by phase 1). Verified against
https://www.rfc-editor.org/rfc/rfc5456.txt before writing.

Most phase-2 sections are non-normative prose (intro, terminology, IANA
registry, acks, references) and collapse to a single "informational only" row.
§5 (URI scheme) is the one phase-2 section with real normative requirements
that map to new implementation gaps.

Sibling: `iax2-conformance.md` tracks ASL3 dialect quirks (au:iax-f02d) — things
the wire does that the RFC doesn't specify.

## Status legend

- **Implemented** — code + test exist; cited in Evidence
- **Partial** — code exists, test missing OR test asserts a subset
- **Deferred** — known gap, has au tracking ticket (cited in Evidence)
- **Won't** — intentionally not implementing; rationale in Notes
- **N/A** — requirement targets a role we don't play (e.g., server-only); reason in Notes

## §1 — Introduction

Background on why IAX exists, its design properties (§1.1), and known
drawbacks (§1.2). Non-normative except for a single MUST in §1.2 noting that
every IAX node in a call path must support every codec used.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-1.0-01 | Section is informational; no implementation contract beyond the codec-mask remark below | INFO | N/A | | Sets scope only |
| R-1.2-01 | Every IAX node in a call path MUST support every used codec to some degree | MUST | Implemented | `VoiceFormat` | We negotiate via CAPABILITY/FORMAT IEs and only accept FORMAT codes we model; unknown codecs surface as parse failures rather than half-supported calls. Codec impl coverage tracked separately (iax-6940) |
| R-1.2-02 | Codec definition is controlled by a 32-bit mask, capping simultaneous codecs | OBSERVATION | N/A | | Architectural property of the on-wire format, not an implementation choice; honoured by `VoiceFormat` being a `u32`-backed bitset |

## §2 — IAX Terminology

Establishes the RFC 2119 keyword interpretation and defines the vocabulary
used by the rest of the spec (Peer, Call, Calling/Called Party, Context,
Dialplan, Frame, Information Element, Registrant, Registrar). Pure
definitions; no normative behavioural requirements.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-2.0-01 | RFC 2119 keywords interpreted per [RFC2119] | DEFN | N/A | | Reading instruction for the rest of the document; not an implementation contract |
| R-2.0-02 | Glossary terms (Peer, Call, Frame, IE, Registrant, Registrar, etc.) define vocabulary; no normative behaviour | DEFN | N/A | | Term-to-type mapping: Frame→`Frame`/`FullFrame`/`MiniFrame`, IE→`Ies`, Call→`Fsm`+`CallNo`, Peer→`Session`. Cross-reference only |

## §3 — Overview of IAX Protocol

Non-normative narrative: peer-to-peer, control+media over a single UDP
association on a well-known port, binary framing with Full/Mini/Meta classes,
call legs may span heterogeneous protocols, optional path optimization, and a
generic encryption framework. Every claim here is restated normatively in
§6–§10; rows captured there.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-3.0-01 | Single UDP association, well-known port 4569, signaling and media share the path | INFO | Implemented | `Session` | `IAX2_PORT = 4569` defined in `crates/astar-conformance/src/replay.rs`; restated normatively in R-7.0-01 |
| R-3.0-02 | Frame classes: Full (signaling), Mini (media), Meta (trunking / video) | INFO | Partial | `FullFrame`, `MiniFrame` | Full + Mini implemented; Meta/trunking deferred per R-7.1-01 |
| R-3.0-03 | Call may span multiple call legs in different protocols (e.g., SIP↔IAX↔ISDN) | INFO | N/A | | We are always an endpoint, never a relay; restated as R-9.0-07 |
| R-3.0-04 | Generic native-encryption framework | INFO | Deferred | au:iax-6c64 | Restated normatively as R-7.4-01 / R-8.6-08 / R-10.0-06 |

## §4 — Naming Conventions

Defines Call Identifier (the two per-peer call numbers), Number (calling /
called number string), and Username. Only one normative keyword — a MAY about
E.164 conformance of dialplan numbers.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-4.0-01 | Each call leg is identified by two integers, one assigned by each peer | DEFN | Implemented | `CallNo`, `FullFrame` | Source/dest call numbers carried in every full frame; round-tripped by `proptest_roundtrip.rs` |
| R-4.0-02 | Calling/Called Number is a string of digits and letters; peer defines its own dialplan | DEFN | Implemented | `Ies` | CALLED_NUMBER / CALLING_NUMBER IEs are typed `&str`; no length cap imposed beyond IE TLV (255 bytes) |
| R-4.0-03 | A peer MAY define its dialplan per ITU-T E.164 | MAY | N/A | | Dialplan policy lives in the peer (asterisk server / dial config), not in this client library |
| R-4.0-04 | Username is a string used for identification | DEFN | Implemented | `Ies` | USERNAME IE typed as `&str`; consumed by `register` and `Fsm` auth paths |

## §5 — IAX Uniform Resource Identifiers

Defines the `iax:` URI scheme (`iax:[user@]host[:port][/number[?context]]`),
its ABNF, and equivalence rules. Carries real normative requirements: IPv6
literals MUST be bracketed, URI components are case-insensitive except for
`username`, default port is 4569, FQDN form is RECOMMENDED.

We currently do not have a typed `iax:` URI parser — callers construct
`Session` targets from `(host, port, username)` tuples or `SocketAddr` values
directly. Every row below is therefore Deferred pending a dedicated parser
module. No follow-up ticket has been filed yet (flagged in the phase-2 return
report for the ticketing pass).

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-5.1-01 | `iax:` URI syntax per the ABNF: `iax:[userinfo@]host[:port][/number[?context]]` | MUST | Deferred | | No parser exists in `crates/astar-iax-core`; callers pass `SocketAddr` + username directly. Follow-up ticket needed |
| R-5.1-02 | IPv6 host literal MUST be enclosed in brackets per [RFC3986] | MUST | Deferred | | Same gap as R-5.1-01; `Session` accepts `SocketAddr` which already encodes v4/v6, but no `iax:` string parsing path exists |
| R-5.1-03 | FQDN host form is RECOMMENDED whenever possible | SHOULD | N/A | | Choice belongs to the caller / config layer, not the library |
| R-5.1-04 | Default UDP port for `iax:` URIs (and for the protocol) is 4569 | MUST | Implemented | `Session` | `IAX2_PORT = 4569` defined in `crates/astar-conformance/src/replay.rs`; `examples/harness.rs` defaults to `127.0.0.1:4569`. Will need to be re-exported from a future URI parser |
| R-5.1-05 | URI scheme semantics: identifies a resource reachable by IAX2; new-call initiation is the only defined operation | DEFN | Implemented | `Fsm`, `Session` | We initiate calls; we do not register `iax:` as a system URI handler (OS integration is out of scope) |
| R-5.2-01 | URI equality MUST compare components after port-default substitution and case normalisation (except username) | MUST | Deferred | | No parser ⇒ no equality function. Follow-up ticket needed |
| R-5.2-02 | Host in domain form and host in IP form are NOT equivalent even if DNS would resolve them identically | MUST | Deferred | | Will fall out naturally from string-level comparison once a parser exists |

## §6 — Peer Behavior and Related Messages

Governs how IAX2 peers exchange the protocol-level messages that bracket and
control a call: registration, call setup/tear-down, mid-call signaling,
network monitoring, dial-plan queries, and miscellaneous flow control. The
bulk of the FSM lives here.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-6.1-01 | REGREQ MUST carry USERNAME IE; if peer requires auth, server replies REGAUTH with CHALLENGE+AUTHMETHODS | MUST | Implemented | `register`, `md5_response`, `fixtures/asterisk/register.pcap` | Driver-side, not FSM-side. au:iax-bc14 tracks the FSM-side registration state machine |
| R-6.1-02 | Client MUST resend REGREQ with MD5_RESULT (or RSA_RESULT) once challenge received | MUST | Implemented | `register`, `md5_response` | RSA path: au:iax-bb01 |
| R-6.1-03 | Server replies REGACK on success, REGREJ with CAUSE on failure | MUST | Partial | `IaxCommand` | RegAck/RegRej are parsed and recognised; we currently only treat REGACK as success-signal in the driver-level scenario. Full FSM coverage: au:iax-bc14 |
| R-6.1-04 | REGREL releases an active registration | MUST | Implemented | `register` | Final REGREL→ack roundtrip captured in `fixtures/asterisk/register.pcap` |
| R-6.2-01 | NEW MUST include VERSION, CALLED_NUMBER, CAPABILITY, FORMAT IEs | MUST | Implemented | `Fsm`, `Ies`, `fixtures/asterisk/call_notoken.pcap` | Built by FSM `Init→NewSent` transition. **Note**: `CallMode::WebTransceiver` (`call_mode.rs`, `send_capability: false`) intentionally omits the CAPABILITY IE per the ASL3 WT dialect; the MUST applies to standard IAX2 calls only |
| R-6.2-02 | Caller MUST set source call number; callee selects its own and echoes via dest_call | MUST | Implemented | `CallNo`, `FullFrame` | Round-tripped by `fixtures.rs` proptest |
| R-6.2-03 | Receiving ACCEPT moves the call to "in-progress" / Active | MUST | Implemented | `Fsm`, `SessionState` | Verified by `session_loopback.rs` |
| R-6.2-04 | Receiving REJECT must abort the call attempt and surface CAUSE | MUST | Implemented | `Fsm`, `FailReason` | `Rejected { cause }` variant |
| R-6.2-05 | HANGUP must be reliable (ACKed); either side may originate | MUST | Implemented | `Fsm`, `HangupOrigin`, `fixtures/asterisk/peer_hangup.pcap` | Local+Peer paths both covered |
| R-6.2-06 | AUTHREQ carries CHALLENGE + AUTHMETHODS; client responds AUTHREP with MD5/RSA result | MUST | Implemented | `Fsm`, `md5_response`, `AuthMethods`, `fixtures/asterisk/call_token.pcap` | RSA branch: au:iax-bb01 |
| R-6.3-01 | PROCEEDING / RINGING / ANSWER signal call progress between ACCEPT and media | SHOULD | Deferred | au:iax-85e7 | Outgoing client currently treats ACCEPT as connected; intermediate states not surfaced |
| R-6.4-01 | FLASH / HOLD / UNHOLD / QUELCH / UNQUELCH mid-call control frames | MAY | Won't | | astar endpoints do not use mid-call hold/quelch in 2026; reassess if a deployment needs them |
| R-6.5-01 | TXREQ / TXCNT / TXACC / TXREADY / TXREL / TXMEDIA / TXREJ call-path optimization | MAY | Deferred | au:iax-90d1 | Implementation MUST gracefully ignore unrecognised TX* per §6.5; UNSUPPORT path covers it today |
| R-6.6-01 | Tear-down: any peer may send HANGUP; receiver MUST ACK and drop call state | MUST | Implemented | `Fsm`, `Reliability` | Hangup→Closed verified by `session_loopback.rs` |
| R-6.7-01 | POKE: lightweight reachability check, no call setup | MAY | Deferred | au:iax-b764 | |
| R-6.7-02 | PING/PONG during active call for keep-alive | SHOULD | Implemented | `handlers_outbound.rs`, `keepalive.rs` | `IaxCommand::Ping` in Active → PONG reply; `KeepaliveState::on_ping_timer` drives periodic PING+LAGRQ on `TimerKind::Keepalive`; `on_pong_received` samples RTT |
| R-6.7-03 | LAGRQ/LAGRP for one-way latency measurement | MAY | Implemented | `handlers_outbound.rs`, `keepalive.rs` | LAGRQ emitted every keepalive tick alongside PING; LAGRP echoes timestamp back (§6.7.5); handled in Active state |
| R-6.8-01 | DPREQ / DPREP for dial-plan lookups, DIAL for dial-plan-mode placement | MAY | Won't | | astar dials extensions directly via NEW; dial-plan-mode is a feature of phone clients we don't model |
| R-6.9-01 | ACK MUST piggyback on reliable frames; pure ACK frames acknowledge anything outstanding | MUST | Implemented | `Reliability`, `RxOutcome` | `on_frame_in` consumes ACKs; `enqueue` schedules retransmits |
| R-6.9-02 | INVAL signals "I do not recognise this call number" — receiver MUST drop the call | MUST | Implemented | `handlers_outbound.rs` | `IaxCommand::Inval` in Active cancels `TimerKind::Keepalive`, emits `AppEvent::Disconnected { reason: FailReason::PeerInval }`, transitions to `SessionState::Failed(PeerInval)` |
| R-6.9-03 | VNAK signals "I missed at least one of your reliable frames" — sender MUST resend | MUST | Implemented | `Reliability`, `RxOutcome` | `RxOutcome::Vnak` surfaces oseqno; runtime resends in-flight |
| R-6.9-04 | MWI: message-waiting indicator | MAY | Won't | | Out of scope for a voice-call client |
| R-6.9-05 | UNSUPPORT: response when peer sends an unrecognised command | MAY | Partial | `IaxCommand` | Variant defined; FSM does not yet generate UNSUPPORT for unknown opcodes (it currently logs+ignores) |
| R-6.10-01 | Media: voice frames carried as VOICE full-frame (initial codec) then mini-frames at steady state | MUST | Implemented | `MiniFrame`, `FullFrame`, `VoiceFormat`, `fixtures/asterisk/call_ulaw.pcap` | 209-frame µ-law capture replays cleanly |

## §7 — Message Transport

Governs how IAX2 frames are carried on the wire below the message layer:
trunked vs. single-call streams, retransmission timers, NAT considerations,
and end-to-end encryption.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-7.0-01 | All IAX2 frames carried over UDP, default port 4569 | MUST | Implemented | `Session` | Bound by `Session::connect` |
| R-7.1-01 | Trunking: multiple call legs sharing one UDP stream with header compression | MAY | Deferred | au:iax-9ec5 | Single-call client; trunking unlikely needed pre-1.0 |
| R-7.2-01 | Reliable full frames retransmit with bounded RTO and max-attempt cap | MUST | Implemented | `Reliability`, `ReliabilityConfig` | Defaults: 1s initial RTO, 4s max, 5 attempts, exponential backoff |
| R-7.2-02 | After max retries, sender MUST abandon the frame and tear down the call leg | MUST | Implemented | `Reliability`, `Fsm`, `FailReason` | `RxOutcome::GaveUp` → FSM emits `Disconnected { reason: Timeout }` |
| R-7.2-03 | Receiver MUST dedupe by ISeqno and re-ACK duplicates rather than re-dispatching | MUST | Implemented | `Reliability`, `RxOutcome` | `RxOutcome::Duplicate { resend_ack }` |
| R-7.3-01 | NAT: client MUST tolerate peer's apparent source address differing from registered address | SHOULD | Partial | `Session` | We send to the socket the peer used; we do not yet honour APPARENT_ADDR IE on REGACK |
| R-7.3-02 | Periodic keep-alive (PING) recommended for NAT pinhole | SHOULD | Deferred | au:iax-a307 | |
| R-7.4-01 | Encryption (AES via shared secret + ENCRYPTION/ENCKEY IEs) | MAY | Deferred | au:iax-6c64 | Not deployed in astar's network; flag if a use case appears |

## §8 — Message Encoding

Governs the on-wire byte layout: full vs mini frames, frame types, control
and IAX subclasses, the HTML opcode space, the IE TLV format, and audio
codec subclass bitmasks.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-8.1-01 | Full-frame MUST have F bit (0x8000 on first 16-bit word) set; mini-frame MUST have it cleared | MUST | Implemented | `parse`, `encode`, `FullFrame`, `MiniFrame` | Round-tripped by `proptest_roundtrip.rs` |
| R-8.1-02 | Full-frame header is 12 bytes: scallno, dcallno+retrans bit, ts, oseqno, iseqno, frametype, subclass | MUST | Implemented | `FullFrame`, `parse`, `encode` | `FULL_HEADER_LEN` constant + structural test |
| R-8.1-03 | Mini-frame header is 4 bytes: scallno + low-16 ts; payload format implied by last full voice frame | MUST | Implemented | `MiniFrame`, `parse`, `encode` | `MINI_HEADER_LEN`; replay confirms in `call_ulaw.pcap` |
| R-8.1-04 | Retransmission bit (high bit of dcallno) MUST be set on every retransmit of a reliable frame | MUST | Implemented | `Reliability`, `FullFrame` | `retransmission: true` set on every re-enqueue |
| R-8.2-01 | Frame types: DTMF, VOICE, VIDEO, CONTROL, NULL, IAX, TEXT, IMAGE, HTML, CNG | MUST | Implemented | `FrameType` | Enum models all ten codes |
| R-8.2-02 | Subclass value uses compressed-byte encoding for powers of two; full u32 otherwise | MUST | Implemented | `encode_subclass_byte`, `parse_subclass_byte` | Edge cases covered by `proptest_roundtrip.rs` |
| R-8.3-01 | Control subclasses: HANGUP, RING, RINGING, ANSWER, BUSY, ... (per §8.3 table) | MUST | Implemented | `ControlSubclass` | All §8.3 values + Asterisk extensions in scope; ASL `RADIO_KEY`/`RADIO_UNKEY` extensions documented in `iax2-conformance.md` |
| R-8.4-01 | IAX command subclasses 1..40 per §8.4 table (NEW=1 through CALLTOKEN=40) | MUST | Implemented | `IaxCommand` | Includes upstream-only TXMEDIA=38 + RTKEY=39 + CALLTOKEN=40; vendored astar headers stop at 37 — see `iax2-conformance.md` (2026-05-29) |
| R-8.5-01 | HTML command subclasses (URL push, link/unlink) | MAY | Won't | | Audio-only client; HTML opcodes never exchanged |
| R-8.6-01 | IE wire format: 1-byte id, 1-byte length, payload | MUST | Implemented | `Ies`, `parse`, `encode` | TLV; lenient variant for vendor IEs we don't model |
| R-8.6-02 | CALLED_NUMBER (1), CALLING_NUMBER (2), USERNAME (6), CAPABILITY (8), FORMAT (9), VERSION (11) MUST be supported by any caller | MUST | Implemented | `Ies` | All six fields modelled in the typed container |
| R-8.6-03 | AUTHMETHODS (14), CHALLENGE (15), MD5_RESULT (16), RSA_RESULT (17) for auth round-trip | MUST | Implemented | `Ies`, `md5_response` | RSA_RESULT typed but unused (au:iax-bb01) |
| R-8.6-04 | CAUSE (22) carried on REJECT/HANGUP/REGREJ | SHOULD | Implemented | `Ies`, `FailReason` | Surfaced via `FailReason::Rejected { cause }` |
| R-8.6-05 | CALLTOKEN IE (54): empty = "I support call tokens", populated = server-issued token | MUST | Implemented | `Ies`, `Fsm`, `fixtures/asterisk/call_token.pcap` | Documented in `iax2-conformance.md` (2026-05-29) |
| R-8.6-06 | Receiver SHOULD ignore unknown IEs rather than aborting the frame | SHOULD | Implemented | `parse_lenient` | `parse_lenient` skips unknown/malformed; strict `parse` retains failure path for synthetic round-trip tests |
| R-8.6-07 | APPARENT_ADDR (18) on REGACK reveals client's public-side ip:port | SHOULD | Partial | `Ies` | Field parsed as raw bytes; no typed accessor yet |
| R-8.6-08 | ENCRYPTION (43) / ENCKEY (44) IEs negotiate AES media encryption | MAY | Deferred | au:iax-6c64 | Fields modelled; FSM does not act on them |
| R-8.6-09 | Voice quality stats IEs: RR_JITTER..RR_OOO (46..51) | MAY | Partial | `Ies` | Typed fields exist; sender side does not yet populate from `astar-codec::jitter` |
| R-8.7-01 | VoiceFormat subclass bitmask: G711U=4, G711A=8, GSM=2, ..., SLIN16, etc. | MUST | Implemented | `VoiceFormat` | All `AST_FORMAT_*` constants modelled; codec impl coverage tracked separately (iax-6940) |

## §9 — Example Message Flows

These are non-normative illustrations of how the §6/§7/§8 building blocks
compose. We treat them as conformance fixtures: each flow that we support
should have a captured pcap that replays cleanly.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-9.0-01 | Ping/Pong exchange during active call | IMPLIED | Deferred | au:iax-a307 | Will produce a fixture pcap once PING is wired |
| R-9.0-02 | LAGRQ/LAGRP latency probe | IMPLIED | Deferred | au:iax-a307 | |
| R-9.0-03 | Registration flow: REGREQ → REGAUTH → REGREQ+MD5 → REGACK | IMPLIED | Partial | `register`, `fixtures/asterisk/register.pcap` | pcap exists but currently captures 0 IAX datagrams (tshark startup race) — fix tracked under iax-7022 |
| R-9.0-04 | Registration release: REGREL → REGACK | IMPLIED | Partial | `register` | Same pcap-capture issue as R-9.0-03 |
| R-9.0-05 | Call path optimization full sequence (TX* family) | IMPLIED | Deferred | au:iax-90d1 | |
| R-9.0-06 | IAX media call (NEW → AUTHREQ → AUTHREP → ACCEPT → ANSWER → voice → HANGUP) | IMPLIED | Implemented | `call_notoken`, `call_token`, `call_ulaw`, `fixtures/asterisk/call_notoken.pcap`, `fixtures/asterisk/call_token.pcap`, `fixtures/asterisk/call_ulaw.pcap` | Three captured scenarios cover non-token / token / sustained-media variants |
| R-9.0-07 | IAX media call routed via intermediate IAX device | IMPLIED | N/A | | We are always an endpoint, never a relay |
| R-9.0-08 | Peer-initiated hangup mid-call | IMPLIED | Implemented | `peer_hangup`, `fixtures/asterisk/peer_hangup.pcap` | Tests `HangupOrigin::Peer` path |

## §10 — Security Considerations

Governs authentication, anti-spoofing, replay protection, and DoS resistance.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-10.0-01 | MD5 challenge/response authentication using shared secret | MUST | Implemented | `md5_response`, `AuthMethods`, `fixtures/asterisk/call_token.pcap` | Server-issued challenge round-tripped in pcap |
| R-10.0-02 | RSA challenge/response authentication using public-key signatures | MAY | Deferred | au:iax-bb01 | RSA_RESULT IE parsed but not generated |
| R-10.0-03 | CALLTOKEN anti-spoof handshake (§8.6 IE 54 + §6 CALLTOKEN command 40) MUST be honoured when server requires it | MUST | Implemented | `Fsm`, `Ies`, `fixtures/asterisk/call_token.pcap` | `CallTokenReceived` state + seqno reset after token round-trip |
| R-10.0-04 | Client MUST NOT send password in plaintext when AUTHMETHODS advertises stronger options | MUST | Implemented | `Fsm`, `AuthMethods` | We always select MD5 if offered; plaintext PASSWORD never emitted by the FSM |
| R-10.0-05 | Server SHOULD rate-limit call setup to mitigate flood-based DoS | SHOULD | N/A | | Server-side requirement; we are a client |
| R-10.0-06 | Encryption (per §7.4) protects media content in transit | MAY | Deferred | au:iax-6c64 | |
| R-10.0-07 | Source address verification: receiver SHOULD validate that incoming frames come from the call's negotiated peer ip:port | SHOULD | Partial | `Session` | We currently accept any source ip:port for an established call leg; tightening this is a hardening item |

## §11 — IANA Considerations

Registers the `iax:` URI scheme as Permanent per [RFC4395]. Carries no
behavioural requirements for client implementations — every IE id, subclass
value, and AUTHMETHODS bit referenced elsewhere in the RFC is defined inline
in §6/§8 rather than via separate IANA-managed registries. The registry
itself is the obligation of the document editor / IANA, not of an
implementation.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-11.0-01 | `iax:` URI scheme registered as Permanent with IANA | OBSERVATION | N/A | | IANA-side registration; nothing for the client to implement. URI handling itself is audited under §5 |
| R-11.0-02 | No separate IANA registries for IE ids, frame subclasses, AUTHMETHODS bits, or FORMAT codes | OBSERVATION | N/A | | Allocations are document-internal (§6.6, §8.3, §8.4, §8.6, §8.7); extending them requires an RFC update, not an IANA action |

## §12 — Implementation Notes

Practical advice for implementers: threading limits, codec-mask cap,
single-port DoS surface, scalability via signaling/media split, and NAT
considerations when several IAX servers share a NAT. Entirely non-normative;
each topic is restated normatively in §6/§7/§10.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-12.0-01 | Threading: implementations should size worker pools to survive bursts and DoS | INFO | Implemented | `Fsm`, `Reliability` | Runtime is `mio + blocking thread per call`; tokio explicitly rejected (see project memory `runtime choice`). DoS sizing tracked under hardening, not protocol conformance |
| R-12.0-02 | Codec-mask cap (32-bit FORMAT bitset) limits simultaneous codec count | INFO | Implemented | `VoiceFormat` | Same observation as R-1.2-02 |
| R-12.0-03 | Single well-known port (4569) is a DoS amplifier; CALLTOKEN mitigates call-setup floods | INFO | Implemented | `Fsm`, `Ies` | CALLTOKEN handshake covered normatively by R-10.0-03 |
| R-12.0-04 | Splitting media from signaling (TX* family) improves scalability | INFO | Deferred | au:iax-90d1 | Same gap as R-6.5-01 / R-9.0-05 |
| R-12.0-05 | Multiple IAX servers behind a NAT typically need distinct UDP ports | INFO | N/A | | Server-side deployment concern; we are a client |

## §13 — Acknowledgments

Credits the open-source community that developed IAX through Asterisk. No
implementation contract.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-13.0-01 | Contributor acknowledgments only; no normative content | INFO | N/A | | |

## §14 — References

Bibliography split into normative (§14.1) and informative (§14.2) references.
The normative list is what the RFC depends on; an implementation must respect
those upstream specs to the extent it touches the relevant features.

| ID | Requirement | Level | Status | Evidence | Notes |
|----|-------------|-------|--------|----------|-------|
| R-14.1-01 | [RFC2119] keyword semantics | NORM-REF | N/A | | Reading instruction; covered by R-2.0-01 |
| R-14.1-02 | [RFC1321] MD5 used for challenge/response auth | NORM-REF | Implemented | `md5_response` | Wire bits covered by R-6.2-06 / R-10.0-01 |
| R-14.1-03 | [RFC3447] RSA PKCS #1 signatures for RSA auth | NORM-REF | Deferred | au:iax-bb01 | Wire bits covered by R-10.0-02 |
| R-14.1-04 | [AES] Rijndael block cipher for native media encryption | NORM-REF | Deferred | au:iax-6c64 | Wire bits covered by R-7.4-01 / R-10.0-06 |
| R-14.1-05 | [RFC1851] ESP Triple-DES Transform | NORM-REF | N/A | | Cited by the RFC's encryption discussion; no 3DES path is exercised by our client and none is planned |
| R-14.1-06 | [RFC3261] SIP — referenced for terminology (Registrar) and motivational comparison | NORM-REF | N/A | | We are not a SIP implementation; cross-protocol gateways live in the peer (asterisk) |
| R-14.1-07 | [RFC3629] UTF-8 transformation format | NORM-REF | Implemented | `Ies` | String IEs treated as raw bytes; `text.rs` provides UTF-8 helpers for display |
| R-14.1-08 | [RFC3761] ENUM DDDS — IAX URIs may be returned from ENUM lookups | NORM-REF | Deferred | | Requires the §5 URI parser first; flagged in phase-2 report |
| R-14.1-09 | [RFC3986] URI generic syntax — basis for `iax:` URI ABNF | NORM-REF | Deferred | | Covered by §5 rows; same parser gap |
| R-14.1-10 | [RFC4395] URI scheme registration guidelines | NORM-REF | N/A | | Followed by the RFC editors when registering `iax:`; no implementation work |
| R-14.1-11 | [RFC5234] ABNF — syntax notation for §5 URI grammar | NORM-REF | N/A | | Notation only |
| R-14.1-12 | [E164] ITU-T E.164 numbering | NORM-REF | N/A | | Only referenced via the §4 MAY about dialplan numbering; peer responsibility |
| R-14.2-01 | Informative references ([RFC3435], [RFC3525], [RFC3550], [RFC4566], [RFC4733], [RFC4734], [RFC5125], [RFC3932], [html401]) | INFO | N/A | | Background reading; no implementation obligation |
