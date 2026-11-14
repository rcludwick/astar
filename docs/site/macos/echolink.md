---
icon: lucide/globe
---

# EchoLink

EchoLink is a network that lets licensed amateurs talk to each other over the
internet. Thousands of stations are signed in to it at any moment: people
talking from a computer or a phone, radio links and repeaters connected to
the internet, and conference servers that many stations join at once. Each
one has a callsign and a **node number**, a short number EchoLink assigns
when the callsign first signs in. You call a station by picking it from
EchoLink's list, and then you talk to it with push to talk, the same way you
would over AllStarLink.

In astar, EchoLink is one more segment in the network picker, and it dials
from the same row as every other network: a field, a magnifying glass, the
saved-nodes menu, a star and **Connect**. You type a callsign or node number
in the field, or pick a station from EchoLink's list behind the magnifying
glass, and press **Connect**.

## Before you start: register and validate your callsign

EchoLink only lets licensed amateurs on, so before astar can sign in for you,
EchoLink has to know two things. The first is that your callsign exists in its
system, which is called **registering**. The second is that you really hold
the licence for it, which is called **validating**. You do both once, by
hand, on EchoLink's side. astar never registers or validates anything.

1. **Register** by signing in once with EchoLink's own app, for example the
   free EchoLink app for iPhone. Use your plain callsign, with nothing added
   to the end, and choose a password. The app will say the callsign has not
   been validated yet. That's expected.
