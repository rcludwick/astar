# nodebench fixtures

## `link-test-phrase.wav`

The spoken phrase the speech-echo scenarios send through a link and expect
back. Every run, on the Mac and in CI, sends exactly these samples.

| | |
|---|---|
| Text | "This is the astar link test. One, two, three, four, five. Testing complete." |
| Format | WAV, 8000 Hz, mono, 16-bit signed PCM, 41 428 samples (5.18 s), 82 944 bytes |
| Level | Peak normalised to -6 dBFS |
| SHA-256 | `61b48d5903a34f7e219d5398ad95e88f8e133a5f94c2d180e436752caf987255` |
| Speech | eSpeak NG 1.52.0 (Homebrew bottle `espeak-ng 1.52.0`), voice `en-us` (`gmw/en-US`, the built-in formant voice), 200 words per minute, word gap 30 ms |
| Resampling | SoX 14.4.2 (Homebrew `sox 14.4.2_6`), dither off so the output is byte-for-byte repeatable |
| Made | 2026-11-13, on macOS (arm64) |

The phrase was chosen for what it exercises: it starts on a fricative ("Th"),
has voiced and unvoiced sounds, and has clear pauses at the commas and full
stops, so a lost first syllable, a hole, or a repeated chunk all show up in
the loudness curve.

### How to make it again

```sh
tmp=$(mktemp -d)
espeak-ng -v en-us -s 200 -g 3 -w "$tmp/raw.wav" \
  "This is the astar link test. One, two, three, four, five. Testing complete."
sox -D "$tmp/raw.wav" -r 8000 -c 1 -b 16 -e signed-integer link-test-phrase.wav \
  silence 1 0.01 0.5% reverse silence 1 0.01 0.5% reverse gain -n -6
```

`espeak-ng` writes 22 050 Hz. The `sox` line resamples to 8 kHz, trims the
silence espeak puts before and after the speech (so the phrase starts and
ends on speech, and "the first and last 200 ms" mean the first and last
sounds), and normalises the peak to -6 dBFS. Running both lines twice gives
the same SHA-256 above with the same tool versions; a different eSpeak NG
version may pronounce it slightly differently, which is why the file is
committed rather than generated at test time.

### Licence

eSpeak NG is GPL-3.0-or-later, and SoX is GPL-2.0-or-later/LGPL. Neither
licence covers what the programs produce: the GPL governs the program, and
its output is covered only when it copies part of the program into it (GNU
GPL FAQ, "Is there some way that I can GPL the output people get from use of
my program?"). The `en-us` voice is eSpeak NG's own rule-based formant
synthesis, with no recorded human voice in it, so there is no speaker or
voice-database licence either. (The MBROLA voices, which do carry their own
licence terms, were deliberately not used.) The text is ours.

So the file is part of astar and is covered by the repository's
AGPL-3.0-only licence like the rest of this crate. Even on the stricter
reading that the waveform were a GPL-3.0-or-later work, GPLv3 section 13
allows it to be conveyed together with AGPL-3.0 code, so committing it here
is permitted either way.
