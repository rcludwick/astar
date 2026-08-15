# AllStarLink interop reference

A conformance reference for making `astar-lib` (and the `astar-server` daemon)
interoperate with real AllStarLink nodes over IAX2. This captures the protocol and
operational behavior a node must implement — not how to parse AllStar's config
files, but how to *behave like* the node those files describe.

Sources are AllStar primary docs, RFC 5456, and the Asterisk IAX2 docs (linked at
the bottom). This is a behavior spec, not a copy of those pages.

> Library-design context: AllStar is a **policy/config layer** on top of plain
> IAX2. `astar-iax-core` stays vendor-neutral; AllStar specifics (node-number
> identity, link modes, DTMF control) live in the policy layers. Keep it that way.

---

## 1. Node-number → host:port resolution (DNS)

A node number resolves through **DNS under `nodes.allstarlink.org`** (AWS Route 53,
synced from the central registration DB):

| Record | Meaning |
| ------ | ------- |
| `A`   | node's current public IPv4 (e.g. `2000.nodes.allstarlink.org → 162.248.93.134`) |
| `SRV` | the port — UDP **4569** |
| `TXT` | `NN/IP/PT` triplet — **debug / human-readable only** |

**Do:** resolve a peer via the `A` + `SRV` records of `<node>.nodes.allstarlink.org`.
**Don't:** treat the `TXT` record as the source of truth — it's diagnostic. (`dig TXT
<node>.nodes.allstarlink.org` is fine for manual lookup only.)

---

## 2. Registration (becoming reachable)

Two mechanisms, both **outbound** from the node:

- **HTTP registration** (current): via `register.allstarlink.org`; the portal records
  the source IP:port and pushes it to the DB → DNS.
- **IAX registration** (legacy): `register => node:password@register.allstarlink.org`
  in `iax.conf` — the classic IAX2 `REGREQ`/`REGAUTH`/`REGACK` flow.

