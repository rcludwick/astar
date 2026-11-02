---
icon: lucide/shield-alert
---

# On-air safety

astar keys real transmitters. This page is the short version of everything that
means, and it is not optional reading.

## You are the licensed operator

Operating any of this requires an **appropriate amateur radio licence for your
jurisdiction**, and the control operator is responsible for every transmission
their station makes — including the ones a computer initiated on their behalf.

Station identification, band and mode privileges, third-party traffic rules,
and unattended-operation rules are all yours to satisfy. astar does not know
your callsign rules, your licence class, or your local regulator.

## Never transmit autonomously

!!! danger "The rule"

    **Connecting to a live node or reflector and keying a transmitter are
    deliberate human actions.** No script, no scheduled job, no automated test,
    and no agent gets to do it.

Concretely, in this project:

* Automated tests must not key a transmitter and must not reach anything
  outside `127.0.0.1`. Self-hosted loopback targets — a local parrot, a local
  reflector — are the only legitimate test destinations.
* The hardware-touching test suites **skip by default**. They run only when
  `IAX_THUMBDV_TESTS=1` is set explicitly, and that variable exists so that
  a machine with a dongle attached opts in, not so that CI opts everyone in.
* Remote keying through astar-server's [`POST /key`](../server/control-api.md#ptt)
  is an operator-supervised action on a loopback-bound control port. It is not
  an automation hook.

## Two guards that must not be removed

These are not defensive coding. They are the difference between a bug and an
unattended transmission.

### Only trusted vocoder sticks are opened

The engine opens a vocoder port only when it is an FTDI `0x0403:0x6015` device
**and** its USB serial number is on the trust list — added by the operator in
[Settings → Vocoders](../macos/vocoders.md), or in `node.toml`'s `[vocoder]`
table for the daemon. An empty list opens nothing and scans for nothing.

The reason is physical. Many radio CAT/PTT cables use the same FTDI chip as a
vocoder stick and look identical on USB, and opening a USB radio interface's
tty **asserts RTS**, which is the radio-key line. A scan that opened every
matching port would key whatever radio was on the other end of such a cable.

Adding a stick is the one operation that opens a port before it is trusted. It
is always a user action, after the user confirms the device is not a radio
cable, and it holds DTR and RTS low before writing anything.

`IAX_THUMBDV_PORT` narrows the choice among the trusted ports. It can
**never** add one: a pin naming a port whose serial is not trusted is refused,
and the open fails. That is the correct behaviour.

### The daemon refuses to key during a D-Star session

`astar-server` will not honour a remote key command while a D-Star session is
active:

```
refusing to key: a D-Star session is active and
D-Star transmit is not remotely keyable
```

D-Star is the one network the daemon must never key remotely, and the check
lives at the caller so it cannot be bypassed by reaching the station directly.
IAX2 and the console's own M17 client session are remotely keyable; the node's
M17 *monitor* has no transmit path, and `POST /key` cannot reach it.

## Listening is always safe

If you only want to hear what is happening, astar has a **TX disabled —
listening only** mode, and links can be established as monitor-only
(`*2` / `"action": "monitor"`), which receives without ever keying.
astar-server can also monitor an M17 reflector
([`POST /m17`](../server/control-api.md) or `/dial m17`): it sends the
reflector only the packets that hold a link open, never audio.

When in doubt, monitor.

## Secrets

Node secrets and portal passwords are **connect/init arguments only**. They are
never stored on a station, never present in snapshots, events or errors, and
never written to a log. Keep it that way on your side too: no passwords in
config files you commit, in bug reports, or in screenshots.
