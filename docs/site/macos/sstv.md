---
icon: lucide/image
---

# SSTV pictures

Sometimes, in the middle of a net, the audio turns into a long warble: a
two-tone chirp, then a steady rasping buzz that goes on for a minute or two.
That is **SSTV**, slow-scan television, a still picture sent as sound. The
sending station turns each line of the picture into a run of tones between
1100 and 2300 Hz, sends the lines one after another, and a receiver that
knows the code turns the tones back into pixels.

astar knows the code. When a picture comes in on a call, a picture card
appears in the astar window under the call card, counting through the
picture as it airs. When the picture ends, it appears in the card and is
saved as a PNG file on your Mac. Every picture saved is also kept in a
gallery window.

SSTV decoding is in the macOS app. The Windows/Linux client is paused and
does not decode pictures yet. `astar-server` does not decode them either:
a node is not a picture viewer. To keep the pictures from nets you record,
see the [net recorder](../netrec.md#sstv-pictures).

## astar never sends a picture

astar only **receives** SSTV. There is no way to send a picture from it: no
button, no setting, and no picture encoder in any build that ships. Turning
decoding on changes nothing about what astar transmits. It only listens to
the audio you are already hearing.

## Which networks can carry a picture

A picture survives only if the network carries the tones as tones. That
depends on the network's **codec**, the method it uses to squeeze audio
into packets.

| Network | Codec | Does a picture get through? |
|---|---|---|
| AllStarLink | G.711 µ-law, 16-bit linear, or GSM | Yes. The first two are waveform codecs: they copy the shape of the sound, and the tones sit well inside what they carry. GSM smears a little. |
| EchoLink | GSM 06.10 | Usually, with some smearing. |
| M17 | Codec 2 | No. |
| D-Star | AMBE | No. |

M17 and D-Star use **vocoders**: codecs that don't copy the sound at all.
They measure a voice (its pitch, its loudness, the shape of the mouth) and
send those measurements, and the far end synthesises a voice from them.
That works well for speech and destroys anything that isn't speech, so an
SSTV picture sent over M17 or D-Star arrives as noise. The same is true of
Fusion, DMR and NXDN.

So in practice, pictures come in on AllStarLink and EchoLink calls.

## Turning it on

It is on already. Open Settings → **SSTV**:

* **Decode SSTV pictures** turns decoding on and off. It is on unless you
  turn it off. Decoding only reads audio that is already playing, so it
  costs a little processor time and nothing else.
* The folder row shows where pictures are saved, `~/Pictures/astar SSTV`
  unless you choose another. **Choose…** picks another folder; pictures
  already saved stay where they are. **Show in Finder** opens the folder,
  creating it first if nothing has been saved yet. If the folder can't be
  created, the reason appears in red under the row.
* **Show Pictures** opens the gallery window.

These settings belong to this Mac. They are not part of an exported
`.astarconfig`, because a folder path means nothing on another computer.

## Watching a picture come in

A picture starts with a **VIS header**: a short burst of tones, about a
third of a second long, that says which **mode** follows. A mode is one
recipe for turning a picture into sound: how many lines, how wide, how
long each line takes, and in what order the colours go. astar starts a
picture only on a valid header, so someone whistling into a microphone
never turns into a picture.

When a header arrives, the picture card appears:

* The frame takes the picture's shape and fills downwards in the accent
  colour as time passes, with a thin line at the row the picture has
  reached.
* The status line says what is happening and how far along it is, for
  example **Receiving PD120 · 0:48 of 2:06**.
* Under it are the mode, the talker (when the network names one) and the
  time.

The pixels themselves appear at the end. astar holds the whole picture
before drawing it, because it straightens out the timing of every line
first (see [Why pictures come out clean](#why-pictures-come-out-clean)).

When the picture is done, the card shows it with a green **Saved**. The
card stays until you close it with the **×** in its corner or the next
picture starts. It also stays if the call ends, so a picture that finishes
as the far end hangs up is still there to look at. **All Pictures** in the
card's header opens the gallery.

### The common modes

| Mode | Size | Length | Heard on |
|---|---|---|---|
| Robot 36 | 320 × 240 | 36 s | VHF/FM and repeaters |
| Martin 1 | 320 × 256 | 1:54 | HF, mostly in Europe |
| Scottie 1 | 320 × 256 | 1:50 | HF, mostly in the US |
| Scottie DX | 320 × 256 | 4:29 | weak-signal HF |
| PD120 | 640 × 496 | 2:06 | the ISS |
| PD180 | 640 × 496 | 3:07 | the ISS |

astar also decodes PD240, Robot 24 and 72, Martin 2, Scottie 2, Wraase
SC-2 120 and 180, and the black-and-white Robot modes. Those are rarer, so
they are tested less.

## Partial and lost pictures

A picture can stop part way: the sender lets go of the key, the link drops,
or the call ends. astar treats **two seconds of silence** in the middle of
a picture as the signal lost.

* If at least **half** the lines arrived, the picture is kept. The card
  shows it in orange as **Partial picture saved**, with the bottom missing,
  and the gallery marks it **Partial**.
* With less than half, it is not worth keeping. The card says **Picture
  lost** in red, and nothing is saved.

A header followed by noise ends the same way. Sometimes a burst of
interference looks enough like a header to start a picture, and what
follows decodes to nothing that looks like a picture. astar throws that
away rather than saving a file of static, and the card says **Picture
lost** until you close it.

On AllStarLink there is one more chance. A picture whose live decode was
lost, or thrown away as noise, is decoded a second time from its packets
(see [The packets behind an AllStarLink
picture](#the-packets-behind-an-allstarlink-picture)). If that rebuilt
version has at least half the lines, it is saved after all.

## Where the files go

Each picture is a PNG in the SSTV folder. Its name says when it started, on
which network, in which mode, and who sent it when the network said:

```
2026-11-11 19-04-12 AllStar PD120.png
2026-11-11 19-20-31 EchoLink Robot36 N0CALL.png
```

The time is your Mac's local time, with dashes instead of colons, because a
colon can't go in a file name. The mode is written without spaces. If a file
of that name already exists, the new one gets a number on the end
(`… PD120 2.png`) rather than replacing it.

AllStarLink doesn't say who is talking, so an AllStarLink picture's name
has no callsign. EchoLink does.

Beside the pictures, astar keeps `pictures.jsonl`, a list with one line per
picture saved: the file name, the time, the network, where you were
connected (the node or conference), the mode, the talker and whether it is
partial. The gallery reads it.

An AllStarLink picture that kept its packets has three more fields on its
line: `packets`, the name of its `.pcap` file; `codec`, the codec the call
used (`ulaw`, for example); and `source`, which says whether the saved
picture is the one decoded `live` from the audio as you heard it or the one
rebuilt from the `packets`. Other pictures' lines don't have these fields.

### The packets behind an AllStarLink picture

An AllStarLink picture comes with a second file of the same name ending in
`.pcap`:

```
2026-11-11 19-04-12 AllStar PD120.png
2026-11-11 19-04-12 AllStar PD120.pcap
```

That file holds the voice packets that carried the picture, exactly as they
arrived from the network, from a few seconds before the header to a couple of
seconds after the end. It is a standard packet capture, so Wireshark opens
it. For a two-minute picture on a G.711 µ-law call it is about 1.3 MB:
around 10 KB for every second of audio, counting the packet headers, plus
the few seconds kept either side. A call using 16-bit linear audio makes a
file about twice that size, and one using GSM about half a megabyte.

astar uses it straight away. Packets on the internet don't always arrive in
the order they were sent, and the audio you hear has to be played the moment
it arrives. Afterwards, though, astar can sort the packets by the time stamp
each one carries and rebuild the picture's audio in the order it was sent,
knowing exactly where any packet went missing. It decodes that rebuilt audio
too and keeps whichever picture came out better. Both versions land in the
same file name, so you only ever see one picture. The `.pcap` stays so the
picture can be decoded again later, for example with
[`astar-netrec sstv`](../netrec.md#decoding-a-recording-by-hand).

EchoLink pictures have no `.pcap`.

## The gallery

The gallery is a window called **SSTV Pictures**. Open it with **Show
Pictures** in Settings → SSTV, or **All Pictures** on the picture card.

It shows every picture in the SSTV folder's list, newest first, as a grid
of thumbnails. Under each one are the mode and where you were connected,
then the talker and the date and time. Partial pictures carry an orange
**Partial** badge. Double-click a picture, or use **Show in Finder** from
its right-click menu, to find the file.

The gallery keeps up by itself. A new picture appears as soon as it is
saved, and a file you delete in Finder drops out the next time you bring
the window forward. A PNG copied into the folder by hand doesn't appear,
because it has no line in `pictures.jsonl`.

## Why pictures come out clean

An SSTV picture is far fussier about timing than a voice. Each line starts
with a short **sync pulse**, a burst at 1200 Hz, and the decoder places each
line by where its pulse falls. If 20 milliseconds of audio go missing in the
middle of a picture, every line after that point arrives 20 milliseconds
early, and the rest of the picture shifts sideways into a slanted tear. A
voice with the same 20 ms missing just sounds like a tiny click.

Audio over the internet loses and gains bits like that all the time, so
astar does three things while a picture airs:

* **It finds every line's sync pulse and re-times the line.** Before the
  picture is decoded, astar locates each pulse, works out where audio was
  lost or added, and stretches the audio back onto a perfect grid. A missing
  piece of a line is filled from the line above to keep the timing, then
  smoothed by blending the rows above and below it. A line where the signal
  drops out, even for a fraction of a second, is rebuilt the same way. After
  a longer dropout, astar finds the line pulses again even if the picture
  comes back shifted.
* **It keeps the jitter buffer steady.** The jitter buffer is the small
  delay astar keeps so late packets can still play in order (see
  [Jitter buffer](first-run.md#jitter-buffer-leave-it-on)). When a sender
  runs slightly fast, the buffer normally drops an occasional packet to
  keep the delay short. Nobody hears that in speech, but it would tear a
  picture, so while a picture is airing the buffer holds and drops nothing.
* **On AllStarLink, it rebuilds the picture from the packets** as described
  [above](#the-packets-behind-an-allstarlink-picture).

## Credits

The decoder at the heart of this is **slowrx.rs** by Jason Herald, a Rust
port of **slowrx** by Oona Räisänen. Both are free software (MIT and ISC);
their notices are in `LICENSE-EXCEPTIONS.md` §10, in astar's source (see
[Licence](../about/license.md)). The timing repair that lines a picture's scan
lines back up before slowrx reads them lives in astar's fork of the **`sstv`**
crate by Tobias Klockau, also MIT; its notice is in `LICENSE-EXCEPTIONS.md`
§14.