The registrar reflects the node's **NAT-mapped** address. Consequence: registration
**succeeds behind CGNAT even when inbound is unreachable** — `registered: true` does
**not** imply a peer can call you. See [NAT / CGNAT reachability](#6-nat--cgnat-reachability).

---

## 3. IAX2 call exchange (and authentication)

Per RFC 5456 + Asterisk IAX2-security docs.

```
Caller                                   Callee
  | ---- NEW (dialed node) ------------->  |
  | <--- CALLTOKEN ----------------------  |   anti-spoof, NOT auth
  | ---- NEW (+ token, dest_call=0) ---->  |   *** dest_call MUST be 0 ***
  | <--- AUTHREQ (methods) --------------  |   only if callee requires auth
  | ---- AUTHREP (MD5 / RSA / plain) ---->  |
  | <--- ACCEPT -------------------------  |
  | <--- ANSWER -------------------------  |
  | ---- ACK ---------------------------->  |
```

Auth-off path collapses to `NEW`(+token) → `ACCEPT` → `ANSWER` → `ACK`.

**Conformance gotchas (learned against ASL3 parrot 55553):**

- **CALLTOKEN is anti-spoofing, not authentication.** It defeats forged-source
  floods; it does not identify the caller.
- The **token-resent `NEW` must use `dest_call=0`.** ASL3 rejects a non-zero dest
  call on the resend. (Fixed in iax-ff7b.)
- Tolerate the CALLTOKEN round-trip in the dial FSM (the outbound path must accept
  `ACCEPT` after a token resend, not just `AUTHREQ` — fixed in the dial-hang work).

**Node-to-node identity is effectively open/anonymous.** Identity is the node
*number* (registered in the DB); CALLTOKEN is the spoof guard. **Per-node IAX
secrets are opt-in hardening, not the norm** — many public nodes accept calls from
any registered node. The daemon's `auth=off` default interoperates with ASL3.

**Codecs:** AllStar uses **GSM** and **µ-law (ulaw)**. Offer/accept these.

---

## 4. What the AllStar config implies (no need to parse it)

- **`iax.conf`**: `bindport=4569`, the `register =>` line, and per-peer/user or
  `allow anonymous` / `radio` contexts governing who may call in + codec.
- **`rpt.conf`**: the `[<nodenumber>]` stanza defines the node instance — `rxchannel`,
  the `[functions]` DTMF map, and a `[nodes]` block (or DNS) for link-target
  resolution. Permanent links are declared here.

For interop you implement the *behavior*: answer on UDP 4569, speak GSM/ulaw, honor
the link-mode semantics below.

---

## 5. Link modes & DTMF control

The DTMF interface is a **control surface above the IAX2 call** — it makes/breaks
calls and sets their audio direction. Exact codes are **site-configurable** in
`rpt.conf` `[functions]`, but the `ilink` defaults are near-universal:

| DTMF (default) | `ilink` mode | Audio direction |
| -------------- | ------------ | --------------- |
| `*3<node>` | **transceive** (connect) | full two-way — you hear them, they hear you |
| `*2<node>` | **monitor** (receive-only) | you hear them; they don't hear you |
| `*1<node>` | disconnect | tear down the link to `<node>` |

Other `ilink` ops exist (status, disconnect-all, local-monitor, command-mode); the
specific `*7x`-style codes are **config-defined**, not protocol — treat the numbers
as configurable.

**Permanent vs temporary:** temporary links are made/broken at runtime via the DTMF
codes; **permanent links** are declared in config and auto-reconnect. At the wire
level a permanent link is just an IAX2 call the node keeps re-establishing.

**Maps onto our design:** transceive vs monitor is exactly the `AudioRouter`/mixer
split — 1:1 mic→node on transceive, monitor-only on switch (see the multi-node
routing design, iax-42e9).

---

## 6. NAT / CGNAT reachability

IAX2 is NAT-friendly: **one UDP port (4569) for signaling + audio**, and **no IP
addresses in the payload**. Reachability depends on direction:

- **Outbound connect (you dial out):** works through ordinary NAT and most CGNAT —
  the outbound packet opens the mapping and IAX2 keepalives hold it open.
- **Inbound connect (a peer dials you):**
  - Behind your own NAT: **forward UDP 4569** to the node. Works.
  - Behind **CGNAT: you cannot port-forward** → inbound fails. Registration still
    succeeds (it's outbound), so the node looks reachable but isn't.
- **Symmetric CGNAT** also breaks the registrar-advertised address (per-destination
  port mapping), so even the published `host:port` is wrong for new peers.

**STUN doesn't fix it** (and IAX2 has no STUN client anyway): address discovery is
already covered by registration, and that was never the blocker. **TURN's idea
(relay) is the fix**, but IAX2 has no TURN client — so the AllStar-native equivalent
is: **both ends connect outbound to a public-IP rendezvous** (a hub, or a VPS/VPN
tunnel that gives the node a routable address). Rule of thumb: NAT is solved once
both sides make outbound connections to a common reachable point.

> **astar-server** implements the relay client side natively: set `[wireguard]`
> in `node.toml` (userspace boringtun stack, iax-580b — no TUN device, no root)
> and the WHOLE engine (outgoing calls, registrar, inbound listener) rides the
> tunnel, so inbound 4569 arrives via the VPS. The private key is supplied at
> runtime via the env var named by `secret_ref` (default `WIREGUARD_PRIVATE_KEY`,
> never in the file).
>
> **Deferred (follow-up):** A reachability self-check (POKE / external probe to
> confirm inbound UDP 4569 actually arrives over the tunnel after bring-up) is not
> yet implemented.  Track as a follow-up once the tunnel is in production use.

---

## Sources

- DNS/resolution: <https://allstarlink.github.io/adv-topics/dns-servers/>
- Registration: <https://allstarlink.github.io/adv-topics/httpreg/>,
  <https://allstarlink.github.io/adv-topics/iaxreg/>, <https://register.allstarlink.org/>
- IAX2 protocol/auth: RFC 5456 <https://datatracker.ietf.org/doc/html/rfc5456>,
  <https://docs.asterisk.org/Configuration/Channel-Drivers/Inter-Asterisk-eXchange-protocol-version-2-IAX2/IAX2-Security/>,
  <https://github.com/asterisk/asterisk/blob/master/doc/IAX2-security.txt>
- Config: <https://allstarlink.github.io/config/rpt_conf/>,
  <https://allstarlink.github.io/config/iax_conf/>
- Link modes / DTMF: <https://allstarlink.github.io/adv-topics/dtmffunctions/>,
  <https://allstarlink.github.io/adv-topics/permanentnode/>
