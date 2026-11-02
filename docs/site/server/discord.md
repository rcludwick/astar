---
icon: lucide/radio-tower
---

# Discord bridge

A Discord voice channel can join this node's conference as a **listen-only**
member: the channel hears the net, the net never hears the channel. On top of
that, the bot carries slash commands, so people in the channel can point the
node at an AllStar node or an M17 reflector, ask what it is linked to, and
take a shared mic — all without a login on the node itself.

It is behind a cargo feature (`--features discord`) and a `[discord]` section
in `node.toml`. A node with neither is completely unaffected.

!!! danger "Listen-only is a property of the build, not a setting"

    Nothing on this page can put Discord audio on the air, and no combination
    of settings can. The conference member the bridge enrolls is registered
    **non-contributing**, so the mixer has nowhere to take audio from it; the
    `[discord]` section has no `arm`, `key`, `transmit` or `contributes` key,
    and `deny_unknown_fields` turns an invented one into a startup error; and
    the rule engine behind the slash commands is written against an interface
    that cannot express a transmit link, a host or a port. Its dial verbs,
    AllStar and M17, are receive-only by name and by contract.

    The mic described below decides **who may speak in the Discord channel**.
    It is built now so that the rules, the timers and the audit trail are
    proven long before anything is armed.

## Setting the bot up

**1. Create the application.** In the Discord Developer Portal, make an
application, add a Bot, and copy its token — that is the only secret involved.

**2. Leave every privileged intent OFF.** Presence, Server Members and Message
Content are all unnecessary. The bot works entirely from voice state and from
interactions, which need no intent at all.

**3. Invite it** with this URL, substituting your application's client id:

```
https://discord.com/oauth2/authorize?client_id=<APP_ID>&scope=bot+applications.commands&permissions=7343104
```

`7343104` is five permissions: **View Channel**, **Send Messages**,
**Connect**, **Speak** and **Mute Members**.

| Permission | What stops working without it |
|---|---|
| View Channel, Connect, Speak | The bridge itself — it cannot join or be heard. |
| Send Messages | The status lines and the PTT panel. Audio still works; the disclosure and the audit trail silently do not. |
| Mute Members | **The one-open-mic rule.** The bot posts that it is missing the permission and everyone stays unmuted, so the mic becomes a label rather than a rule. |

Re-authorizing a bot that is already in the server **updates its managed role
in place** — nothing has to be kicked and re-added. A bot invited before Mute
Members existed (`permissions=3148800`) needs exactly that.

**4. Collect the user ids of your approvers.** In Discord, turn on
*Settings → Advanced → Developer Mode*, then right-click a user → *Copy User
ID*. Those go in `approvers`.

**5. Give the node the token.** Open the settings page,
`http://127.0.0.1:8730/settings#discord`, and use **Set** beside the bot
token. On a node whose `[secrets] source` is `file` (the default for new
nodes) it lands in `credentials.toml`; under the other sources it applies
until the next restart and the page offers **Move credentials to
credentials.toml**. The two older routes still work:
`DISCORD_BOT_TOKEN` in the environment under `[secrets] source = "env"`, or

```bash
curl -X POST http://127.0.0.1:8730/secrets \
     -d '{"username":"discord","secret":"<bot token>"}'
```

**6. Configure the section:**

```toml
[discord]
guild_id   = "123456789012345678"
channel_id = "123456789012345678"
join_at_start    = false
announce_on_join = true

approvers  = ["123456789012345678"]              # (1)!
state_file = "/var/lib/astar/discord-approvals.json"
mic_timeout_secs = 180                           # (2)!
mic_silence_secs = 5                             # (3)!
```

1.  The master switch for everything below. Empty or absent means no slash
    commands are registered, nobody is muted, and the bot is offline unless it
    is in the voice channel — exactly how the bridge behaved before slash
    commands existed.
2.  Repeater-style hard time-out on the open mic, in seconds. `0` turns it off.
3.  Stop talking for this long and the mic is released. `0` turns it off.

