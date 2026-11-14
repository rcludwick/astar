---
icon: lucide/search
---

# Searching the reflector directory

astar carries a list of digital-voice reflectors: thousands of D-Star, M17,
System Fusion, NXDN and DMR rooms, with their names, where they are and who
runs them. You can search that list instead of remembering a reflector's
name, and pick one to put it in the dial field. While you are logged in to
EchoLink, the same search covers EchoLink's stations too.

## Opening the search

Set the network picker to a reflector network, such as M17 or D-Star, and
click the **magnifying glass** beside the dial field. The window shows the
search pane. The Back chevron in its top-left corner (or ⌘[) returns to the
call card.

There is no magnifying glass on AllStarLink, because AllStar nodes are not in
the list. On EchoLink the magnifying glass opens EchoLink's own station
browser instead; see [EchoLink → Finding a station](echolink.md#finding-a-station).

## Searching

Type in the search field at the top. A reflector matches when its name, its
number, an older name it went by, its description, its country or its
sponsor contains what you typed, in any case. So `836` finds `XLX836`, and so
does `XRF836`, its older name.

The menu at the top right is the **network filter**. It starts on the network
the dial field is set to, and lists only networks the directory actually
has. Choose **All networks** to search everything at once. Each row carries a
small label naming its network, so mixed results stay readable.

A reflector that is listed but that astar cannot dial yet (P25, for example)
still shows up, greyed, with a line saying why. Hiding it would make the list
look smaller than it is.

## Choosing a reflector

Click a row to choose it. Choosing never connects: it fills the dial field,
switches the network picker to that row's network, and takes you back to the
call card. Connecting is still the **Connect** button, a second deliberate
click.

On D-Star a reflector is split into rooms called **modules**, each a letter
from A to Z, and the directory does not say which ones are in use. So after
you choose a D-Star reflector, astar asks which module you want rather than
guessing one for you. The letter you last used on that reflector is shown in
bold.

## EchoLink stations in the search

While you are logged in to [EchoLink](echolink.md), the search also covers
the list of stations EchoLink's directory sends astar:

* The network filter gains an **EchoLink** choice, at the end. Choose it to
  see only EchoLink stations.
* **All networks** lists EchoLink stations after the reflectors, in callsign
  order.
* A station matches when its callsign or its location text contains what you
  typed, or when its node number starts with it. Node numbers match from
  their first digit: typing `99` finds node 9999 and every other node whose
  number begins with 99.
* Each EchoLink row shows the station's callsign, its node number, the
  **EchoLink** label, and underneath its location and whether it is **Free**
  or **Busy**.

Choosing an EchoLink station works like choosing a reflector: astar switches
the network picker to EchoLink and puts the station's callsign in the dial
field (or its node number, when the callsign has a character the field
cannot hold), exactly as choosing it in EchoLink's own station browser does.

The EchoLink rows follow your login. When EchoLink refreshes its list, the
search results update by themselves. When you log out, the EchoLink rows and
the EchoLink filter disappear; if the filter was on EchoLink, the search shows
**All networks** meanwhile. astar remembers that you chose EchoLink, so when
the login comes back (after a brief drop, say) the filter returns to
EchoLink, unless you picked another network in between.

## Keeping the directory current

astar ships with a copy of the reflector directory and refreshes it on its
own, as often as the directory's publisher asks. **Settings → Reflector
directory** shows how fresh your copy is and has a **Sync Now** button. The
line at the bottom of the search pane counts the matches and credits the
directory's publisher.
