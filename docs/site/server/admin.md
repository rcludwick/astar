---
icon: lucide/key-round
---

# Signing in

astar-server's admin pages (the node page, Settings, Discord) can be used
from anywhere, not only through an SSH tunnel: people sign in with a
**passkey**, a **security key** or an **authenticator app**. There are no
passwords.

Sign-in is part of the single release executable (`astar-server --features`
lists `auth`). A build from source needs `--features auth`; without it the
public page's **Admin** link explains how to tunnel instead, and the
`astar-server admin` command says the build has no sign-in.

## Where you sign in

| Where | Who needs to sign in |
|---|---|
| The public page, `https://<your hostname>/admin/` | Everyone. Sign-in needs HTTPS there: until the certificate is issued the page says so. |
| The control port on the node itself (`http://127.0.0.1:8730`) | Nobody, by default. A request from the node's own loopback address is the built-in **console** admin, which is how `curl`, scripts and the `astar-server admin` command work, and how you recover if every credential is lost. Set `[control] loopback_login = true` on a shared machine to require sign-in there too. Signing in on the control port works only from the node itself or through an SSH tunnel: the connection's own address must be loopback, whatever `Host` it sends. |

Opening `/admin` on the public page takes you to sign-in, and from there back
to the page you wanted. A link from an email or chat lands on sign-in first
and then goes straight through if you are already signed in.

The same admin pages are served at `/admin/…` on both: `/admin/` (the node),
`/admin/settings`, `/admin/discord/admin`, plus `/admin/account` and
`/admin/users`.

## The first admin

Nobody can sign in until someone has an invite, and the first one comes from
the node's console, never from the web. On the node:

```bash
astar-server admin invite --role admin --name W1ABC
```

In the container:

```bash
podman exec iaxnode astar-server admin invite --role admin --name W1ABC
```

It prints a link once, valid for an hour:

```
https://n12345.example.org/admin/invite#Zq3…
```

