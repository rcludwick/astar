# Additional permissions and third-party notices

astar is licensed **AGPL-3.0-only**; the full text is in [`LICENSE`](LICENSE).
This file records the additional permission granted under section 7 of that
licence, and the notices that third-party components require.

Nothing here weakens the AGPL for astar itself. If you modify astar and let
other people use it over a network, those users are still entitled to the
source of your modified version.

---

## 1. App Store distribution exception

*Additional permission under GNU AGPL version 3, section 7.*

Rob Ludwick is the sole copyright holder of astar. As that copyright holder, he
grants the following additional permission:

> You may convey astar, or a work based on astar, through Apple's App Store or
> another application-distribution platform, notwithstanding the additional
> restrictions that such a platform's terms of service impose on recipients —
> including restrictions on which devices the work may be run on, limits on the
> number of copies that may be installed, digital rights management applied to
> the conveyed binary, and prohibitions on redistribution or reverse
> engineering.

### Why this exists

The AGPL forbids imposing further restrictions on a recipient's exercise of the
rights it grants. App-store terms impose exactly such restrictions on whoever
downloads the binary — device binding, install limits, DRM. That conflict is
about the terms attached to the *distributed binary*, not about source
availability: astar's source is public either way. This permission resolves it
for astar's own code.

### What it does not cover

This permission extends only to the parts of astar whose copyright Rob Ludwick
holds. It does **not** extend to the third-party components listed in the README
or in section 2 below, which carry their own terms and which he cannot grant
permissions over.

In particular, it does not resolve the Codec 2 question for app-store
distribution — see section 2.

### Removing it

Under AGPL section 7, a downstream recipient may remove this additional
permission from their own copy or from a modified version. Nobody may add
further restrictions of their own.

---

## 2. Codec 2 (LGPL) — notices and written offer

M17 voice uses **Codec 2**. astar can obtain it two ways, both opt-in Cargo
features that are deliberately **never** part of any default feature set (see
`crates/astar-codec/src/codec2.rs` and the `[features]` comment in
`crates/astar-codec/Cargo.toml`), so a plain `cargo build` links no LGPL code:

| Feature | Component | Licence |
|---|---|---|
| `codec2-static` | the `codec2` Rust crate (a reimplementation), vendored at `vendor/codec2` and linked into the binary | `LGPL-2.1-only AND MIT` |
| `codec2-runtime` | the system `libcodec2` C library, `dlopen`ed at runtime, never linked | LGPL-2.1 |

**astar-server's single static release** (`just server-release`, the
`release` feature) is built with `codec2-static`: Codec 2 is linked into that
executable, and everything below about static linking and the right to relink
applies to it exactly as it does to the macOS app. A default `cargo build` of
astar-server links no Codec 2. Older container images installed Debian's
`libcodec2` runtime package and loaded it with `dlopen`; the image built from
the static release no longer does.

