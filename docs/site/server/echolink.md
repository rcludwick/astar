---
icon: lucide/globe
---

# EchoLink

EchoLink is a network that lets licensed amateurs talk to each other over the
internet. Every station on it, whether a person at a computer, a radio link
or a conference server, has a callsign and a **node number**, and the
stations signed in at any moment are listed in EchoLink's **directory**.

astar-server can sign in to that directory as a node and call EchoLink
stations into its conference. A station you dial this way becomes one more
member of the conference, alongside the AllStar links and the handsets:

* it **hears the conference**, minus its own audio, the same mix every other
  member hears;
* the conference **hears it**: what the station says is mixed in for
  everyone else.

Several EchoLink stations can be connected at once. Calls only go one way:
the node calls stations, and a station that calls the node is ignored.

The EchoLink leg is built into every astar-server, but it does nothing until
`node.toml` has an `[echolink]` section. A node without one never contacts
EchoLink at all.

## Before you start: a `-L` or `-R` callsign

EchoLink gives nodes their own callsigns. A node's callsign is yours with
**`-L`** on the end for a simplex link (a radio on one frequency), or
**`-R`** for a repeater. EchoLink treats each one as a separate callsign that
has to be registered and validated on its own, even if your plain callsign is
already validated. Register it by signing in once with EchoLink's own
software as the `-L` or `-R` callsign, then validate it on EchoLink's
[validation page](https://www.echolink.org/validation/). The
[app's EchoLink page](../macos/echolink.md#before-you-start-register-and-validate-your-callsign)
walks through both steps. astar never registers or validates anything.

EchoLink sends the password across the internet unencrypted, so give the
node's callsign a password you use nowhere else.

## The `[echolink]` section

```toml
[echolink]
callsign = "KX7ABC-L"
servers  = ["servers.echolink.org", "naeast.echolink.org",
            "nasouth.echolink.org", "backup.echolink.org"]
location = "Anytown, ST"
```

| Key | Default | Meaning |
|---|---|---|
| `callsign` | required | The node's EchoLink callsign. It must end in `-L` or `-R`, and it is uppercased when the file loads. Without a suffix the node refuses to start: `echolink.callsign: "KX7ABC" has no -L or -R suffix; EchoLink requires one for a node`. |
| `servers` | the four above | EchoLink's directory servers, tried in turn. If one does not answer, the node moves to the next, waiting a little longer between tries, up to ten minutes. An empty list is refused: `echolink.servers: at least one server is needed`. |
| `location` | `""` | A short line other stations see beside your callsign in the directory, such as your town. |

Any other key in the section is a start-up error, so a typo cannot pass
silently. `deploy/node.toml.example` carries the same section, commented out.

## The password

The password is a **secret**, so it never goes in `node.toml` and never
appears on the settings page. The node keeps it under the name
`echolink:<CALL>`, where `<CALL>` is the callsign from the section, for
example `echolink:KX7ABC-L`. There are three ways to give it to the node:

* **The environment.** Set `ASTAR_ECHOLINK_PASSWORD` before the node starts.
  It works under any `[secrets] source`.
* **The credentials file.** Add a line to `credentials.toml`:

    ```toml
    "echolink:KX7ABC-L" = "…"
    ```

* **The control port**, while the node runs:

    ```bash
    curl -X POST http://127.0.0.1:8730/secrets \
         -d '{"username":"echolink:KX7ABC-L","secret":"<password>"}'
    ```

The node logs in to the directory once, at start-up, when it has both the
section and the password. With no password it logs nothing in, and the
EchoLink card on the node page says *no password: set the echolink:KX7ABC-L
secret*. Setting the secret through `POST /secrets` then starts the login
straight away, with no restart. Changing the password of a node that is
already logged in takes effect at the next restart.

Logging in only puts the node on EchoLink's list. It never calls a station
by itself.

## Calling a station

An admin dials a station from the **EchoLink card** on the node page
(`/admin/`), by node number or callsign. The card shows the login state, the
connected stations, and a **Disconnect** button for each one; see
[Signing in](admin.md#the-echolink-card) for what each part shows. Scripts
can do the same with [`POST /echolink`](control-api.md#post-echolink).

A dial can only name a station the directory lists. Nothing in a request can
name an address: the station's address comes from the directory the node
logged in to.

While at least one EchoLink station is connected:

* **`POST /key` is refused**, with *refusing to key: an EchoLink session is
  active and EchoLink transmit is not remotely keyable*. The node's own radio
  cannot be keyed from the control port while EchoLink stations are on.
* **A restart from the settings page is refused**, the way it is for live
  AllStar links, and the refusal counts the EchoLink stations among the
  reasons ("1 EchoLink station").

**A net that never pauses is cut off at 10 minutes.** Each station on the
node hears the rest of the net through the node, and the node stops sending
to a station after 10 minutes of continuous audio, the way a stuck key is
let go on every other network. It sends again as soon as the net pauses. The
station's row on the EchoLink card shows a **stopped at 10 min** pill, the
log says *Stopped sending the net to … after 10 minutes of continuous talk*,
and no setting turns it off.

If the station you dial is not in the station list the node has, which can be
up to ten minutes old, the dial is refused with *Not in the station list yet —
refreshing…* and the node asks the directory for a fresh list, so a station
that has just logged in can be dialled a moment later. The directory is asked
at most once every 30 seconds.

A password the directory's login line cannot carry (one with a line break, or
a non-ASCII character such as `¬`) is refused before anything is sent, and
the card says so; set the secret again with a plain one.

The bridge has to be a conference for a station to join it. The default
`[bridge] mode = "bridge"` is one; handset and parrot modes are not, and a
dial in either is refused.

When the node shuts down, it hangs up every EchoLink station and then logs
out of the directory, last of all and for at most ten seconds, so a slow
directory never holds up the AllStar hang-ups.

## Ports

EchoLink uses three fixed ports:

| Port | Direction | What for |
|---|---|---|
| TCP 5200 | out | The directory: logging in, the station list, logging out. |
| UDP 5198 | out, and the replies | Voice. |
| UDP 5199 | out, and the replies | Call control: who is on the call, keepalives, chat. |

Because the node only calls out, nothing has to be forwarded in. A firewall
that allows those connections out and lets their replies back in is enough.

**One EchoLink login per public IP address.** EchoLink allows one signed-in
station for each public address, so nothing else behind the node's address
may log in to EchoLink while the node does: not the EchoLink app on a phone
on the same network, and not a second node.

## Why EchoLink can't run beside Discord or the public listener

EchoLink's [access policy](https://secure.echolink.org/access_policies.htm)
forbids connecting it to other voice-over-internet systems that people can
reach directly from a computer. The [Discord bridge](discord.md), the
[public listener page](listening.md) and `/stream.mp3` are exactly that kind
of system: anyone with a browser or a Discord account can hear them, licensed
or not.

So the node refuses to load `[echolink]` beside any of them:

* beside `[discord]`: *echolink: can't run with [discord] — EchoLink's access
  policy forbids linking it to VoIP systems people reach directly from a
  computer; remove one of the two sections*
* beside `[web] public_bind` or `[stream] enabled = true`: *echolink: can't
  run with the public listener ([web] public_bind or [stream] enabled) —
  EchoLink's access policy forbids linking it to VoIP systems people reach
  directly from a computer; remove one of them*

The same check runs on every save from the settings page, so turning one of
them on while the other is configured is refused there too, with the same
sentence.

## What it does not do yet

* **No incoming calls.** A station that calls the node is ignored, and audio
  from any station the node did not call is dropped without a reply.
* **No EchoLink Proxy.** A node behind carrier-grade NAT, where many
  customers share one public address, may not get EchoLink audio through.
* **No restart memory.** Connected stations are not redialled after a
  restart; the login itself comes back if the password is set.