2. **Validate** on EchoLink's
   [validation page](https://www.echolink.org/validation/). For a US licence
   the usual way is to upload the official copy of your licence from the
   FCC's License Manager (a "reference copy" is refused). Volunteers check
   each request by hand, so expect to wait a while. EchoLink's
   [validation FAQ](https://secure.echolink.org/faq_validation.htm) covers
   other countries and the other ways to prove a licence.

Once your callsign is validated, it works from any EchoLink program, and
astar is one of them.

!!! warning "EchoLink sends your password unencrypted"

    EchoLink's sign-in sends the password across the internet as plain text,
    and astar cannot change how EchoLink's servers work. Anyone who can watch
    the traffic between you and EchoLink can read it. Use a password for
    EchoLink that you use nowhere else.

## Logging in

EchoLink needs you logged in before you can call anyone, and the login lives
in **Settings → EchoLink** (described in the next section). Fill in your
callsign and password there once.

Then pick **EchoLink** in the network picker. Under the dial row, one line
says where the login stands, with the button that moves it on:

| What the line says | What the button does |
|---|---|
| *Add your EchoLink callsign and password in Settings to connect.* | **Open EchoLink Settings…** takes you straight to the EchoLink section. |
| *Not logged in to EchoLink.* | **Log In** logs in with the callsign and password saved in Settings. |
| *Logging in as KX7ABC…* | **Cancel** stops the login. |
| *Logged in as KX7ABC* | **Log out** takes your callsign off EchoLink's list. |

astar signs in to EchoLink's directory, the service that keeps the list of
who is on. Logging in is not the same as calling someone. Being logged in puts
your callsign on EchoLink's list and fetches the list of everyone else. No
station is called until you press **Connect**.

### The password is kept in your Keychain

astar saves the EchoLink password in your macOS Keychain, the same place it
keeps your AllStarLink account. It is never written to astar's settings, to an
exported setup file or to a log. The dial card never shows a password field:
**Log In** there always uses the one saved in Settings.

### Logging in when astar starts

**Log in automatically**, in Settings → EchoLink, is a separate switch from
saving the password. Saving only remembers the password; the switch decides
whether astar uses it at launch. It takes effect only once a password is
saved. With the box ticked, astar logs in shortly after it starts. It never
calls a station by itself.

## The EchoLink section in Settings

Your EchoLink login lives in **Settings → EchoLink**, just below your
AllStarLink account. It is the one place the callsign, password and "Log in
automatically" are edited; the dial card and the station browser only log in
and out with what is saved here.

The section has these rows, from the top:

* **Status**: *Not logged in*, *Logging in as KX7ABC…* or *Logged in as
  KX7ABC*, the same words as the line under the dial row.
* **Callsign**: the callsign astar logs in to EchoLink as. You can only change
  it while you are logged out, because EchoLink holds a login to one callsign.
  "Log in automatically" belongs to a callsign, so switching to a callsign that
  has no password saved turns it off.
* **Password**: the password saved for that callsign. The field starts empty
  even when one is saved, and says *Saved — re-enter to change*. To see the
  saved password, click the eye button at the end of the field. astar reads it
  from the Keychain and shows it as plain text; click the eye again (now with a
  line through it) to hide it. Type a new password to replace the saved one.
* **Log in automatically**: whether astar logs in by itself when it starts. It
  can be ticked once a password is saved for the callsign above it.
* **Location**: a short line other stations see beside your callsign in
  EchoLink's list, usually your town. It is sent with your next status update
  to EchoLink, and with every login. Leave it empty to send none.

Changes save by themselves a moment after you stop typing, the same way the
AllStarLink account does. A password typed here is saved right away, before
EchoLink has checked it, because Settings is where you manage what is stored.

Below the rows, **Log in** logs in with what is saved, or with a password you
have just typed (**Log out** or **Cancel** takes its place while you are
logged in or logging in). **Clear** asks first, then deletes the saved
password for this callsign from the Keychain and turns off "Log in
automatically". Your callsign and location stay.

EchoLink sends this password unencrypted, so use one you use nowhere else. None of these settings go into
an exported `.astarconfig` file, so sharing your setup never shares your
EchoLink login.

### When the login fails

The status line says why, in plain words:

| What you see | What it means |
|---|---|
| *EchoLink said the password is wrong* | The directory turned the password down. Check it in Settings → EchoLink (the line under the dial row has a **Settings…** button) and click **Log In Again**. |
| *EchoLink refused the login ("…"). If you registered recently, your callsign may not be validated yet.* | The directory said no for another reason, quoted in the brackets. The most common one is a callsign that is still waiting to be validated. |
| *Couldn't reach any EchoLink server* | None of EchoLink's servers answered. astar keeps trying by itself, waiting a little longer each time, and **Cancel** stops it. |
| *The EchoLink password has a character the directory can't take: a line break, or a non-ASCII character such as ¬.* | EchoLink's login line cannot carry that character, so astar sends nothing. Change the password on EchoLink's site to plain letters, digits and punctuation. |

astar stops after a refusal, so a wrong password isn't sent again until you
ask.

## Finding a station

You can type a station straight into the dial field: its callsign as EchoLink
lists it (`KX7ABC-L`, `*ECHOTEST*`, in any case) or its node number. When you
don't know either, browse EchoLink's list.

Click the **magnifying glass** beside the dial field. The window shows the
**EchoLink Stations** pane, the same way it shows the reflector directory on
D-Star or M17. The Back chevron in its top-left corner (or ⌘[) returns to the
call card. If you aren't logged in yet, the pane says so and offers the same
**Log In** or **Open EchoLink Settings…** button as the dial row.

astar fetches the list when you log in and refreshes it every ten minutes. To
fetch it now, click the **Refresh** button (the circular arrow) beside the
search field. It says *Refreshed* for a moment and the new list follows a
second or two later. EchoLink's directory is asked at most once every 30
seconds, so a second click inside that time says *Try again in a moment*
instead.

* **Show** narrows the list to one kind of station: **All**, **Favorites**
  (the stations you starred; see [Favorites](#favorites) below), **People**
  (someone at a computer or phone), **Links** (a radio on simplex; their
  callsigns end in `-L`), **Repeaters** (callsigns ending in `-R`) or
  **Conferences** (servers many stations join at once). A conference's
  callsign is written in stars, which is why EchoLink's test station,
  `*ECHOTEST*`, is under Conferences.
* **Search** matches a callsign anywhere in it, in any case, or the start of
  a node number. Typing `w1` finds `W1AW` and `KW1X`; typing `99` finds node
  9999 and every other node number that starts with 99. Press Return with one
  match left and astar selects it and moves to **Choose**.

Each row shows the callsign, the description the station gave itself, its
node number, and a dot:

| Dot | Meaning |
|---|---|
| Filled green | Free: the station is on and not in a call. |
| Filled orange | Busy: the station is already talking to someone. Many stations take only one call at a time. |
| Grey ring | EchoLink didn't say. |

The busy and free state comes from a short tag EchoLink adds to the end of a
station's description, so a station that leaves it out shows the grey ring.
VoiceOver reads "Free", "Busy" or "Status unknown" for each row, so the colour
is never the only clue.

Select a row and click **Choose**, or double-click it. The pane closes and the
station's callsign is in the dial field. Choosing never calls anyone: you
still press **Connect** on the dial row.

## Favorites

Most people call the same handful of stations again and again: a local link, a
net's conference, a friend. EchoLink favorites are ordinary astar favorites,
kept in the same list as your AllStar nodes, reflectors and DMR talkgroups.
That means a starred EchoLink station is in the **saved-nodes menu** beside the
dial field (the list button) whichever network is selected, and picking it
switches the network picker to EchoLink and fills in the callsign. See
[Favorites and recents](favorites.md) for the menu itself.

There are three stars, and they are all the same favorite:

* the **star beside the dial field**, for whatever is typed there;
* the star at the right-hand end of each row in the **station browser** (or
  right-click a row and choose **Add to Favorites**);
* the star next to the far station's name **while you're on a call**, so you
  can save a station you've just found.

A favorite is saved under the station's callsign as EchoLink lists it, in any
mix of upper and lower case, even when you typed its node number. Starring
`9999` beside the dial field and starring `*ECHOTEST*` in the browser are the
same favorite, and a station that comes back under a new node number is still
that favorite. EchoLink node 1234 and AllStar node 1234 are different places,
so they are different favorites.

In the station browser, choose **Favorites** under **Show** to see only your
starred stations. Each one appears in one of two ways:

* **It's in EchoLink's list right now.** The row looks like any other, with
  its green or orange dot, and you can choose it.
* **It isn't.** The station is off the air, or hasn't logged in to EchoLink
  yet. Its row stays in the list, greyed out with *Not online* under the
  callsign. You can't choose it until it comes back; when it does, the row
  turns back into a normal one by itself the next time the list refreshes.

The count under the list reads like *4 favorites, 3 online*.

Favorites are saved on this Mac and survive a restart. They are listed, with
an *ECHO* tag, in **Settings → Favorites**, where you can rename them. Exporting
your configuration (Settings → **Backup** → **Export…**, see
[Audio configs](configs.md#export)) carries them with the **Node directory**
section, and importing that file adds them to the favorites already on the
other Mac. It never removes any.

With VoiceOver, each row in the browser reads "favorite" after the station's
details when it's starred, and the row's actions (VoiceOver-Command-Space)
include **Add to favorites** or **Remove from favorites**.

## Calling a station

Put the station in the dial field (type it, choose it in the station browser,
or pick a favorite or recent from the saved-nodes menu) and click **Connect**,
or press Return in the field. **Connect** stays off until you are logged in
and the field names a station in EchoLink's list. When it doesn't, a line
under the field says so, for example *N0CALL isn't in the station list. Check
the spelling, or refresh the list.*, with a **Refresh List** button for a
station that logged in after astar last fetched the list.

While astar waits for the far end to answer, the call card says *Calling on
EchoLink…*; once it answers, *Connected on EchoLink*. Each answered call is
added to **Recents** in the saved-nodes menu, under the station's callsign.

A good first call is **`*ECHOTEST*`, node 9999**. It is EchoLink's own test
station: whatever you say, it plays back to you a moment later. Type `9999`
in the dial field, connect, key up, say a few words, let go, and listen. If you hear
yourself, your microphone, speaker and connection all work.

Talking works the way it does on every other network in astar:

* **Push to talk** is the same button, hardware key or VOX you already use.
* **A stuck key is always let go.** EchoLink stops transmitting after 10
  minutes of continuous talk; the talk timer warns before that. The call card
  then says *EchoLink stopped transmitting after 10 minutes of continuous talk.
  Release PTT to talk again.* No setting turns this off.
* **The meters** show what you are sending and receiving.

**Disconnect** hangs up and leaves you logged in, so you can call someone
else straight away. **Log out** takes your callsign off EchoLink's list.

astar holds one call at a time across all its networks, so while an EchoLink
call is up, AllStarLink, M17 and the others wait, and the other way round.

### When a call doesn't start

| What you see | What it means |
|---|---|
| *… isn't in the station list. Check the spelling, or refresh the list.* (under the dial field, with **Connect** off) | The station isn't in the list astar has, which can be up to ten minutes old. Check the callsign, or click **Refresh List** and try again in a moment. |
| *Couldn't open UDP ports 5198–5199 — is another EchoLink program running?* | EchoLink calls use UDP ports 5198 and 5199, and only one EchoLink program can hold those ports at a time. Quit the other one (EchoLink itself, or another copy of a link program) and call again. |

## Chat and station information

Most EchoLink stations send a few lines about themselves when you connect:
their location, their equipment, sometimes the rules of a net. astar shows
that **station information** text above the chat.

Below it is the **chat**: a text box and a **Send** button. A message goes to
the station you are connected to, and its replies appear in the list,
yours in your accent colour. The chat belongs to this one call: hang up and
it clears.

## Voice quality

EchoLink carries voice in **GSM 06.10**, a codec from the first digital mobile
phones. It is narrowband, about 13 kilobits a second, so EchoLink sounds like
a phone call rather than like a wideband AllStarLink link. That's EchoLink's
format, not a setting: every EchoLink program uses it, so astar does too.

Received audio plays through the same **jitter buffer** as an AllStarLink
call. EchoLink sends voice in packets of 80 ms, and they never arrive exactly
80 ms apart: one is a little late, the next two come together, and now and
then the network stalls for a moment and delivers a burst. The buffer holds a
small cushion back so the speaker always has the next piece ready, and it
keeps that cushion between the **Min** and **Max** you set under **Jitter
buffer** (see [Jitter buffer](first-run.md#jitter-buffer-leave-it-on)). When
the cushion grows past what the network needs, it gives the extra back by
skipping a 20 ms piece now and then. That matters most on a busy conference
or repeater, where one station follows another with no pause: the delay stays
the same for the whole call instead of creeping up.

While a call is up, the line under the meters shows what the buffer is doing,
for example `jitter 30 ms · buffer 120 ms · lost 2 · late 0`. **lost** counts
the pieces that never arrived and the ones skipped to keep the delay down;
**late** counts the ones that arrived after their turn to play and were
thrown away. A few of each over a long call is normal.

## Things worth knowing

* **One sign-in per callsign.** EchoLink allows one program at a time to be
  signed in with a callsign. If your phone's EchoLink app is signed in with
  the same callsign, close it first, or the two knock each other off.
* **One station per public IP address.** EchoLink's rules allow one
  signed-in station for each public internet address. Everything behind your
  home router shares one address, so don't run two EchoLink programs from the
  same home at once.
* **No port forwarding needed.** astar only calls out, and calling out works
  through an ordinary home router. It needs TCP port 5200 and UDP ports 5198
  and 5199 open going out, which a home router allows by default.
* **Incoming calls are not accepted in this version.** Other stations see you
  on the list, but if one of them calls you, astar ignores the call. You can
  only call them.
* **Not every connection works.** A mobile network or an internet provider
  that puts many customers behind one shared address (called carrier-grade
  NAT) can stop EchoLink's audio from getting through. EchoLink's own way
  around that, its Proxy service, is not supported yet.
* **Never share or lend your password.** EchoLink treats that as a
  permanent ban.

## Windows and Linux

The Windows and Linux client is paused while it is being replaced, so it has
no EchoLink. EchoLink will be part of the replacement client.