Open it on the phone or computer you will sign in with, keep or change the
name, and choose a passkey or a security key (an admin can't sign in with an
authenticator app; see [Roles](#roles)). You are signed in as an admin as soon
as it is set up. The link is spent.

If the node has no public page yet, the link is
`http://localhost:8730/admin/invite#…` and the command prints the `ssh -L`
line that makes it work from your own machine.

On the node's own page (`http://127.0.0.1:8730/` through a tunnel), a node
with no admin shows **No admin yet** with a **Create an admin invite**
button that does the same thing.

The `admin` command finds `node.toml` at `/etc/iaxnode/node.toml` or
`/etc/astar/node.toml`; pass `--config <path>` otherwise. It works whether
the node is running or not.

| Command | Does |
|---|---|
| `astar-server admin invite --role admin\|viewer [--name N] [--hours H]` | A new user's invite. Admin invites last 1 hour, listener invites 24 hours, unless `--hours` says otherwise (5 minutes to 7 days). |
| `astar-server admin invite --user N [--hours H]` | A re-enrol link: adds a sign-in method to the existing user `N`. The way back in for someone who lost every device. |
| `astar-server admin users` | Users, roles, and how many of each sign-in method they have. |
| `astar-server admin revoke-sessions --user N` | Signs `N` out everywhere. |

## Ways to sign in

| Method | What it is | Notes |
|---|---|---|
| **Passkey** | Face ID, Touch ID, Windows Hello, Android, or a password manager (iCloud Keychain, Google Password Manager, 1Password, Bitwarden). | The quickest and the safest: a passkey only works on the site that made it, so it cannot be phished. Synced passkeys follow you to your other devices. Use **Sign in with a passkey**, or pick it from the name field's autofill. |
| **Security key with a PIN** | A YubiKey 5, SoloKey, Feitian or similar. | Choose **Use a security key or code**, type your name, and tap the key. |
| **Security key without a PIN** | An older U2F-only key. Tick **My key has no PIN** when you add it and when you sign in. | Proves you hold the key, nothing more: the same weight as an authenticator app. |
| **Authenticator app** | Google Authenticator, Authy, 1Password, Aegis, any TOTP app. Six-digit codes. | **Signs in listeners only**; for an admin it only confirms it's you (step-up) once you're signed in with a passkey or key. Works on every hostname, so it is a listener's bridge when the node's hostname changes. Codes can be phished, and five wrong codes from one address lock codes from that address out for 15 minutes (30, then an hour… up to a day); signing in from there with anything else lifts it. Wrong codes from addresses you have never signed in from hold all such addresses back for 15 minutes at most, never longer, and never the places you have signed in from — so nobody else can lock you out of your codes. The next sign-in says how many codes failed. Passkeys and keys are never locked out. |

Everyone can have several of each. Add, rename and remove them on
**Account** (`/admin/account`). You can't remove your last one, and an admin
can't remove their last passkey or security key.

## Roles

| Role | Can |
|---|---|
| **Admin** | Everything: the node page, keying, linking, Settings, Discord, users and invites. |
| **Listener** (`viewer`) | Sign in to listen when the public page is private (`anonymous_status = false`). Nothing else. |

The last admin can't be made a listener or deleted.

**Admins sign in with a passkey or a security key, never with an
authenticator-app code.** Codes are only slowed down, not stopped: someone with
enough addresses can try about 100 a day against one name, and over months
that adds up to a real chance of getting in. That's an acceptable risk for a
listener, but not for an account that can key the transmitter and change the
node. So:

- An admin invite, or a re-enrol link for an admin, offers only a passkey or a
  security key.
- **Make admin** needs the user to have a passkey or key already; send them a
  re-enrol link first.
- An admin's last passkey or key can't be removed.
- A code typed at sign-in for an admin is refused exactly like a wrong one,
  so the answer doesn't reveal who is an admin. Once signed in, an admin can
  still use their app to confirm it's them.

An admin set up before this rule with only an authenticator app keeps signing
in with it until they add a passkey or key. Nobody gets locked out, and the
server logs a warning naming them at start-up. **Account** asks them to add one,
and once they have, codes stop signing them in.

## Inviting people

**Users & invites** (`/admin/users`, admins) → **New invite**: choose admin
or listener, optionally a name, and how long the link lasts (1 hour, 24 hours
or 7 days). The link and its QR code are shown **once**: copy it then. Send
it however you like; it works once, until it expires, and **Revoke** ends it
early. Used invites show who used them and when, for a week.

An invite is only as good as the admin who made it. Deleting them, making
them a listener, or **Sign out everywhere** for them (here or with
`astar-server admin revoke-sessions`) withdraws every pending invite they
made, so a link handed out by an account you no longer trust cannot mint
an admin later. A listener keeps any link they made to re-enrol themselves,
except after **Sign out everywhere**, which withdraws those too.

The same page lists users with their sign-in methods and last sign-in, with
**Make admin/listener**, **Sign out everywhere**, **Re-enrol link**, removing
one of their methods (never their last), and **Delete**. **Activity** is the
last 500 sign-in events: sign-ins and failures, lockouts, invites, added and
removed methods, role changes and every key-up from the web, with the
user's name. Failed sign-ins are rolled up: one line per kind, user and
address for each hour, with how many times (×N) and since when, so a flood
of guesses cannot push the rest out of the 500.

Limits on sign-in attempts count an IPv6 client by its /64, not by its
full address, so taking a new address for every try does not reset them.

## Sessions and "Confirm it's you"

Signing in lasts 7 days from last use, and 30 days at most
(`session_idle_days`, `session_max_days`). **Account** lists every signed-in
browser with **Sign out** and **Sign out everywhere else**.

A page left open does not outlive its session: the admin event feed, and on
a private page the listener's event feed and audio, check every 5 seconds
that the session still stands, and stop once it is signed out, expired, its
user removed, or (for the admin feed) its user is no longer an admin. An
open feed does not count as use, so it does not keep an idle session alive.

Some actions need a sign-in method used in the last 10 minutes
(`step_up_minutes`): setting secrets, saving settings, restarting or shutting
down the node, and managing users, invites and your own sign-in methods. If
it has been longer, a **Confirm it's you** sheet asks for any of your
methods and then carries on. Keying and linking never ask: they are what an
operator does all day, and every key is bounded by the transmit time-out and
recorded in Activity.

## Losing a device

- Another sign-in method still works: sign in with it, then remove the lost
  one on **Account**. Removing a method also signs out every browser that
  signed in with it (other than the one you are removing it from), so a
  lost phone's session ends with its passkey. The same goes when an admin
  removes it for you. Browsers signed in before this release cannot say
  which method they used, so removing any method signs those out too.
- None left: an admin sends you a **Re-enrol link** from Users & invites.
- You are the only admin and have nothing left: on the node,
  `astar-server admin invite --user <your name>`.

Removing a passkey removes the node's copy. Your browser or password manager
may still offer it; **Account → Manage passkeys in your browser** says where
each keeps them.

## Moving to your own domain

A passkey or security key works only on the hostname it was made on (its
*relying-party ID*). An authenticator app works on any.

1. Add the new name to `[web] public_hostnames`, keeping the old one. Both
   are served, and the node gets the new certificate.
2. Everyone, signed in on the old name, uses **Account → Set up another
   device or hostname**: it makes a 15-minute link that adds a sign-in method
   to their own account. They open it on the new name and add a passkey
   there. (Or a listener signs in on the new name with their authenticator
   app and adds a passkey. Admins can't, so they use the link.)
3. When everyone has, remove the old name.

**Account** says when none of your passkeys works on the hostname you are
using. Saving hostnames on the Settings page names the people whose passkeys
would stop working and asks you to confirm; if no admin would be able to
sign in at all afterwards, it refuses and tells you the console command.

`[web] rp_id` avoids the move entirely when you own a domain: with
`rp_id = "example.org"`, passkeys made on `node.example.org` also work on
`n2.example.org`. It must be a parent of every public hostname, and never a
shared one such as `sslip.io`. Changing it is itself a move.

## The EchoLink card

On a node with an `[echolink]` section, the node page (`/admin/`) has an
**EchoLink** card. On a node without one the card is not there at all. It is
the one place to call EchoLink stations into the conference by hand; the
[EchoLink](echolink.md) page covers the section itself.

From the top down, the card shows:

* **The login line.** *Logged in as KX7ABC-L* once the directory has
  accepted the node. Before that it reads *Logging in as KX7ABC-L…*,
  *Not logged in (KX7ABC-L)* or *Login failed for KX7ABC-L*, followed by the
  reason when there is one, such as *no password: set the echolink:KX7ABC-L
  secret*.
* **One row per connected station**: its callsign, its node number, its
  state (*connecting* until it answers, then *connected*), a **talking** pill
  while it is sending audio, a **stopped at 10 min** pill (hover for the
  sentence) when the node has stopped sending a net that never paused, how long it has been up, and a **Disconnect**
  button that hangs up on that station alone. With nobody connected the
  table says *no stations*.
* **The Dial box.** Type a node number (`9999`) or a callsign
  (`*ECHOTEST*`) and click **Dial**. The station has to be in the directory's
  list. If the node refuses, the reason appears under the box in one
  sentence, for example *Not in the station list yet — refreshing…*

Callsigns, descriptions and error text come from other stations and from
EchoLink's directory, so the page shows them as plain text and never as
markup. The card uses [`POST /echolink`](control-api.md#post-echolink), which
is an admin route, so only the console and signed-in admins can dial or
disconnect.

## Settings

```toml
[control]
bind = "127.0.0.1:8730"
loopback_login = false   # true: loopback needs sign-in too (shared hosts)

[web]
anonymous_status = true  # false: the listener page and its streams need a session
admin_on_public = true   # serve sign-in and /admin on the public page
rp_id = ""               # "" = each hostname on its own; or a parent domain you own
session_idle_days = 7    # 1–30
session_max_days = 30    # session_idle_days–90
step_up_minutes = 10     # 2–60
```

| Key | Default | Meaning |
|---|---|---|
| `[control] loopback_login` | `false` | `false`: a loopback peer on the control port is the console admin, with no sign-in. A request carrying `Forwarded` or `X-Forwarded-For` never is, so a reverse proxy on the node cannot make the world look local. `true`: everyone signs in, and recovery is the `astar-server admin` command only. |
| `[web] anonymous_status` | `true` | `false` makes the listener page, its status feed and both streams need a signed-in listener or admin. On a build or page without sign-in they are then refused rather than served openly. |
| `[web] admin_on_public` | `true` | Serve sign-in and the admin pages under `/admin/` on the public page. `false` keeps the public page read-only and its Admin link explains the tunnel. Needs `public_bind`. |
| `[web] rp_id` | `""` | See [Moving to your own domain](#moving-to-your-own-domain). |
| `[web] session_idle_days` | `7` | A browser unused this long signs in again. 1–30. |
| `[web] session_max_days` | `30` | Every browser signs in again after this long. From `session_idle_days` to 90. |
| `[web] step_up_minutes` | `10` | How recent "Confirm it's you" needs a sign-in to be. 2–60. |

All of them take effect at the next restart.

## Where it is kept

`auth.json` in the state directory (`state/` beside `node.toml`, or
systemd's `StateDirectory`), mode 0600 in a 0700 directory. It holds users,
their passkeys' public keys, authenticator-app secrets, sessions (only a hash
of each cookie), invites (only a hash of each link's token) and the activity
log. If it is readable by anyone else, or unreadable, the node still runs
but sign-in is off until it is fixed, and the node's own page says why:

```bash
chmod 600 state/auth.json
```

Nothing in it ever appears in a log, the status feed or an error message.
