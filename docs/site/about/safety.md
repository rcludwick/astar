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
M17 *monitor* has no transmit path toward the reflector, and `POST /key` cannot
reach it.

## How keying is limited

astar can be keyed several ways at once: the on-screen PTT button and the
Spacebar, VOX, the PTT switch on a USB radio interface, a
[Bluetooth PTT button](../macos/bluetooth-ptt.md), and the key command of
astar-server, the Python binding and the CLI's `dstar-listen`. The engine combines them, and it
enforces the Bluetooth lease, the transmit time-out and the release-to-re-arm
latches itself, on its own thread, so those do not depend on the app's window,
its timers or the Bluetooth stack staying healthy. Faster link-loss detection
for the Bluetooth button runs in the app, with the engine's lease behind it.

### Sources combine

You are on air while any source holds the key. Releasing one never cuts off
another that is still held: VOX going quiet does not unkey a handset whose PTT
switch is down.

### The on-screen button is the universal stop

Pressing the on-screen PTT button, or Space, while another source holds the key
stops transmitting outright. It is the one control a stuck wireless button
cannot hold down. With VOX on, the call card shows the VOX indicator instead of the PTT button,
and the indicator is not a button: Space is the stop then, while the astar
window is in front. **TX disabled — listening only** does the same the moment you
turn it on, and turning it off again keys nothing. astar-server's
[`POST /unkey`](../server/control-api.md#ptt) is a stop too.

### Release to re-arm

After any forced stop, a source that was holding the key cannot key again
until it has been released: the button let go, the PTT switch up, VOX quiet. A
reconnect, a wake from sleep or a new call never keys by itself, and a button
held through the end of one call does not key the next.

A Bluetooth button that was **up** when its link dropped has nothing to
release: once astar reconnects, its next press keys normally. A button that
was down at the drop, or down as the link came back, is latched instead:
let it go, and the press after that keys normally. astar-server's `/key` is
latched only by the transmit time-out, below.

### The Bluetooth lease

A Bluetooth button's hold is a lease of 1.5 seconds that the app renews every
250 ms while the button is down. If the renewals stop — the app hangs, the
Bluetooth stack stalls — the engine unkeys when the lease runs out.

Separately, the app watches the link itself and catches one that dies faster
than the operating system would notice: in about 2.5 seconds. That detection
runs in the app, through the Bluetooth stack. If the app or the stack stalls,
the engine's 1.5-second lease is the backstop, and the transmit time-out backs
up everything.

### The transmit time-out

astar stops transmitting after a set time continuously on air, **whatever is
keying**.

* **One clock for the whole over.** It starts when the radio keys and resets
  only when the radio unkeys. Handing over from Space to the Bluetooth button
  mid-over does not restart it.
* **1 to 10 minutes, default 2, and never off.** Set it in Settings →
  **Transmit** → **Transmit time-out**, in whole minutes. The engine refuses
  anything outside that range, 0 included, from every client and binding, and
  keeps the value it had. A config file carrying 0 loads as 1 minute.
* **It stops everything.** Every source that was holding the key is released
  and must re-arm before anything keys again. The banner reads "Transmit stopped after 2 minutes. Release to talk again." with your setting's minutes.
* **One value for the whole app**, not per config. It travels in config
  exports with the other audio settings.
* **It covers astar-server.** The node has no setting for it and runs on the
  2 minute default, so a [`POST /key`](../server/control-api.md#ptt) held
  longer than that unkeys, and the next `/key` is refused with
  `ptt source is latched: release it before keying again` until an `/unkey`.
  Only the time-out needs that `/unkey`. A `/key` the node refuses (no call
  up yet) leaves nothing latched, and a `/key` still held when a call ends is
  simply released by the hang-up, so the next call's `/key` keys.
  The Windows/Linux client and the CLI's `dstar-listen` also run on the
  2 minute default, with no setting and no banner. `astar-cli dial` and
  `parrot` key the IAX2 call directly, below the station, so neither the
  arbiter nor the transmit time-out applies to them. A Python script runs on
  the default too unless it calls `Station.set_tx_timeout_ms`
  (60 000–600 000 ms; 0 is refused).
* **It limits what astar sends to the network.** The radio line of a USB
  radio interface, which astar keys while the far end is talking (see
  [PTT wiring](../macos/hardware.md#ptt-wiring)), follows that incoming
  traffic and is not timed by it.
* **It is not VOX's Hang Timeout.** Hang Timeout is how long VOX waits after
  you stop talking; the transmit time-out is how long you may stay on air. VOX
  re-arms once it has been quiet for its Hang Timeout.

D-Star keeps its own 5-minute network time-out as well; whichever is shorter
fires first.

## Listening is always safe

If you only want to hear what is happening, astar has a **TX disabled —
listening only** mode, and links can be established as monitor-only
(`*2` / `"action": "monitor"`), which receives without ever keying.
astar-server can also monitor an M17 reflector
([`POST /m17`](../server/control-api.md) or `/dial m17`): it sends the
reflector only the packets that hold a link open, never audio.

!!! warning "A monitored M17 reflector is relayed to your links"

    The M17 monitor is listen-only toward the reflector, but not toward your
    own network. What it hears is mixed into the node's conference and
    relayed to every AllStar node linked to the node and every EchoLink
    station on it, and a linked node with a radio puts it on the air. Link
    only reflectors whose traffic you would rebroadcast, and mind your
    licence's rules on retransmission. See
    [Configuration](../server/configuration.md#m17-receive-only-m17-reflector-monitor).

When in doubt, monitor.

## Secrets

Node secrets and portal passwords are **connect/init arguments only**. They are
never stored on a station, never present in snapshots, events or errors, and
never written to a log. Keep it that way on your side too: no passwords in
config files you commit, in bug reports, or in screenshots.
