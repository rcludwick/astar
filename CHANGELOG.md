# Changelog

Notable changes to astar. Newest first.

astar has never had a binary release: every version here is something you build
from source. Versions are `MAJOR.MINOR.PATCHbeta` and will stay on `beta` until
the client has had a real sit-down-and-use-it pass on all three platforms.

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
