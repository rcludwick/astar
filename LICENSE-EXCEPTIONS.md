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

### App-store distribution is a separate, unresolved question

LGPL's relink requirement is difficult to satisfy on a platform where a
recipient cannot rebuild and install their own binary. The exception in section 1
does not reach it, because Codec 2 is not Rob's copyright to grant permissions
over. astar's plan for app-store-distributable M17 is a cleanly-licensed Codec 2
implementation — tracked as `iax-e5d9` in `docs/BACKLOG.md`.

This does **not** affect astar's current distribution: the macOS release is
distributed directly, signed and notarized, from the project's own releases
page, where the published source fully satisfies the relink requirement.

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

The App Store question in section 2 applies here too, and section 1's
exception does not reach this code, because it is not Rob's copyright.

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
