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
* Remote keying through [`POST /key`](../server/control-api.md#ptt) is an
  operator-supervised action on a loopback-bound control port. It is not an
  automation hook.

## Two guards that must not be removed

These are not defensive coding. They are the difference between a bug and an
unattended transmission.

### The ThumbDV port pin only narrows the scan

`IAX_THUMBDV_PORT` filters the results of the FTDI `0x0403:0x6015` scan for a
D-Star vocoder dongle. It can select among ports the scan already matched. It
can **never** replace the scan or point the opener at an arbitrary serial port.

The reason is physical: opening a USB radio interface's tty **asserts RTS**, and
RTS is the radio-key line. A pin that could name any port would be a remote
transmit button disguised as a convenience feature.

A port the scan did not match yields no candidates, and the open fails. That is
the correct behaviour.

### The daemon refuses to key during a D-Star session

`astar-server` will not honour a remote key command while a D-Star session is
active:

```
refusing to key: a D-Star session is active and
D-Star transmit is not remotely keyable
```

D-Star is the one network the daemon must never key remotely. IAX2 and M17 are
remotely keyable by design; D-Star is not, and the check lives at the caller so
it cannot be bypassed by reaching the station directly.

## Listening is always safe

If you only want to hear what is happening, astar has a **TX disabled —
listening only** mode, and links can be established as monitor-only
(`*2` / `"action": "monitor"`), which receives without ever keying.

When in doubt, monitor.

## Secrets

Node secrets and portal passwords are **connect/init arguments only**. They are
never stored on a station, never present in snapshots, events or errors, and
never written to a log. Keep it that way on your side too: no passwords in
config files you commit, in bug reports, or in screenshots.
