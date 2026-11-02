---
icon: lucide/globe
---

# The public page

`astar-server` can serve a **public, read-only page** about the node: what
it is linked to, who is talking, today's activity, with a live spectrum and
level graph — and, if you turn it on, the **net's audio**, live. It runs on
its own listener, separate from the [control port](control-api.md), and it
can do nothing but show and play things. There is no button on it that
changes the node.

It is off until `[web] public_bind` is set in `node.toml`.

!!! info "Two listeners, two routers"

    The control port (`[control] bind`, loopback by default) keeps every
    route, including `POST /key`. The public bind runs a **separate** router
    with a closed list of read-only `GET` routes. Every other path is `404`
    there, whatever the method, so no admin route can be reached from it,
    not even by mistake.

## Turning it on

The shortest setup uses [sslip.io](https://sslip.io), a public DNS service
that answers `108-175-15-153.sslip.io` with `108.175.15.153`. It needs no
DNS account at all:

```toml
[web]
public_bind = "0.0.0.0:443"
public_hostnames = ["108-175-15-153.sslip.io"]   # your IP, dots as dashes
```

Open TCP ports **80 and 443** to the box. Port 80 answers Let's Encrypt's
HTTP-01 check and redirects everything else to HTTPS. Then restart the
node: it asks Let's Encrypt for a certificate, and the page is at
`https://108-175-15-153.sslip.io/`.

With your own domain at Porkbun the node can do the DNS too. See
[Porkbun](#porkbun) below.

## Live audio

With `[stream] enabled = true`, the page gets a play button and the node
serves two streams of the same audio:

| Stream | What it is | Delay |
|---|---|---|
| The page's player | Opus over a WebSocket (`/stream.ws`), decoded in the browser | about 0.4 s |
| `/stream.mp3` | a plain MP3 stream: open it in VLC, a phone's audio app or a scanner app | about 6 s |

```toml
[stream]
enabled = true
```

**What is in it:** the audio the node receives from its links, mixed: every
linked node and an M17 reflector it is monitoring. **What is never in it:**
the node's own announcements, IDs and greetings, which are played to
members privately, and anything in parrot mode. A parrot caller is making a
private test, so there is no stream in parrot mode whatever `enabled`
says, and none in handset mode either; the page says "No live audio in this
mode", and the stream comes back on its own when the node returns to
bridge or conference mode.

Silence keeps flowing, so players do not drop the stream between overs.
`max_listeners` caps both streams together; past it a listener is told
"Full, try again shortly". A listener whose connection stalls is dropped
after 30 seconds, and its place is freed.

**Browsers.** The low-delay player needs HTTPS and WebCodecs (current
Chrome, Edge, Firefox and Safari). Without WebCodecs the page falls back to
`/stream.mp3` by itself; if the server was built without MP3 it says "Live
audio needs a newer browser" rather than failing silently. Browsers never
autoplay: playing is always a click. Volume and mute are remembered per
browser.

`/stream.mp3` needs a server built with `stream-mp3` (the static release has
it); the Opus player is in every build.

## Hostnames and `{node}`

`public_hostnames` is the list of names the page answers to, and the names
its certificate covers. A request for any other name, a bare IP address
included, gets `421 Misdirected request`. That stops DNS rebinding. A
request with no `Host` at all gets `400 Bad Request`.

Any entry may contain `{node}`, which is replaced with this node's number,
the `[register] node_id`. With a `[web.dns]` section, `public_hostnames`
can be left out altogether. It then defaults to `{node}.<domain>`:

| `node_id` | `[web.dns] domain` | `host` | The page answers to |
|---|---|---|---|
| `69586` | `example.net` | *(default `{node}`)* | `69586.example.net` |
| `69586` | `example.net` | `"radio{node}"` | `radio69586.example.net` |
| `69586` | `example.net` | `""` | `example.net` |

Names are compared in lower case, with a trailing dot and a port ignored.
A template that uses `{node}` on a node with no `[register]` is refused at
start-up with a message naming `[register]`. An IP address or a `*.`
wildcard is refused too, because no certificate can be issued for it here.

## HTTPS

Certificates come from **Let's Encrypt**, through an ACME client built into
the server. There is no Caddy, nginx or certbot to run.

- The certificate and the ACME account key are kept in the state directory,
  under `acme/`, with mode `0600`.
- After a restart the stored certificate is served at once.
- It renews 30 days before it expires. If a renewal fails, the old
  certificate keeps serving and the node retries, backing off from a minute
  to an hour.
- A certificate Let's Encrypt has issued is never thrown away. If the state
  directory will not take it, it is served anyway and only the save is
  retried. If it will not load, the node waits six hours before ordering
  another.
- At most **three certificates a week** are ordered for the same hostnames,
  which keeps well inside Let's Encrypt's limit of five. The count is kept
  in `acme/issued.json`; when it is reached, the admin status page says when
  the next order is allowed.
- **Until the first certificate arrives**, the page is served over plain
  HTTP, on both ports, and the reason is shown on the admin status page.
  Browsers allow the low-delay audio player only over HTTPS.
- Every response over HTTPS carries `Strict-Transport-Security:
  max-age=31536000`, so a browser that has been here once goes straight to
  HTTPS for a year. It covers the page's own names only, not your other
  subdomains, and it is never sent over plain HTTP.
- `acme_staging = true` uses Let's Encrypt's staging CA while you test. Its
  certificates are not trusted by browsers, but its rate limits are generous.

HTTPS needs a server built with the `public-tls` feature (the release
build has it; `cargo build -p astar-server --features public-tls`). Without
it, the public page is plain HTTP, and the node says so once at start-up.

## DNS providers

Two jobs can be handed to your DNS provider. **Dynamic DNS** keeps the DNS
record for your page pointed at this box, even when its address changes.
**DNS-01** is how the node proves to Let's Encrypt that it owns the name: it
writes a short-lived DNS record instead of answering on port 80, so port 80
is not needed at all.

`[web.dns]` names the provider that holds your zone (the domain you manage
there). `provider` defaults to `"porkbun"`. Every provider is driven with
an API credential that you create in the provider's own control panel:

| `provider` | What to create at the provider | Credential key | Environment variable | Build |
|---|---|---|---|---|
| `"porkbun"` (the default) | API key and secret API key, with the DNS permission | `porkbun_pk`, `porkbun_sk` | `PORKBUN_API_KEY`, `PORKBUN_SECRET_API_KEY` | any build with `public-tls` |
| `"cloudflare"` | Cloudflare API token (Zone → DNS → Edit) | `dns:cloudflare` | `ASTAR_DNS_CLOUDFLARE_TOKEN` | release build only |
| `"digitalocean"` | DigitalOcean personal access token | `dns:digitalocean` | `ASTAR_DNS_DIGITALOCEAN_TOKEN` | release build only |
| `"desec"` | deSEC token | `dns:desec` | `ASTAR_DNS_DESEC_TOKEN` | release build only |
| `"hetzner"` | Hetzner DNS API token | `dns:hetzner` | `ASTAR_DNS_HETZNER_TOKEN` | release build only |
| `"linode"` | Linode personal access token | `dns:linode` | `ASTAR_DNS_LINODE_TOKEN` | release build only |
| `"gandi"` | Gandi personal access token | `dns:gandi` | `ASTAR_DNS_GANDI_TOKEN` | release build only |
| `"vultr"` | Vultr API key | `dns:vultr` | `ASTAR_DNS_VULTR_TOKEN` | release build only |
| `"ionos"` | IONOS API key (prefix.secret) | `dns:ionos` | `ASTAR_DNS_IONOS_TOKEN` | release build only |
| `"bunny"` | bunny.net API key | `dns:bunny` | `ASTAR_DNS_BUNNY_TOKEN` | release build only |
| `"gcore"` | Gcore API token | `dns:gcore` | `ASTAR_DNS_GCORE_TOKEN` | release build only |

Porkbun works in any build that has `public-tls`. The others need the
release build, which carries the `dns-providers` feature; a build without it
refuses to start with a message saying so. Porkbun is walked through first,
then Cloudflare as an example of the rest.

### Porkbun

If your domain is registered at [Porkbun](https://porkbun.com), the node can
keep its DNS record pointed at itself and prove its certificate through DNS-01.

#### 1. Turn on API access for the domain

API keys work only on domains that are opted in. In Porkbun's
**Domain Management**, open the domain's details and switch on **API
Access**. Without it, every call fails with `API_ACCESS_DISABLED`, and the
node's log says to turn this on.

#### 2. Create an API key

At [porkbun.com/account/api](https://porkbun.com/account/api), create a key.
Porkbun shows two values, once:

- the **API key**, starting `pk1_`;
- the **secret API key**, starting `sk1_`.

A key limited to the **DNS** permission and to this one domain is enough.
Porkbun lets you restrict both when you create the key.

#### 3. Give the keys to the node

They are credentials, so they go wherever your `[secrets] source` keeps
them. They never go in `[web]`.

| `[secrets] source` | Where the two keys go |
|---|---|
| `file` | `porkbun_pk` and `porkbun_sk` in `credentials.toml`, or **Public page → Porkbun API key** on the settings page |
| `env` | the environment variables `PORKBUN_API_KEY` and `PORKBUN_SECRET_API_KEY` |
| `config` | `porkbun_pk = "pk1_…"` and `porkbun_sk = "sk1_…"` under `[secrets]` in `node.toml`, kept mode `0600` |
| `control` | `POST /secrets` with usernames `porkbun_pk` and `porkbun_sk` |

The node reads them at start-up. They never appear in a log line, an error
message, the status page or the settings page, which shows only whether
each one is set.

#### 4. Configure `[web.dns]`

```toml
[register]
node_id = "69586"

[web]
public_bind = "0.0.0.0:443"     # public_hostnames defaults to {node}.<domain>

[web.dns]
provider = "porkbun"
domain = "example.net"          # the zone registered at Porkbun
ddns = true                     # keep 69586.example.net pointed here
```

On start-up the node asks Porkbun for this box's public IPv4 address, then
creates the A record for `69586.example.net` or corrects it. It checks
again every `ddns_interval_secs`, and writes only when the address has
changed. The log line says the name and the address, nothing else.

For the certificate it writes a TXT record at
`_acme-challenge.69586.example.net`. It then waits until Porkbun's own
nameservers serve that record, because that is where Let's Encrypt will
look, and only then asks for validation. The TXT record is always deleted
afterwards, whether issuance succeeded or failed. A TXT record of your own
at the same name is left alone.

### Cloudflare

To see the same setup with a different provider, here is a zone at
[Cloudflare](https://www.cloudflare.com). Create an API token with
**Zone → DNS → Edit** permission for the one zone. Then set up `[web.dns]`:

```toml
[web]
public_bind = "0.0.0.0:443"

[web.dns]
provider = "cloudflare"
domain = "example.net"
ddns = true
ip_source = "https://ip.example.org/"   # any https URL that answers with your public IPv4
```

Give the token to the node in any one of three ways. They are credentials,
so none of them goes in `[web]`:

* the environment variable `ASTAR_DNS_CLOUDFLARE_TOKEN`;
* a line `"dns:cloudflare" = "…"` in `credentials.toml`;
* `POST /secrets` on the control port, with the username `dns:cloudflare`.

The other providers work the same way: the credential key and environment
variable in the table above take the place of the Cloudflare ones. There is
no field for these tokens on the settings page or inline in `node.toml`. The
names are never accepted as an inbound IAX2 username, so a caller cannot use
them to probe the store.

When the token is missing, the node logs it and reports it in its status,
naming both the credential key and the environment variable, and does no DNS
work. Each call to the provider gives up after 60 seconds and is reported like
any other provider error. Errors from the provider are logged with the token
removed.

### Where the public address comes from

Dynamic DNS needs this box's public IPv4 address, and `ip_source` says where
to ask:

* Absent, or `"provider"`: Porkbun's own `/ping`. Only Porkbun can tell the
  node its own public address, so every other provider needs a URL.
* An `https://` URL that replies with the address as plain text, for example
  `ip_source = "https://ip.example.org/"`. Plain `http://` is refused, both
  at start-up and when the URL redirects: the answer decides which address
  your DNS record points at, so it must not be forgeable on the wire.

A node that is not using `ddns` does not need `ip_source`. The node refuses
to start on any other value, and on a non-Porkbun provider that has
`ddns = true` or `ip_source = "provider"` without a URL.

### How long DNS-01 waits

After writing the challenge record, the node waits until the record is
visible before it asks Let's Encrypt to check. For Porkbun it asks Porkbun's
nameservers directly. For the other providers it waits until the provider
lists the record and then 60 more seconds, because the node does not yet look
up those providers' nameservers. Issuance with them can therefore take a
little longer.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `[web] public_bind` | *(absent: no page)* | Address and port of the public page, e.g. `0.0.0.0:443`. |
| `[web] public_hostnames` | `["{node}.<domain>"]` with `[web.dns]`, else none | Names the page answers to and the certificate covers. `{node}` is the node number. Required when there is no `[web.dns]`. |
| `[web] acme_email` | *(none)* | Optional address for Let's Encrypt's expiry notices. |
| `[web] acme_staging` | `false` | Use Let's Encrypt's staging CA. |
| `[web] acme_challenge` | `"dns-01"` with `[web.dns]`, else `"http-01"` | How ownership of the names is proved. |
| `[web] acme_http_bind` | `"0.0.0.0:80"` under HTTP-01; none under DNS-01 | The port-80 listener: HTTP-01 answers and the redirect to HTTPS. Set it under DNS-01 to keep the redirect. |
| `[web.dns] provider` | `"porkbun"` | The DNS provider; see [DNS providers](#dns-providers). Anything but `"porkbun"` needs the release build. |
| `[web.dns] domain` | — | The zone registered at the provider. Every hostname must be in it. |
| `[web.dns] host` | `"{node}"` | The subdomain of the default hostname. `""` is the domain itself. |
| `[web.dns] ip_source` | `"provider"` | `"provider"` (Porkbun only) or an `https://` URL that returns this box's public IPv4. Every other provider needs the URL when `ddns` is on. |
| `[web.dns] ddns` | `false` | Keep an A record for each hostname pointed at this box. |
| `[web.dns] ddns_interval_secs` | `300` | How often to check the address. At least 60. |
| `[web.dns] ttl` | `600` | TTL of the records the node writes. Porkbun's minimum is 600, and the node keeps 600 as the floor for every provider. |
| `[stream] enabled` | `false` | Serve the live streams. Never in parrot or handset mode. |
| `[stream] max_listeners` | `25` | Listeners across both streams together, 1–200. |
| `[stream] mp3_kbps` | `32` | `/stream.mp3` bitrate: 8, 16, 24, 32, 40, 48 or 64. |
| `[stream] opus_kbps` | `24` | The page player's Opus bitrate, 6–128. |

## The admin link

The page's **Admin** link opens `/admin`. On a node with sign-in (the
release build) that is the sign-in page, and after it the admin pages under
`/admin/`: see [Signing in](admin.md). Sign-in needs HTTPS, so until the
certificate is issued the page says so (`localhost` is the exception).

On a build without sign-in, or with `[web] admin_on_public = false`, `/admin`
explains how to reach the admin pages through an SSH tunnel instead:

```bash
ssh -L 8730:127.0.0.1:8730 you@your-node
```

Then open <http://localhost:8730>.

## A private page

`[web] anonymous_status = false` makes the page, its status feed and both
streams need a signed-in **listener** or admin: invite listeners from
**Users & invites** with the *Listener* role. Someone who is not signed in
is sent to sign-in and back. The page's scripts and stylesheets stay
public; they say nothing about the node. On a page without sign-in, a
private page is refused (503) rather than served openly.

A stream opened on a private page stops within 5 seconds of its session
ending, whether it was signed out, expired or its user removed.

## What the page never shows

The public status is built field by field from a short allowlist: the node
number, the mode, the linked nodes and reflectors with their direction and
uptime, the current talker, the last 20 link and key events, today's totals
(midnight to midnight UTC; a key under half a second counts there but stays
out of the feed), the listener count, and whether the stream is live. It
never contains an IP address, a port, a credential, the configuration,
Discord details, or registration state.

## Connection limits

The public bind faces the whole internet, so no one client can hold it.
These limits are built in; there are no settings for them.

| Limit | Value | What happens past it |
|---|---|---|
| Time to send a request | 10 s, for the TLS handshake, the request and its body together | The connection is closed. Sending a byte at a time gains nothing |
| Connections open at once | 1024 on the public bind; port 80 has its own 256 | A new connection is closed at once |
| Connections from one address | 16 on each port | A new connection from that address is closed at once |
| Streams from one address | 4: the audio streams and the event feeds together, enough for two tabs | `429 Too many streams open from your address` |
| Event feeds open at once | 256, for every address together | `503 Full, try again shortly` |

One address means one IPv4 address, or one IPv6 `/64`, since an IPv6 host
is normally given a whole `/64`. Port 80 keeps its own pool, so a public
bind full of connections never blocks Let's Encrypt's HTTP-01 check.
With the default `max_listeners` of 25, no one address can take every
audio slot; set `max_listeners` above 4 to keep it that way.

## Ports

| Port | When | What |
|---|---|---|
| `public_bind` (443) | `[web] public_bind` is set | The page and the streams: HTTPS once a certificate exists, plain HTTP before |
| `acme_http_bind` (80) | HTTP-01, or set explicitly | Let's Encrypt's check, and a redirect to HTTPS (the page itself before there is a certificate) |

Binding ports below 1024 needs a privilege. The
[systemd unit the server prints](../build/server.md#deploying-it-one-executable-one-config)
grants exactly `CAP_NET_BIND_SERVICE`; in a rootless container, set
`net.ipv4.ip_unprivileged_port_start=80` on the host instead.
