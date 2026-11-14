---
icon: lucide/messages-square
---

# Net recordings

[`astar-netrec`](../netrec.md) records scheduled nets into a folder. This page
covers how `astar-server` shows that folder: a list of recorded nets, one page
per night laid out as a conversation, one play button, and a delete button for
administrators. The pages themselves are described under
[The recordings page](../netrec.md#the-recordings-page); this page covers what is
specific to the server.

## Turning it on

Point the server at the folder the recorder writes into. In `node.toml`:

```toml
[netrec]
archive = "/var/lib/astar/netrec"
```

`archive` is the same folder as `root` in `netrec.toml`. Restart the node, then
open `/nets/` on its address. Without `[netrec]` the address answers 404. If
the folder does not exist yet, the list is simply empty; the server never
creates it.

The recorder and the server can share one machine, which is the usual setup
on a VPS: the recorder writes, the server reads. They need no network between
them, only the folder. Make sure the server's user can read it.

## Who sees what

The pages are served at `/nets/` on the control port, and on the public page
too when the node has one (`[web] public_bind`). Who counts as signed in
depends on which of the two you open, and the table below is about who is
asking once that is settled.

| Who is asking | Public sessions | Private sessions | Delete |
|---|---|---|---|
| Anyone, signed out | yes | no | no |
| Signed in | yes | yes | no |
| Signed in as an admin | yes | yes | yes |

A session is public or private by the `visibility` of its net in
`netrec.toml`. To a signed-out reader a private session does not exist: the page
says "No such recording", exactly as it does for a session that was never
there, so the address gives nothing away. Where the node offers sign-in, the
signed-out list carries a link to it.

On the **control port**, the [sign-in rules](admin.md#where-you-sign-in) of the
admin pages apply. A request from the node's own loopback address is the
console, which counts as an admin, unless `[control] loopback_login = true`.
A build without sign-in (no `auth` feature) treats every caller on the control
port as an admin: they see private sessions and can delete. That matches the
port's open policy everywhere else, so keep the control port private: bind it
to loopback (`127.0.0.1:8730`) and reach it from elsewhere through an SSH
tunnel.

On the **public page**, sign-in only counts when `[web] admin_on_public` is on
(the default whenever there is a public bind). With it off, the public page has
no sign-in at all: everyone there is a signed-out reader, sees public sessions
only, and the list shows no sign-in link. Private sessions are then reachable
only through the control port.

`[web] anonymous_status` is about the node's status page and does not hide
public nets. A public net's session is open to everyone whether it is on or off.

## Deleting

An admin sees a Delete button on a finished session. It asks first, then removes
the whole session folder. A session that is still recording is refused. So is
one whose recorder crashed mid-session: it was never marked as ended, so it
still reads as recording, and it cannot be deleted here until `astar-netrec`
starts once and its crash recovery closes the session. The
delete goes through the admin routes, so it needs an admin sign-in and the same
same-origin check as every other change.

That is the only thing the server ever writes to the folder. Everything else it
does is read, and the recorder's own retention (size and age) still decides what
else is deleted.

## What a listener hears

Audio is played from each session's `session.flac` with byte ranges, so the
browser fetches only the part it needs. Sessions recorded before the recorder
wrote `my_callsigns` into `session.json` show everyone on the left; the server
has no copy of the recorder's config to fall back on.