**astar-netrec's release build** (`cargo build --release -p astar-netrec
--features release`, see `docs/RELEASING.md`) is built with `codec2-static` in
the same way, and everything below applies to it too. A default `cargo build`
of astar-netrec links no Codec 2: it looks for the system `libcodec2` at
runtime.

### You are free to change Codec 2 and rebuild astar

Stated plainly, because it is the point of everything in this section and not
merely a licence obligation being discharged:

**Nothing here restricts anyone from modifying Codec 2, or astar, and building
their own astar from the result.** You may patch the codec, swap in a different
implementation, fix a bug in it, tune it, or replace it wholesale, and rebuild
astar against your version. You do not need permission, you do not need to ask,
and you do not owe anyone an explanation. The AGPL asks you to pass the same
freedom on to anyone you distribute your version to; it asks nothing else.

astar links Codec 2 statically for a practical reason — a hardened-runtime macOS
binary cannot load an unsigned dylib, and most Macs have no libcodec2 at all —
not to make it harder to replace. Everything needed to swap it out is published.

### Static linking and your right to relink

Where a released astar binary is built with `codec2-static`, it contains LGPL
code linked into the executable. LGPL-2.1 section 6 requires that a recipient be
able to modify the library and relink it into the application.

astar satisfies this by publishing its **complete, buildable source**:

* Source: <https://github.com/rcludwick/astar>
* The Codec 2 source itself ships in that repository, at `vendor/codec2`, with
  the LGPL-2.1 text beside it — not merely as a reference to crates.io.
* Every release is tagged, so the exact source for a given binary, the library
  included, is `git checkout v<version>`.
* Build instructions: <https://rcludwick.github.io/astar>

Anyone may replace the Codec 2 implementation with their own modified version
and rebuild astar from that source — the simplest way is to edit
`vendor/codec2` in place. No part of astar is withheld.

### Modifications

astar ships Codec 2 **unmodified**, as published upstream. The copy in
`vendor/codec2` is the published `codec2` 0.3.1 crate with its source files
byte-for-byte intact; the only additions are the LGPL-2.1 text (upstream ships
none), a provenance file, build plumbing that keeps the workspace's tooling
from reformatting or adopting it (a `rustfmt.toml`; and, in its `Cargo.toml`, a
lint cap, an empty `[workspace]` table and comments), all listed in
`vendor/codec2/VENDORED.md`. Should the source ever change, the modified files
will be published in this repository alongside the release that carries it, and
identified as changed, as LGPL-2.1 section 2 requires.

### Upstream

* Vendored copy: `vendor/codec2` in this repository — version, upstream
  commit and crates.io checksum in its `VENDORED.md`
* `codec2` Rust crate: <https://crates.io/crates/codec2>
  (repository: <https://github.com/scriptjunkie/codec2>)
* Codec 2, by David Rowe: <https://www.rowetel.com/codec2.html>

### App-store distribution: decided 2026-11-21

Rob decided on 2026-11-21 that astar's App Store builds carry both LGPL
vocoders: Codec 2 (M17, this section) and the software D-Star vocoder
(section 9).

The decision rests on a discussion on the `digitalvoice` list
(<https://groups.google.com/g/digitalvoice/c/XoYeEGE4sLk>, September–October
2026), about Codec 2 in a third-party iPhone and Android app:

* **Bruce Perens (6 October 2026)** accepted app-store distribution
  ("I think we have to live with it"). He spoke of LGPL code generally ("the
  LGPL terms", "the LGPL part"), not of Codec 2 alone and not of one licence
  version. He set two conditions:
  * users must "be able to re-link your application if they fix a bug in the
    LGPL part";
  * the source should be served for download, complete enough to rebuild the
    binary bit-identically.
* **Mooneer Salem (K6AQ)** of the FreeDV project agreed, saying he was not
  a lawyer.

Perens owns Codec 2's `pack.c`, and he is the author of the D-Star vocoder,
so his statement speaks for his own code in both. The rest of Codec 2 is
David Rowe's, and Salem spoke for the FreeDV project, which maintains it.
Nobody on the thread discussed LGPL-3.0's "installation information" terms
by name. Rob takes Perens' general statement as covering them, because the
relink path below includes installing the rebuilt app.

astar's store builds must therefore:

* **Serve the complete corresponding source.** Each store release links to a
  source download that astar itself serves: the public repository at the
  release's tag, which includes `vendor/codec2` and
  `vendor/ham-digital-modes-ambe`. That source must build the same binary.
* **Keep each LGPL vocoder relinkable.** In the store builds, each vocoder
  lives in its own replaceable library or framework, not merged into the
  app's binary. A user can then rebuild it from source, re-link it, and
  install the result. On iPhone that means re-signing the app with their own
  developer identity, and the app's documentation explains how.
* **Carry the notices and the licence texts** (LGPL-2.1 for Codec 2,
  LGPL-3.0 for the D-Star vocoder) in the app's licences screen, together
  with the written offers in this section and section 9.

A cleanly-licensed Codec 2 (`iax-e5d9` in `docs/BACKLOG.md`) is no longer
needed for the store. It stays as a fallback in case Apple or the copyright
holders object. The research is in Rob's notes,
`superpowers/notes/astar/2026-11-21-lgpl-app-store-m17.md` and
`2026-11-21-codec2-non-lgpl-feasibility.md`.

This does **not** affect astar's current distribution: the macOS release is
distributed directly, signed and notarized, from the project's own releases
page, where the published source fully satisfies the relink requirement.

The App Store build also carries the GPL-3.0-or-later P25 IMBE vocoder
(`vendor/imbe-vocoder`), under section 15's terms and the risk Rob accepted
there, not under the LGPL conditions in this section.

## 3. nnnoiseless / RNNoise (BSD-3-Clause) — notices

Mic noise reduction uses **`nnnoiseless`**, a pure-Rust port of Xiph's RNNoise
(`crates/astar-audio/src/rnnoise.rs`). Unlike Codec 2 it is an ordinary
crates.io dependency rather than vendored source, and it is **not** behind an
opt-in feature: it is compiled into every build, and the ~87 KB model is a
const array inside the crate rather than a file to locate at runtime.

BSD-3-Clause is GPL-compatible, so the combination is fine and a shipped astar
binary is distributable under the AGPL. What BSD-3 asks in return is
attribution: the copyright notice, the licence text and the no-endorsement
clause reproduced in the documentation or other materials accompanying a binary
distribution. This section is that.

Note that **five** copyright lines ship together, not one — the Rust port, the
Mozilla-era RNNoise, Jean-Marc Valin's original work, the Xiph.Org Foundation
(whom the no-endorsement clause names specifically), and Mark Borgerding's
KISS FFT.

Nothing here implicates the App Store exception in section 1 or the relink
offer in section 2: BSD-3 imposes no relink obligation and no app-store
conflict.

```
Copyright (c) 2020, Joe Neeman
Copyright (c) 2017, Mozilla
Copyright (c) 2007-2017, Jean-Marc Valin
Copyright (c) 2005-2017, Xiph.Org Foundation
Copyright (c) 2003-2004, Mark Borgerding

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of the Xiph.Org Foundation nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE FOUNDATION
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## 4. `astar-discord` (feature `discord`, off by default)

The Discord voice bridge. Everything it pulls in is permissive or MPL-2.0, so
unlike `codec2` there is no relink obligation and no copyleft interaction with
AGPL-3.0-only.

| Dependency | Role | License |
|---|---|---|
| `songbird` 0.6 | Discord gateway, voice websocket, AEAD, RTP | ISC |
| `serenity` 0.12 | Discord API client songbird drives | ISC |
| `tokio` | the runtime songbird's `driver` feature requires | MIT |
| `opus2` 0.4 | songbird 0.6's safe binding to libopus | MIT/Apache-2.0 |
| libopus | Opus encode at 48 kHz stereo, linked by `opus2` | BSD-3-Clause |
| `symphonia` 0.5.5 (+ `-core`, `-metadata`) | songbird's audio input decoding | **MPL-2.0** |
| `symphonia-codec-pcm` 0.5.5 | decodes the raw PCM the bridge streams into songbird — see below | **MPL-2.0** |
| `hpke-rs` 0.6.1 (+ `-crypto`, `-rust-crypto`) | HPKE under `davey`, songbird's DAVE end-to-end-encryption layer | **MPL-2.0** |