`deploy/node.toml.example` carries the same keys with the full commentary.

!!! warning "Turning `approvers` off again does not un-register the commands"

    The node registers its slash commands on READY, as a bulk overwrite of
    whatever that server had. Take `approvers` back out of `node.toml` and it
    simply stops registering — it never *clears* what a previous run put
    there. The commands stay visible in the server's picker and answer **"The
    application did not respond"**, because nothing is listening for them any
    more.

    Nothing in the node fixes that, so do it in Discord: **Server Settings →
    Integrations →** the bot **→** remove its commands, or remove the app from
    the server and re-invite it.

!!! info "What the bot reads"

    It requests **no privileged gateway intent**, so Discord never sends it
    message content and it receives no message events at all.

    The one thing it reads is message **ids**. To keep its PTT panel from
    scrolling out of sight, it fetches — at most once a minute, and only while
    it is redrawing the panel anyway — the ids of the newest 20 messages in the
    voice channel's own chat, to see whether its own panel is still among them.
    Ids are compared; no content, author or attachment is read, stored or
    logged. That needs **Read Message History**, which `@everyone` usually
    already grants. Without it the check is skipped, logged once, and the panel
    simply is not re-posted.

## The four tiers

Widest first:

| Tier | Where it comes from |
|---|---|
| **everyone** | Anyone who can see the channel. |
| **approved** | Granted with `/approve` or from the admin page. Persisted. |
| **admin** | Every approver, plus anyone added with `/admin add` or from the page. Persisted. |
| **approver** | The `approvers` list in `node.toml`. **Config, not state** — an approver can never be revoked, banned or demoted from inside Discord, and the admin page lists them read-only. Edit them on the settings page (`/settings#discord`) — restart-tier; removing one is behind a confirm that reminds you their slash commands stay registered in Discord until you remove them there. |

**Banned** is separate and beats everything except approver: a banned user is
refused `/register` and holds no tier.

An admin may not act on another admin — only an approver, or the admin page,
can. (An admin may always put their *own* mic down.)

## The commands

Every command is visible to everyone. There is no `default_member_permissions`
anywhere, on purpose: the node answers for itself, ephemerally, and says which
rule you broke — rather than the command vanishing from your UI because of a
Discord permission the node never saw.

