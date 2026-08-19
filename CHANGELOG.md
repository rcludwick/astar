# Changelog

Notable changes to astar. Newest first.

Since `0.1.1beta` there is a signed, notarized macOS `astar.dmg` on the
[releases page](https://github.com/rcludwick/astar/releases/latest); everything
else is still built from source. Versions are `MAJOR.MINOR.PATCHbeta` and will
stay on `beta` until the client has had a real sit-down-and-use-it pass on all
three platforms.

## 0.1.4beta — 2026-08-18

M17 now works on a Mac that has never seen Homebrew. That was the whole point
of this release: `0.1.3beta` offered an M17 network picker that could not
actually open a codec, because the Codec 2 library it needed was something you
had to install yourself.

### Added

- **Codec 2 is linked into the shipped app**, so M17 works out of the box.
  astar still prefers a system `libcodec2` if you have one and only falls back
  to the linked copy, so nothing changes for anyone who installed it via
  Homebrew. Verified as a controlled experiment on a Mac with Homebrew's
  `codec2` uninstalled: the runtime-only build reports M17 unavailable, the
  shipped build reports it available.

  This is the only LGPL code in astar, it is unmodified, and it is deliberately
  never part of a default build — see `LICENSE-EXCEPTIONS.md` for the Codec 2
  notices and the written offer, and `ci/guard-codec2-licensing.sh`, which
  fails the build if it ever leaks into a default feature set.

- **An App Store distribution exception** under AGPL-3.0 §7, scoped to Rob
  Ludwick's own copyright and removable exactly as §7 allows. astar stays
  AGPL-3.0-only; the exception exists so the same source can eventually ship
  through the App Store, whose terms conflict with the bare AGPL. It grants no
  rights over Codec 2, which is not Rob's to relicense — and nothing here stops
  you modifying Codec 2 and rebuilding astar, which the notices say in as many
  words.

- **A first-run walkthrough** in the docs — install, account, audio levels, the
  four ways to key up, first contact. Written from the questions an outside
  tester actually asked over a week rather than from what seemed obvious from
  the inside.

### Fixed

- The CodeQL workflow could hang for 25 minutes on its own `apt-get` step
  against an unreachable Ubuntu mirror. The step is now bounded and retried,
  and reports what happened instead of stalling the run.

## 0.1.3beta — 2026-08-17

Everything here is about the first ten minutes with astar. `0.1.2beta` fixed
Settings being unreachable; this fixes Settings being *findable but silent* —
you could open it and still not learn that an AllStarLink account is the thing
standing between you and a call.

### Added

- **Settings opens by default when no AllStarLink account is configured.**
  Without one the dial field is disabled, so the call UI is a form that cannot
  be typed into. astar now lands on Settings instead, from every route into the
  window — the menu-bar asterisk, the Dock icon, and `Cmd-,`. It stops the
  moment an account is saved.

- **A first launch with no account raises the window on Settings** — once.
  astar is a menu-bar app, so a new user otherwise sees an asterisk and nothing
  else. It happens a single time: M17 needs no portal login, so running astar
  without an AllStarLink account is perfectly legitimate and is not nagged at.

- **The account password is outlined in red** when it is empty with nothing
  saved, or when the portal rejected the last token test, with the reason
  underneath.

  An empty box on an account that *is* saved stays unmarked. astar never
  pre-fills the password — it lives in the Keychain and the field reads
  "re-enter to change" — so flagging that would tell you your working
  credentials are broken.

- **Settings now says what a missing account costs you:** AllStarLink is
  unavailable without one, because dialling a node signs in to your
  allstarlink.org account and there is no guest access. M17 is unaffected.

### Fixed

- CodeQL had never completed a single run. cpal's Linux backend builds against
  ALSA and the hosted runner ships no ALSA headers, so the build died before
  analysis started. Invisible from a Mac, where cpal uses CoreAudio.

## 0.1.2beta — 2026-08-17

Both fixes here came out of the first outside report against the `0.1.1beta`
DMG, from a tester who had never built astar from source — so he met the app
exactly as a new user does, and hit two walls in a row.

### Fixed

- **Settings opened an empty window.** astar spent its life as an `LSUIElement`
  accessory, which has no application menu. The Dock icon added in `0.1.1beta`
  promotes the app to a regular one, and that handed it a menu bar whose
  `Settings…` item was still wired to the placeholder empty scene the app used
  to satisfy SwiftUI's "an App must have a Scene" requirement. Choosing it
  opened a window with nothing in it. astar now builds its menu explicitly, and
  `Cmd-,` opens the real settings pane — the same one the popover's own settings
  button shows, not a second copy.

- **A fresh install invented a config for hardware you may not own.** With no
  saved configs, astar seeded one named after the AllScan UCI150 and put it on a
  serial hardware profile, whether or not that interface had ever been plugged
  in. Meanwhile the entry that described plain system audio was filtered out of
  the settings list and never appeared at all.

### Added

- **A built-in `System Default` config**: your Mac's current input and output,
  no serial PTT. It is always present, can't be deleted, sits at the top of
  Saved configs, and is what a fresh install starts on and stars as its launch
  default.

  Existing setups are left alone. If you already have saved configs but never
  set a launch default, astar still applies nothing at startup — making System
  Default win there would reset your devices and switch off your serial PTT on
  every launch.

- **A real menu bar.** `About astar` with links to the documentation and to
  AJ7HR on QRZ; `Settings…` on `Cmd-,`; a standard Edit menu, so `Cmd-C` /
  `Cmd-V` / `Cmd-A` work in the account, node, and config fields; Window; and a
  Help menu linking the documentation site, the issue tracker, and QRZ.

- **`Report an Issue…` in the Help menu**, pointing at the public repository's
  issue tracker. Nothing in the app used to say where a bug should go.

## 0.1.1beta — 2026-08-14

### Added

- **A Dock icon for the macOS app**, on by default, with a `Show in Dock` toggle
  in the status item's right-click menu. astar has had a real main window for a
  while; it now has the Dock presence and Cmd-Tab entry a windowed app is
  expected to have. Turning it off returns it to menu-bar-only, and that choice
  sticks across launches.

  The app still launches as an `LSUIElement` accessory and promotes itself
  afterwards, so anyone who turns the icon off never sees it flash on at startup.

- **Clicking the Dock icon opens the astar window.** It shows the window, it
  never hides it — a second click on a Dock icon is not a close button.

- **M17 in every default client build.** The macOS and Iced clients ship the M17
  network without a feature flag.

### Fixed

- The `Show in Dock` toggle applied only after a restart. Under
  `@NSApplicationDelegateAdaptor`, `NSApp.delegate` is SwiftUI's own wrapper
  delegate rather than the app's, so the status item's `as? AppDelegate` cast
  silently produced nil and the apply step never ran. The activation-policy
  change now lives on a stateless type both callers own outright.

### Changed

- CI moved to a private development repository with a self-hosted runner; the
  public repository is the release target and carries no runner. Both workflow
  files exist in both repositories and are guarded on `github.repository`, so
  each is inert in the wrong one.

- Documentation is published to GitHub Pages on merge to `main` in the public
  repository.

### Documentation

- Stopped claiming the macOS app has "no Dock icon and no main window". The main
  window has existed for some time; the Dock icon now exists too. Corrected in
  the README, the site front page, the macOS page, and the build guide.

- Brought the site in line with the README on D-Star, and stopped
  `.github/README.md` shadowing the front page.

## 0.1.0beta — 2026-08-11

First tagged version, and the point at which astar became one repository: the
client and the `iaxclient-rs` engine were merged into a single AGPL-3.0-only
cargo workspace with one git history.

What that version contains:

- **astar-lib** — the engine. IAX2, M17, and D-Star protocol work, codecs,
  audio, PTT, the station facade, and a C ABI.
- **The macOS client** — a SwiftUI menu-bar app over that engine.
- **The Iced client** — the Windows and Linux front-end, sharing the same core.
- **astar-server** — the headless node daemon.

No binaries were published, then or since. `just dmg` produces a local, unsigned
disk image; building from source is the only install path.