**MPL-2.0 is not "permissive", and it is not a problem here.** It is weak,
file-level copyleft: the obligation attaches to the MPL-licensed *files*, not
to anything that links against them. §3.3 ("Distribution of a Larger Work")
explicitly permits distributing the covered software as part of a Larger Work
under a secondary licence — the GNU licences are named — so shipping these
crates inside an AGPL-3.0-only binary is exactly the case the licence
anticipates. There is no relink obligation and nothing to offer beyond the
source, which is the condition that is real: recipients must be able to get
the unmodified MPL sources. astar modifies none of them and every one is an
unpatched crates.io release, so a crate name and version discharges it.

`hpke-rs` is not asked for directly — it arrives through `davey`, songbird's
DAVE end-to-end encryption. `hpke-rs-libcrux` 0.6.1 appears in `Cargo.lock` but
is not in the resolved graph — it is an optional backend nothing selects — so
it is not shipped.

**`symphonia` IS asked for directly, for exactly one cargo feature.** No code
in the workspace names it. The audio path into Discord is songbird's raw
container, which tags its track `CODEC_TYPE_PCM_F32LE` and leaves decoding to
whichever symphonia codecs are compiled in; songbird turns those on only
through its own default features, which `astar-discord` disables. Without
`pcm` there is no decoder at all, and the bridge reported itself connected
while every track died on its first frame. `astar-discord/Cargo.toml`
therefore carries `symphonia = { version = "0.5", default-features = false,
features = ["pcm"] }`, which feature-unifies with songbird's own copy and adds
one package — `symphonia-codec-pcm` 0.5.5, MPL-2.0, the same licence and the
same §3.3 reasoning as the symphonia crates that were already here.

**songbird's `receive` feature is enabled and adds no package.** It is on for
voice-packet TIMING only, in `DecodeMode::Pass`, so nothing is decrypted or
decoded; in songbird 0.6 the feature resolves to `dep:bytes` (already pulled in
by `driver`) plus `discortp`'s `demux` and `rtcp` features on a crate that was
already in the graph.

A default-features build links none of them; `ci/guard-discord-feature.sh`
fails the build if that stops being true. A released `astar-server` container
IS built with `--features discord`, so it ships tokio and songbird to
operators who never configure a channel — accepted deliberately, on the same
reasoning as `codec2-static`.

Verified on 2026-09-20 by resolving the full graph
(`cargo tree -p astar-server --features discord --edges normal --prefix none`,
**204 packages** more than a default build — 203 from crates.io plus the
workspace's own `astar-discord`) and reading each crate's `license` field out
of the registry cache: songbird 0.6.0 ISC, serenity 0.12.5 ISC, opus2 0.4.0
MIT/Apache-2.0, and exactly **seven** MPL-2.0 crates — symphonia,
symphonia-core, symphonia-codec-pcm, symphonia-metadata 0.5.5, hpke-rs,
hpke-rs-crypto, hpke-rs-rust-crypto 0.6.1. Everything else is permissive.
(The count was 203 and six before `symphonia-codec-pcm` was added; that crate
is the whole of the difference.) There is no LGPL relink obligation, and the
MPL-2.0 crates are AGPL-compatible under their §3.3.

## 5. LAME (LGPL) — `/stream.mp3` (feature `stream-mp3`, off by default)

astar-server's MP3 stream (`/stream.mp3`, the public listener page's
fallback and the address scanner apps use) is encoded by **LAME 3.100**,
vendored with its Rust bindings as a licence island beside `vendor/ambe-thumbdv`:

| Component | Where | Licence |
|---|---|---|
| LAME 3.100 (C) | `vendor/mp3lame-sys/lame-3.100/` | LGPL-2.0-or-later |
| `mp3lame-sys` 0.1.11 (bindings) | `vendor/mp3lame-sys/` | LGPL-3.0 |
| `mp3lame-encoder` 0.2.5 (safe wrapper) | `vendor/mp3lame-encoder/` | LGPL-3.0 |

Both crates' `Cargo.toml` files hard-code their licence; never convert them to
`.workspace = true`, which would relabel LGPL code as AGPL.

**Opt-in, never default.** Only astar-server's `stream-mp3` feature links
LAME. A plain `cargo build` links none, and `ci/guard-codec2-licensing.sh`
fails a default build that does. The single static release (`release`
feature) turns it on deliberately, and links LAME statically.

**Static linking and your right to relink.** As for Codec 2 in section 2:
astar publishes its complete, buildable source, with LAME's exactly as
vendored, every release is tagged, and anyone may modify LAME (or swap it
out) and rebuild astar-server from that source with `just server-release`.
Nothing is withheld. LAME's encoder is used unmodified; the build was changed
to compile it with the `cc` crate instead of autotools, and every such change
is listed under "Patches" in `vendor/mp3lame-sys/VENDORED.md` and
`vendor/mp3lame-encoder/VENDORED.md`, as LGPL section 2 requires of a
modified copy. LAME's `mpglib` decoder is never built.

MP3's patents have expired; LAME is not patent-encumbered.

* LAME: <https://lame.sourceforge.io/>
* `mp3lame-sys`: <https://github.com/DoumanAsh/mp3lame-sys>
* `mp3lame-encoder`: <https://github.com/DoumanAsh/mp3lame-encoder>

## 6. Opus via `mousiki` (MIT) — notices

The listener page's low-delay stream is Opus, encoded by **`mousiki`** 0.2, a
pure-Rust port of the Xiph Opus reference implementation, from crates.io. It
is in astar-server's default `stream` feature: MIT puts no relink or source
obligation on astar, only attribution, and there is no C library to link.
Opus is royalty-free. Because `mousiki` is a port of libopus, the libopus
notice is reproduced below its own.

