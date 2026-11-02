---
icon: lucide/star
---

# Favorites and recents

Most of the time you talk to the same few places: a local hub on AllStarLink,
a D-Star reflector your club uses, a DMR talkgroup, an EchoLink link down the
road. astar keeps those in one list, so any of them is two clicks away, whichever
network the window is set to right now.

## The saved-nodes menu

Next to the dial field there is a small list button (three lines and dots).
Click it and a menu opens with two parts:

* **Favorites**: the places you starred, sorted by name.
* **Recents**: the last ten places you connected to, newest first. astar adds
  one each time a call is answered, so you never have to save anything to find
  it again later.

The menu holds every network at once. Each row starts with the network's
symbol and ends with its name, like *Local hub — 51234 · AllStar* or
*Worldwide — freedmr-network-server-freedmr-eu/91/2 · DMR*, so you can tell
them apart when the list mixes them.

Pick a row and two things happen. The network picker switches to that row's
network, and the dial field fills with the row's whole target: a node number,
a reflector and its module (`XLX836 A`), a DMR network with its talkgroup and
timeslot, or an EchoLink callsign. Nothing is dialled yet. Look it over and
press **Connect** when you are ready, the same as if you had typed it.

A row for a network that can't be used right now is shown greyed out with
*(not available)* after it. That happens, for example, to a D-Star favorite
while no vocoder stick is plugged in, because D-Star only appears in the
picker when one is.

## Starring a favorite

The star beside the dial field saves whatever is in the field as a favorite on
the network that is selected. Click it, give the favorite a name you'll
recognise, such as "Local 2m" instead of `51234`, and press **Save**. The star
turns yellow. Click it again to take the favorite off; if you have connected
to that place before, it stays in Recents.

On EchoLink, a favorite is saved under the station's callsign as EchoLink
lists it, even when you typed its node number. So starring `9999` and starring
`*ECHOTEST*` give you the same favorite. The stars in the EchoLink station
browser are the same favorites too, so a star you add there shows up in this
menu and the other way round. See [EchoLink](echolink.md#favorites).

Every network keeps its own numbers. AllStar node 1234 and EchoLink node 1234
are different stations, so starring one never stars the other, and the menu
can hold both.

## Settings → Favorites

**Settings → Favorites** lists every favorite from every network. Each row
starts with a small tag naming its network (*ASL*, *M17*, *DSTAR*, *YSF*,
*NXDN*, *DMR* or *ECHO*), followed by:

* **Name**: edit it in place and press Return to rename the favorite.
* **Target**: what the favorite dials. Edit it and press Return to point the
  favorite somewhere else on the same network. A target that isn't the right
  shape for that network, or one another favorite on the same network already
  uses, is outlined in red with the reason underneath.
* **Talk timer**: *Default* follows the talk-timer setting at the top of the
  section, *Custom* sets a limit for this favorite alone, and *Off* turns the
  timer off for it.
* The trash button deletes the favorite.

The row at the bottom adds an AllStar node by number. To save a favorite on
another network, star it beside the dial field, where its target is built.

## Moving favorites to another Mac

Exporting your configuration (Settings → **Backup** → **Export…**) carries
favorites and recents from every network in its **Node directory** section; see
[Saved configs](configs.md#export). Importing that file on another Mac adds
what it doesn't have yet and updates what it does, matching each one by its
network and target. An import never removes a favorite. Files exported by
older versions of astar import as they always did: their entries are AllStar
or M17, as they were saved.
