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
| `codec2-static` | the `codec2` Rust crate (a reimplementation), linked into the binary | `LGPL-2.1-only AND MIT` |
| `codec2-runtime` | the system `libcodec2` C library, `dlopen`ed at runtime, never linked | LGPL-2.1 |

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
* Every release is tagged, so the exact source for a given binary is
  `git checkout v<version>`.
* Build instructions: <https://rcludwick.github.io/astar>

Anyone may replace the Codec 2 implementation with their own modified version
and rebuild astar from that source. No part of astar is withheld.

### Modifications

astar ships Codec 2 **unmodified**, as published upstream. Should that ever
change, the modified source will be published in this repository alongside the
release that carries it, and identified as changed, as LGPL-2.1 section 2
requires.

### Upstream

* `codec2` Rust crate: <https://crates.io/crates/codec2>
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