```
mousiki — Copyright (c) nemurubaka <cijiugechu@gmail.com>

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

```
Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

`public-tls` (HTTPS, ACME, Porkbun) adds only permissive crates (rustls,
ring, rcgen, ureq: MIT, Apache-2.0, ISC) plus `webpki-roots`, whose root
certificate data is CDLA-Permissive-2.0; `ureq` and `rustls` were already in
the default graph through `astar-asl3`.

## 7. WebAuthn (MPL-2.0) and OpenSSL (Apache-2.0) — sign-in (feature `auth`, off by default)

astar-server's admin sign-in (passkeys, security keys, authenticator apps)
is the opt-in `auth` feature, which the single static release turns on.

**`webauthn-rs`, `webauthn-rs-core`, `webauthn-rs-proto` and
`webauthn-attestation-ca`** 0.5 (Kanidm project,
<https://github.com/kanidm/webauthn-rs>) are **MPL-2.0**. The MPL is a
file-level copyleft and is compatible with the AGPL (MPL-2.0 §3.3 and its
"Secondary License" terms): astar uses those files unmodified from
crates.io, and their source is available there and at the repository
above. Anyone who changes one of those files and distributes the result
must make that file's source available under the MPL. The full text is at
<https://mozilla.org/MPL/2.0/>.

**OpenSSL 3** is built from source and linked statically into an `auth`
build (the `openssl` crate's `vendored` feature, through `openssl-src`), so
the release needs no system library. OpenSSL 3 is **Apache-2.0**,
compatible with GPLv3 and therefore with the AGPL; its licence text is at
<https://www.openssl.org/source/license.html>, and builds that include it
carry this notice:

```
Copyright (c) 1998-2025 The OpenSSL Project Authors.
Licensed under the Apache License, Version 2.0.
```

The rest of `auth` is permissive: `totp-rs` (MIT), `qrcodegen` (MIT),
`sha2`, `getrandom`, `base64`, `uuid` (MIT/Apache-2.0). The tests use
`webauthn-authenticator-rs` (MPL-2.0) as a software authenticator; it is a
dev-dependency and in no shipped build.

## 8. GSM 06.10 via `oxideav-gsm` (MIT) — notices and patent reasoning

EchoLink's voice is GSM 06.10 full-rate, and so is IAX2's GSM format, which
some AllStar nodes still offer as their only codec. astar encodes and decodes
it with **`oxideav-gsm`** 0.0.10, a pure-Rust implementation from crates.io,
built on its support crate **`oxideav-core`** 0.1.41. Both are pulled in by
`astar-codec`'s `gsm` feature, which the IAX2 GSM format and EchoLink use.
They are MIT: no relink or source obligation on astar, only attribution, and
there is no C library to link. `oxideav-gsm` is `#![deny(unsafe_code)]` and
has no build script.

Both versions are pinned exactly (`=0.0.10`, `=0.1.41` in the root
`Cargo.toml`), and a bump is a fresh licence audit: `oxideav-gsm` 0.0.6–0.0.8
were published as `MIT AND LicenseRef-libgsm` and are yanked. 0.0.10's
packaged source was checked for any libgsm notice (Degener, Bormann, TU
Berlin) and carries none. Its bit-exactness is checked against the ETSI
EN 300 961 test sequences by `crates/astar-codec/tests/gsm_etsi.rs`; the
vectors themselves are ETSI's and are not in this repository.

Both crates carry the same notice:

```
MIT License

Copyright (c) 2026 Karpelès Lab Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

**Patents.** GSM 06.10 dates from 1990–92 (prI-ETS 300 036), so any patent on
the standard itself has expired. The one later patent sometimes cited against
GSM full-rate implementations, US 5,943,646 (Philips, filed 1997), lapsed for
non-payment of maintenance fees on 2007-08-24, and would have expired in 2017
regardless.

## 9. The software D-Star vocoder (LGPL) — notices and relinking (feature `dstar-soft-ambe`, in every shipped build)

D-Star's voice codec is DVSI's AMBE 3600×2400. astar can run it on a ThumbDV or
DVstick dongle, or in software with **Bruce Perens' implementation**: the AMBE
tree of his `ham_digital_modes` crate, from
<https://github.com/BrucePerens/hams_open> (`daemons/ham_digital_modes/`),
vendored at `vendor/ham-digital-modes-ambe` at a pinned commit. Which one D-Star
uses is the operator's choice in config (Settings → Vocoders in the app,
`[vocoder] dstar` in `node.toml`); the dongle is the default choice.

| Component | Where | Licence |
|---|---|---|
| `ham-digital-modes-ambe` (Bruce Perens) | `vendor/ham-digital-modes-ambe` | `LGPL-3.0-or-later` |

**Built into every shipped build** (Rob, 2026-11-01). It is reached through
the `dstar-soft-ambe` Cargo feature (of `astar-codec`, re-exported by
`astar-console`, `astar-station`, `astar-sys`, `astar-server` and
`astar-netrec`), and:

* **the macOS app** links it statically: `bindings/swift/build-xcframework.sh`
  turns the feature on unless `ASTAR_DSTAR_SOFT_AMBE=0`, so the signed DMG
  carries it;
* **astar-server's single static release** (`just server-release`, the
  `release` feature) links it statically too;
* **astar-netrec's release build** (`cargo build --release -p astar-netrec
  --features release`) links it statically as well.

A plain `cargo build` of any astar crate does not: like `codec2-static`, the
feature is named on purpose by the builds that ship, never a cargo default
(`ci/guard-dstar-soft-ambe.sh` checks both halves). LGPL-3.0 code may be
combined into an AGPL-3.0 program: LGPL-3.0 is GPL-3.0 with extra
permissions, and AGPL-3.0 section 13 allows combination with GPL-3.0 works.
The LGPL-3.0 and GPL-3.0 texts ship beside the code.

### You are free to change it and rebuild astar

As for Codec 2 in section 2, and stated as plainly: **nothing here restricts
anyone from modifying this vocoder, or astar, and building their own astar from
the result.** Patch it, tune it, replace it, or remove it, and rebuild.

### Static linking and your right to relink

Where a released astar binary links this vocoder statically — the macOS app,
astar-server's static release and astar-netrec's release build — LGPL-3.0 section 4 requires that a recipient
can modify the library and relink it into the application. astar satisfies this
the same way it does for Codec 2 and LAME, by publishing its **complete,
buildable source**:

* Source: <https://github.com/rcludwick/astar>. The vocoder's own source ships
  in that repository, at `vendor/ham-digital-modes-ambe`, with its licence
  texts beside it — not merely as a reference to upstream.
* Every release is tagged, so the exact source for a given binary, the library
  included, is `git checkout v<version>`.
* To relink: edit `vendor/ham-digital-modes-ambe` (or point
  `ham-digital-modes-ambe` in the root `Cargo.toml` at your own copy), then
  rebuild with `just xcframework && just dmg` for the app,
  `just server-release` for the server, or
  `cargo build --release -p astar-netrec --features release` for the recorder.
  Build instructions:
  <https://rcludwick.github.io/astar>.

**Where the source is.** The Corresponding Source of every release that
carries this vocoder, the library included, is published at the repository
above, at that release's tag, from the same place the binaries are offered —
the arrangement GPL-3.0 section 6(d) describes, and the one section 2 relies on
for Codec 2. No part of astar is withheld.

Section 2's 2026-11-21 App Store decision covers this vocoder too. Its author,
Bruce Perens, accepted app-store distribution of LGPL code, provided the
source is served and the LGPL part can be re-linked. In astar's store builds,
then, this vocoder ships as its own replaceable library, on the terms listed
there. Section 1's exception does not reach this code, because it is not
Rob's copyright.

**Modifications.** The vendored copy is upstream's `src/ambe/` unchanged
except that the AMBE+2 directories are removed, with a new `lib.rs` and
`Cargo.toml`. `vendor/ham-digital-modes-ambe/VENDORED.md` lists every
difference and is the dated modification notice LGPL-3.0 asks for.

**Notices it carries.** Upstream took its D-Star quantiser tables and bit
layout from mbelib (ISC, "Copyright (C) 2010 mbelib Author") and checked its
interleave against DSD (ISC, "Copyright (C) 2010 DSD Author"). Both ISC
notices are reproduced in full in `VENDORED.md`. It does not use, refer to or
derive from `rcludwick/ambe`.

**Patents: why this is D-Star only.**

* **AMBE 3600×2400 (D-Star).** This generation of DVSI's codec dates from the
  1990s, before the D-Star standard (2001) adopted it, and the patents on it
  have run their 20-year term. That is why astar allows a software implementation for D-Star at all
  (Rob, 2026-09-19).
* **AMBE+2 (DMR, NXDN, System Fusion).** The half-rate AMBE+2 vocoder still
  reads on **US 8,359,197 B2** ("Half-rate vocoder", DVSI, priority
  2003-04-01), in force until **2028-05-20**. astar has no software backend for
  these modes and must not gain one before then. Upstream's crate does contain
  an AMBE+2 implementation, behind its own off-by-default feature; astar does
  not vendor it. The two `ambe_plus_2` directories are deleted from the copy,
  the feature is not declared, the engine refuses a software backend for every
  mode but D-Star (`software_backend_permitted` in
  `crates/astar-codec/src/ambe.rs`, pinned by tests), and the guard script
  fails if AMBE+2 source or the feature reappears.

This is a summary of the public record, not legal advice.

## 10. SSTV decoding via `slowrx` (MIT) — notices

astar decodes slow-scan TV pictures with **`slowrx`** 0.9.0, a pure-Rust port
by Jason Herald of Oona Räisänen's slowrx decoder, from crates.io. It is one
of the two SSTV dependencies of `crates/astar-sstv`; the other is the vendored
`sstv` fork (section 14), whose de-jitter re-times a received picture before
slowrx decodes it. Every build that decodes pictures carries
`astar-sstv`: today the macOS app's engine, where `astar-sys` turns on
`astar-station`'s `sstv` feature. `astar-server` does not decode SSTV and
leaves that feature off, so it does not link slowrx. It is MIT: no relink or
source obligation on astar, only attribution, and there is no C library to
link. Its port translates much of
slowrx's C source, which is ISC-licensed, so slowrx's ISC notice is
reproduced below its own, as the crate's `NOTICE.md` does.

The version is pinned exactly (`=0.9.0` in the root `Cargo.toml`), and a bump
is a fresh read of its licence, its `NOTICE.md` and its dependencies. The
crate declares `unsafe_code = "deny"` and has no build script. Its one
non-trivial dependency, `rustfft` 6 (with `num-complex`, `num-traits`,
`num-integer`, `primal-check`, `strength_reduce` and `transpose`), is
`MIT OR Apache-2.0`; so is `png` 0.18, which writes the saved pictures.
Png's own dependencies are permissive too: `flate2`, `fdeflate`,
`miniz_oxide` and `crc32fast` are MIT or Apache-2.0 (`miniz_oxide` also
Zlib), `simd-adler32` is MIT, and `adler2` is 0BSD, MIT or Apache-2.0.

astar only receives SSTV in every build that ships today. The **`sstv`**
crate (MIT), which can encode pictures, is vendored as a patched fork at
`vendor/sstv` (section 14). It makes the test signals, and its encoder is
also what `astar-station`'s off-by-default `sstv-tx` feature sends pictures
with; no shipped build turns that feature on. `ci/guard-sstv.sh` fails if
the published crate is named anywhere, if anything but `sstv-tx` and
dev-dependencies asks for the encoder, or if a shipped build's dependency
tree builds the crate with its encoder (its `encode` or `default` feature).
The crate itself is in those trees, for the de-jitter; the encoder is not.

`slowrx` (from the crate's `LICENSE`):

```
MIT License

Copyright (c) 2026 Jason Herald

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

slowrx, the C original (from the crate's `NOTICE.md`):

```
Copyright (c) 2007-2013, Oona Räisänen (OH2EIQ [at] sral.fi)

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

## 11. Inter (OFL-1.1) — the net recorder's video text

`astar-netvideo` draws the text on recorded-net video frames in **Inter 4.1**,
four static weights (Regular, SemiBold, Bold, ExtraBold) compiled into the
binary with `include_bytes!`. They live in `assets/fonts/`, unmodified; the
source release, the path of each file inside it and each file's SHA-256 are in
`assets/fonts/VENDORED.md`.

Inter is licensed under the **SIL Open Font License 1.1**, which allows
bundling, embedding and redistributing the font, in a program of any licence,
provided the copyright notice and licence travel with it and the font is not
sold on its own. The notice, `Copyright (c) 2016 The Inter Project Authors
(https://github.com/rsms/inter)`, and the full licence text are in
[`assets/fonts/OFL.txt`](assets/fonts/OFL.txt), which is Inter's own
`LICENSE.txt` byte for byte, and which is what this section incorporates by
reference. The font files are not modified, so the licence's Reserved Font
Name clause is not in play. The licence applies to the font only, not to
astar's AGPL code that embeds it.

## 12. tiny-skia (BSD-3-Clause), ab_glyph (Apache-2.0), rml_rtmp (MIT) — the net recorder's video

`astar-netvideo` draws frames with **`tiny-skia` 0.11.4**, an ordinary
crates.io dependency, a Rust port of a subset of Skia. BSD-3-Clause is
AGPL-compatible. It asks that binary distributions reproduce the copyright
notices, the conditions and the disclaimer, and that the names of the
copyright holders not be used to endorse a product. This section is that
reproduction:

```
Copyright (c) 2011 Google Inc. All rights reserved.
Copyright (c) 2020 Yevhenii Reizner All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in
    the documentation and/or other materials provided with the
    distribution.

  * Neither the name of the copyright holder nor the names of its
    contributors may be used to endorse or promote products derived
    from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

`tiny-skia` depends on **`arrayref` 0.3.9**, which is **BSD-2-Clause**, also
AGPL-compatible. Its binary-distribution condition is met by reproducing its
notice here:

```
Copyright (c) 2015 David Roundy <roundyd@physics.oregonstate.edu>
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the
   distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

**`ab_glyph` 0.2.32** and **`ab_glyph_rasterizer` 0.1.10** (by Alex Butler), the
glyph outline reader and rasteriser behind the Inter text, are **Apache-2.0**.
That licence is compatible with the AGPL-3.0, and its text ships in each
crate's `LICENSE` file in its crates.io package. Neither carries a NOTICE file.

**`rml_rtmp` 0.8.0** and **`rml_amf0` 0.3.0** (by Matthew Shapiro), the RTMP and
AMF0 codecs of the net recorder's live-stream output, are **MIT**. Their
crates.io packages carry no licence file, only the `license = "MIT"` field in
`Cargo.toml` and the author name, so this is the standard MIT text with that
author:

```
MIT License

Copyright (c) Matthew Shapiro <me@mshapiro.net>

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

The live stream's RTMPS leg uses **`rustls` 0.23** with **`ring`** and the
**`webpki-roots`** certificate list, the same permissive set (MIT, Apache-2.0,
ISC; the root data CDLA-Permissive-2.0) that section 6 records for
astar-server's `public-tls`. `rml_rtmp`'s own dependencies (`bytes`,
`byteorder`, `hmac`, `sha2`, `rand`, `thiserror`) are MIT or MIT/Apache-2.0.

## 13. `subtle` (BSD-3-Clause) — constant-time comparison, in every build

**`subtle` 2.6.1** (by Isis Agora Lovecruft and Henry de Valence; used by
much of the RustCrypto family) gives constant-time equality. `astar-iax-core`
uses it directly to compare IAX2 MD5 authentication answers and call tokens
(`crates/astar-iax-core/src/session/auth.rs`), so a wrong one leaks no
timing about which byte was wrong. Because `astar-iax-core` sits under every
client and server, `subtle` is compiled into **every shipped binary**: the
macOS app, `astar-server`, `astar-netrec` and the Windows/Linux client. It
also arrives through `digest`, `rustls`, `curve25519-dalek`, `crypto-mac` and
`universal-hash` where those are built in; it is the same crate and the same
notice either way.

BSD-3-Clause is AGPL-compatible. Its binary-distribution condition is met by
reproducing the notice here:

```
Copyright (c) 2016-2017 Isis Agora Lovecruft, Henry de Valence. All rights reserved.
Copyright (c) 2016-2024 Isis Agora Lovecruft. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

1. Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED
TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

---

## 14. The `sstv` crate, vendored (MIT) — notices

`vendor/sstv` is a fork of the **`sstv`** crate 0.3.1 by Tobias Klockau
(<https://github.com/unexcellent/sstv>, upstream commit `9761c4c`), an SSTV
encoder and decoder. It is **MIT**: no relink or source obligation on astar,
only that the copyright and permission notice travel with the code, which
`vendor/sstv/LICENSE` does and the text below repeats. The fork's own changes
are MIT too, offered on the same terms.

It is a licence island inside the AGPL-3.0-only tree. Its `Cargo.toml`
hard-codes `license`, `version`, `edition` and `repository` instead of
inheriting them from the workspace, which would relabel Klockau's code as
AGPL. It is in the workspace `exclude`, so `cargo fmt --all` and clippy never
touch it. `vendor/sstv/VENDORED.md` lists every patch against upstream.

It is the test-signal encoder for `astar-sstv`, `astar-netrec` and
`astar-nodebench` (dev-dependencies, with the `encode` feature on), and the
encoder behind `astar-station`'s `sstv-tx` feature, which is off by default,
which only the bench turns on, and which no shipped build includes
(`ci/guard-sstv.sh`). The fork's `encode` feature can be turned off to leave
the encoder out; the workspace entry does so, so a crate must ask for it by
name. `astar-sstv` uses the fork's de-jitter (feature `dejitter`, encoder
off), which re-times a received picture before `slowrx` (section 10) decodes
it: Rob Ludwick's own code, written for astar and moved into the fork under
its MIT licence so it can be offered upstream. The `sstv` crate's own decoder
is compiled into the builds that carry the de-jitter, since only `encode` is
behind a feature, but no shipped build calls it; only the bench measures it.
It has one runtime dependency, `libm` (MIT OR Apache-2.0), and optional ones astar does
not turn on (`image`, `hound`, `minimp3`, `mp3lame-encoder`).

`sstv` (from the crate's `LICENSE`):

```
MIT License

Copyright (c) 2026 Tobias Klockau

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## 15. The P25 IMBE vocoder (GPL-3.0-or-later) — feature `p25-soft-imbe`, in the Apple apps (macOS and iPhone)

P25 Phase 1 is the digital voice mode US public-safety radios use, and the one
P25 reflectors carry. Its voice codec is DVSI's full-rate **IMBE**: every 20 ms
of speech becomes one 88-bit frame of model parameters (pitch, which frequency
bands are voiced, and the spectral amplitudes), and the decoder rebuilds speech
from those parameters.

astar decodes IMBE in software with **Pavel Yazev's fixed-point
`imbe_vocoder`**, the author's own implementation of the TIA-102.BABA standard.
It comes from OP25 (<https://github.com/boatbod/op25>, directory
`op25/gr-op25_repeater/lib/imbe_vocoder/`, pinned at commit
`38a37491df4532d932f9f1d5634617af410cb3c9`), and astar carries it as a Rust
port, held bit for bit to the C++ original by committed golden vectors. The
port is a derivative work, so it keeps Yazev's licence.

| Component | Where | Licence |
|---|---|---|
| `imbe-vocoder` (Pavel Yazev; Rust port by Rob Ludwick) | `vendor/imbe-vocoder` | `GPL-3.0-or-later` |

There is no dongle path for P25. DVSI documents "APCO Project 25 Full Rate:
NO" for every AMBE-3000 and AMBE-3003 part, which covers the ThumbDV and every
DVstick astar drives; full rate needs DVSI's separate "-P25" models. So Rob
decided on 2026-11-21 that P25 uses this software vocoder only.

**Where it is built in.** It is reached only through the `p25-soft-imbe` Cargo
feature (of `astar-codec`, re-exported by `astar-console`, `astar-station` and
`astar-sys`) and through the network feature `p25`, which requires it at every
level:

* **the Apple apps, macOS and iPhone,** link it statically:
  `bindings/swift/build-xcframework.sh` turns `p25` on unless
  `ASTAR_P25_SOFT_IMBE=0`, and the iPhone App Store build carries it too (see
  "App stores" below);
* **a plain `cargo build`** of any astar crate does not, because neither
  feature is a cargo default anywhere;
* **astar-server and astar-netrec** do not carry it in any build, their
  `release` sets included.

`ci/guard-p25-imbe.sh` checks all three, along with the rules below.

**Why GPL code can sit in an AGPL program.** GPL-3.0 section 13 lets a GPL-3.0
work be combined with an AGPL-3.0 work, and AGPL-3.0 section 13 allows the
reverse. Each part keeps its own licence: the island stays GPL-3.0-or-later,
and astar stays AGPL-3.0-only. Its `Cargo.toml` hard-codes its licence rather
than inheriting the workspace's, and the GPL-3.0 text ships beside it as
`vendor/imbe-vocoder/COPYING`. OP25 does not ship the COPYING file its
headers refer to, so astar adds it.

### The complete source ships with it

The GPL asks that whoever receives a binary can get its **Corresponding
Source**: the complete source needed to rebuild it, the library included. astar
meets that by serving the source the way section 9 does for the D-Star vocoder.
Only section 9's source-serving applies here; its App Store relinking
conditions are for LGPL code, and this island's App Store position is under
"App stores" below:

* Source: <https://github.com/rcludwick/astar>. The vocoder's own source is in
  that repository, at `vendor/imbe-vocoder`, with `COPYING` and a
  `VENDORED.md` that records the upstream commit and every porting decision.
* Every release is tagged, so the exact source for a given binary is
  `git checkout v<version>`.
* To change it and rebuild: edit `vendor/imbe-vocoder` (or point
  `imbe-vocoder` in the root `Cargo.toml` at your own copy), then run
  `just xcframework && just dmg`. `just imbe-check` runs the port's tests,
  including the golden vectors. Build instructions:
  <https://rcludwick.github.io/astar>.

No upstream C++ is committed. `vendor/imbe-vocoder/tools/make-golden.sh`
fetches OP25 at the pinned commit into a temporary directory to record the
golden vectors, and only those vector files are kept.

### Patents

The patents on IMBE belong to Digital Voice Systems (DVSI). These are the ones
found, from Google Patents, with their status on 2026-11-21:

| Patent | What it covers | Priority | Expired |
|---|---|---|---|
| 5,226,084 | Speech quantisation and error correction: **core IMBE** | 1990-12-05 | 2010-12-05 |
| 5,247,579 | Methods for speech transmission: **core** | 1990-12-05 | 2010-12-05 |
| 5,491,772 | Same family: **core** | 1990-12-05 | 2013-02-13 |
| 5,630,011 | Quantisation of harmonic amplitudes: **core** | 1990-12-05 | 2014-05-13 |
| 5,517,511 / 5,870,405 | Transmission over a noisy channel (FEC, bit priority) | 1992-11-30 | 2013 / 2012 |
| 5,081,681 / 5,664,051 | Phase synthesis | 1989 / 1990 | 2012 / 2014 |
| 5,195,166, 5,216,747, 5,226,108 | Voiced synthesis, V/UV and pitch estimation | 1990-09-20 | 2010 |
| 5,715,365, 5,826,222 | Excitation parameter estimation | 1994–95 | 2015 |
| 5,701,390, 5,754,974 | Regenerated phase, spectral magnitudes | 1995 | 2015 |
| 6,131,084, 6,161,089, 6,199,037 | AMBE quantisation | 1997 | 2017 |
| 7,970,606 / 8,315,860 | Interoperable (enhanced full-rate) vocoder | 2002-11-13 | 2025 / 2022 |
| **8,359,197** | **Half-rate vocoder (AMBE+2)** | 2003-04-01 | **Active until 2028-05-20** |
| 8,036,886 | Pulsed-speech parameter estimation | 2006 | Active until 2029-10-02 |
| 8,265,937, 11,990,144, 12,254,895, 12,451,151 | SCBA mask enhancement, data in voice frames, speaker-mask compensation, tone frames | 2008–2022 | Active |

What that table means for astar:

* **US 8,359,197 is AMBE+2 only.** It is the half-rate patent. Its own
  background describes the "standard 'full-rate' 7200 bps APCO Project 25
  vocoder … TIA-102BABA" as the prior art it improves on, and full-rate IMBE
  dates from the early 1990s, a decade before its 2003 priority date. It is
  why P25 Phase 2, DMR, NXDN and System Fusion, which are all AMBE+2, stay
  hardware-only, and why the island contains no AMBE+2 code.
* **The other active patents are add-ons.** They cover features a baseline
  TIA-102.BABA codec does not implement: a later encoder-analysis technique
  (8,036,886), which the 2009 Yazev encoder predates, mask enhancement,
  data-in-voice and tone frames. The island implements none of them.
* **The standard's own patent list could not be checked.** TIA-102.BABA is
  paywalled. The nearest primary source, TIA/EIA/IS-69.5 (EDACS IMBE, 1998),
  says DVSI claims patent rights in IMBE but lists no numbers, so the numbers
  above come from Google Patents.

**Conclusion:** no unexpired patent was found that plausibly reads on a
baseline full-rate IMBE encoder or decoder. This is engineering research, not
a legal opinion.

### Why not `blip25-mbe`

`blip25-mbe` is an MIT-licensed Rust crate with both IMBE and AMBE+2, so it
looks like the easier choice. astar refuses it because of where its code came
from. Its own `PATENT_NOTICE.md` says it is "provided for research and
educational purposes only", and that its analysis and synthesis core "was
recovered by reverse engineering a compiled reference vocoder image — x86
disassembly transliterated to fixed-point Rust, constants recovered from the
binary's data section", which "applies to both codecs' audio output, including
full-rate IMBE". Code transliterated from DVSI's binary is DVSI's code in
another language, and an MIT licence on it cannot change that.

### No encryption

P25 radios can encrypt a call, and a P25 voice superframe says which algorithm
it uses (its ALGID). astar never decrypts. P25 in astar contains no cipher and
no key handling, and neither the island nor `crates/astar-p25` links any
cryptography library, so P25 adds nothing to the app's export-compliance
declaration to Apple. An encrypted call is muted from its ALGID: the talker is
still shown, but none of its audio is decoded, played or recorded. The guard
checks the dependency graph for crypto crates and the P25 code for decryption.

### App stores

Section 1's App Store permission is Rob's to give only for his own code. It
cannot cover this island, whose copyright belongs to Pavel Yazev and OP25's
contributors.

The Free Software Foundation holds that the GPL's terms and Apple's App Store
terms are incompatible: the App Store's usage rules are the kind of "further
restrictions" GPL-3.0 section 10 forbids. VLC was removed from the App Store
in 2011, after one of its contributors objected. That was a GPLv2 case, under
GPLv2 section 6, which forbids the same further restrictions; GPL-3.0 section
10 carries the same conflict. Nobody from OP25
has given astar permission to distribute this code through an app store.

Rob decided on 2026-11-21 to ship P25 in the iPhone App Store build anyway,
with the full source public, and accepts the risk: a copyright holder may
object, and Apple may then remove the app. If that happens, P25 is pulled from
the iPhone build, or this port is replaced there by a clean-room IMBE
implementation (backlog `p25-clean-imbe`).

None of this affects the signed Mac DMG, which is distributed directly from
the project's own releases page with the published source beside it, or the
Android build.