| Command | Who may run it | What it does |
|---|---|---|
| `/status` | everyone | What the node is linked to, who has the mic, and whether commands are switched on. For an M17 link it also names the callsign the reflector sees and the last station heard. |
| `/register callsign:<call>` | everyone not banned | Records your callsign and asks the approvers for dial access, posting one line in the channel so they see it. Asking twice does not ping twice. Running it again — approved or not — replaces the callsign, with three limits: once you are approved, a change is announced in the channel (*"@you changed their callsign from AJ7HR to KC1ABC."*); a change is refused while your own M17 link is up (*"Hang up your M17 link first."*); and a callsign already registered to someone else is refused (*"AJ7HR is already registered to another member — ask an approver."*). |
| `/dial allstar node:<n>` | approved | Points the node at an AllStar node, **receive-only**. Refused if the bridge is not in the voice channel, or if anything is already linked. |
| `/dial m17 reflector:<name> module:<letter>` | approved, with a registered callsign | Links the node to one module of an M17 reflector, **receive-only**, presenting your registered callsign — see [Monitoring an M17 reflector](#monitoring-an-m17-reflector). Refused if M17 is not enabled on this node, if the bridge is not in the voice channel, or if anything is already linked. |
| `/dial dstar` | approved | Registered and honest: *"D-Star isn't wired into this node's conference yet."* |
| `/hangup` | approved | Hangs up **every** link, including an M17 monitor once it is linked — see below. |
| `/mic` (or `/mic state:on`) | approved | Takes the mic. You must already be in the voice channel. |
| `/mic state:off` | whoever holds the mic | Gives it up. |
| `/approve` · `/revoke` | admin | Grant or remove approved status. Revoking someone also hangs up an M17 link they dialled, because the reflector sees their callsign. |
| `/ban` · `/unban` | admin | Ban is stronger than revoke: a banned user cannot even `/register`. A ban hangs up an M17 link the person dialled, as revoke does. |
| `/unmic` | admin | Takes the mic off whoever has it. |
| `/bot state:on\|off` | admin | The master switch. |
| `/admin add` · `/admin remove` | **approver** | Make and unmake admins. |

Every decision is also posted as a plain line in the voice channel's text chat
— *"@someone was approved by @someone-else"* — so the channel is the audit
trail. Those lines ping nobody. The node's own log gets every one of them
too, prefixed `astar-server: discord:` and carrying the user id, so the record
does not depend on a Discord message nobody deleted.

!!! warning ""Receive-only" is about the link, not about the room"

    A `/dial` links in **monitor** mode, so nothing from this node and nothing
    from Discord is ever transmitted over the new link — the far end hears
    silence from you.

    But a dial is still a dial *on this node*, and like any dial here (DTMF,
    or `POST /link`) two things reach everyone else already connected to it.
    The node speaks its *"connecting to node N"* announcement onto every
    **other** inbound leg that is a conference member and is not itself a link
    — a web-transceiver or handset user, or a node that dialled in to you —
    and the dialled node's received audio is mixed into the conference, so
    those same peers hear it.

    If one of those peers is itself an RF node connected in transceive, what
    it does with that audio is **that node's** decision, and it may go out on
    its transmitter. This node's own radio PTT is never touched by a Discord
    command — no path from a slash command reaches the keying call at all.

    An M17 dial is the same kind of dial. The reflector's audio is mixed into
    the conference exactly like a monitored AllStar node's, so the same peers
    hear it, and the node speaks *"Monitoring M17 reflector …"* to them as it
    does for an AllStar dial. Reflector audio on its own does not key an
    AllStar link this node holds in transceive. What is new is that an M17
    dial puts a person's identity on a third-party network: for as long as
    the link is up, the reflector lists the dialler's registered callsign as
    a connected client. Nothing is ever transmitted under it, but anyone
    reading that reflector's dashboard can see it.

    So approve people for `/dial` knowing that. (The spoken announcement needs
    something to render it — `[announce.tts]`, or a matching sample; without
    that there is simply no line. `[announce] enabled` is not consulted on
    this path. The conferenced audio is unaffected either way.)

!!! warning "`/bot state:off` does not trap anything"

    The master switch stops `/register`, `/dial` and *taking* the mic. It
    deliberately leaves `/status`, `/hangup`, `/mic state:off` and the admin
    commands working: `/status` has to be able to say *why* nothing works, the
    admin commands have to be able to switch it back on, and hanging up and
    putting the mic down are the two safe directions. Turning the switch off
    also closes an open mic.

## Monitoring an M17 reflector

`/dial m17 reflector:<name> module:<letter>` links the node to one module of
an M17 reflector as a **receive-only monitor**. Whoever talks on that module
is heard in the voice channel and by every conference member that receives
the mix (handsets and inbound nodes; see the warning above for AllStar
links). The only packets the node ever sends the reflector are the three the
protocol needs to hold a link open — connect, the keepalive reply, and
disconnect. The code that does the sending has no way to build a voice packet,
and no `/mic`, PTT or `POST /key` reaches it.

It needs four things before it works:

* **The operator has opted in.** M17 is off until `node.toml` has an `[m17]`
  section — see [Configuration](configuration.md). Until then the answer is
  *"M17 isn't enabled on this node — its operator can add an `[m17]`
  section."*
* **The node knows the reflector.** A name resolves from the operator's own
  `[m17.reflectors]` table, or from the reflector directory the operator
  switched on. Nothing in Discord can supply an address. An unknown name gets
  *"I don't know a reflector called M17-XYZ. The node's operator can add it
  under `[m17.reflectors]`."*, and a module the directory says the reflector
  does not have gets, for example, *"M17-ABC has modules A–D."*
* **The node has a Codec 2 decoder.** Otherwise: *"this node has no Codec 2
  decoder — its operator needs to install libcodec2 (or build astar-server
  with codec2-static)."*
* **You have registered a callsign.** It is the identity the reflector sees,
  so there is no dialling without one: *"M17 needs your callsign — run
  `/register callsign:<yours>` first."*

Refusals that come back from the node arrive prefixed *"Couldn't dial: "*.

What the channel sees when AJ7HR dials module C of M17-USA:

| Moment | Where | Text |
|---|---|---|
| The dial | your reply | *"Dialling M17 M17-USA C as AJ7HR (receive-only)…"* |
| The dial | the channel | *"@you → M17 M17-USA C (receive-only, as AJ7HR)"* |
| The reflector accepts | the channel | *"Connected to M17 M17-USA C."* |
| No answer within 30 s | the channel | *"M17 M17-USA C did not answer."* |
| The link ends by itself | the channel | *"The link to M17 M17-USA C has ended: reflector refused the link."* The reason can also be `reflector sent DISC`, `no reply for 30 s`, `could not resolve <host>: <error>`, `no Codec 2 decoder`, `conference gone` or `monitor stopped`. |
| `/hangup` | the channel | *"@you hung up M17 M17-USA C."* |

The callsign is in the public dial line on purpose. It is the only record in
the channel of which identity the node presented, and `/register` already
pairs your name with it. `/status` shows it for as long as the link is up,
along with the last station heard.

Because the reflector sees *your* callsign, the node polices it. A changed
callsign is announced; a change is refused while your own M17 link is up; one
callsign cannot be registered to two people; and `/revoke` or `/ban` hangs up
an M17 link the person dialled. `/bot state:off` still leaves live links
alone.

Limits worth knowing:

* **There is no jitter or reorder buffer yet.** Packets are played in the
  order they arrive, so over a poor Internet path audio sounds choppy rather
  than late.
* **One talker at a time.** While one stream is being heard, a second stream
  that overlaps it is dropped. A stream that arrives faster than real time is
  held to real time. Both are counted as `dropped_packets` in the node's
  `GET /status`.
* **There is no automatic reconnect.** A link that fails is torn down and
  reported once, with the reason; dial again to bring it back. A reflector
  that refused or dropped the node is not sent your callsign again unless
  someone asks.
* **A restart of the node ends the monitor.** It is not remembered across
  restarts.
* **D-Star is not wired in yet.** `/dial dstar` still refuses.

## Getting approved

1. Someone runs `/register callsign:<call>`. One line appears in the channel.
   The callsign is mandatory — 3 to 8 letters and digits, with at least one of
   each — and it is the identity the node will connect to the reflector as
   when that person dials M17 (and D-Star, once that is wired in). It shows
   beside their name on the admin page.
2. An admin runs `/approve @them` — or an approver clicks **Approve** on the
   [admin page](#the-admin-page). Both produce the same audit line.

Approval is permission to *take* the mic, not the mic itself: nobody is
unmuted by being approved. Taking it stays a deliberate act.

## The mic

**Everyone who joins the bridged channel is server-muted**, approved or not,
and told once in the channel how to get un-muted. Exactly one person may hold
the mic at a time.

There are two ways to take it:

* **`/mic`**, and `/mic state:off` to give it back.
* **The PTT panel** the bot posts in the voice channel's text chat: a
  **🎙 PTT** button and a **Status** button. The button is a
  **toggle**, not push-to-talk — Discord buttons report a click, never a press
  and a release, so there is no hold to observe.

A toggle can be forgotten, so the mic has two clocks:

| | Default | What it does |
|---|---|---|
| `mic_timeout_secs` | 180 | A repeater-style hard time-out. The mic is released this long after it was taken, whatever is happening. `0` turns it off. |
| `mic_silence_secs` | 5 | **Stop talking for five seconds and it unkeys.** `0` turns it off. |

The silence unkey is what makes a toggle feel like PTT. It is inferred from
the **timing of voice packets** — the only signal there is, because Discord has
no "stopped speaking" event. The voice driver runs in a mode that **never
decrypts and never decodes** what it receives: what the node learns is that
some stream sent a packet in a 20 ms tick, and no Discord audio exists inside
the process at all.

It is also not load-bearing. The hard time-out is a separate clock that reads
none of that, so a holder whose packets stop being observed is still released
on time.

!!! warning "What a server mute can and cannot do"

    A Discord server mute is **guild-wide and sticky**, and this policy applies
    it to everyone who ever joins the bridged channel.

    * Someone who is muted and then leaves voice entirely **stays**
      server-muted — Discord will not let a bot change the voice state of a
      member who is not in voice. The node lifts it the next time they appear
      in any voice channel while it is up. A moderator can also right-click
      them → **Server Unmute**.
    * **Nothing is enforced while the node is down.** Whoever held the mic
      keeps their unmute, and nobody who joins is muted.
    * There is a brief window between a join and the mute landing.
    * If a moderator mutes the holder by hand, the node yields rather than
      fighting them.

    These are exactly why the bridge is listen-only, and why any future armed
    bridge must gate audio **at the node**, not at the mute.

## `/hangup` hangs up everything

`/hangup` — and the **Hang up** button on the admin page — drops the node's
**entire link roster**, not only the link Discord dialled. A link an operator
made with `POST /link`, or someone made over the air with DTMF, goes down too,
and so does the node's M17 monitor once the reflector has accepted it, whoever
started it (one still linking is dropped only if Discord dialled it and
nothing else is linked).

That is the current behaviour and it is worth knowing before you approve
anyone: on a node that does more than feed a Discord channel, `/hangup` is a
bigger hammer than it looks. Narrowing it to "only the link Discord dialled" is
an open backlog item.

## The admin page

```
http://127.0.0.1:8730/discord/admin
```

Approve, revoke, ban, unban, add and remove admins, take the mic, hang up, and
flip the master switch — with the same rules and the same audit lines the slash
commands produce. It also shows who is in the channel, who holds the mic, what
the node is linked to, and the last thing that went wrong.

!!! danger "It lives on `[control] bind`. Never bind that publicly."

    This is the same port that can key a transmitter. To reach the page from
    another machine, tunnel it:

    ```bash
    ssh -N -L 8730:127.0.0.1:8730 <user>@<node-host>
    ```

    then open `http://localhost:8730/discord/admin`. `Host` is then
    `localhost`, which is the point: **every route on this port** carries a
    Host allow-list, so a node bound to `0.0.0.0` and reached at its LAN
    address answers **403** everywhere. That is by design — the guard fails
    closed rather than trusting whatever name a request arrived under, and
    the tunnel needs no exception.

## The state file

`state_file` is where admins, approved users, bans, pending requests and the
list of people this node server-muted are written. Nothing in it is secret:
user ids and the display names last seen.

* In a container, **mount a volume for it** — otherwise every restart loses
  every approval.
* The **parent directory must exist**. A missing one is a typo, and the node
  refuses to start rather than boot looking healthy and fail at the first
  approval.
* A file that exists but **does not parse is a startup failure, by design**.
  Reading it as "nobody is approved" would silently also mean "nobody is muted
  by us", with people still server-muted guild-wide and nothing left that
  remembers why.
* **Absent** means in memory only, and it is the *mutes* that make that
  expensive rather than the approvals. The node says so once at start-up, as a
  warning: everyone in the voice channel is server-muted while the node runs,
  and a server mute is guild-wide. With nothing on disk, the record of **who
  this node muted** dies with the process — so after a restart those people
  arrive in the sweep already muted, with nothing saying the node did it. The
  node reads that, correctly on the evidence it has, as a moderator's mute and
  leaves it alone. **A moderator then has to unmute them by hand.**

If a write fails while the node is running, the action still stands for this
process and the reply says so in as many words — *"couldn't write the state
file — this won't survive a restart"*.
