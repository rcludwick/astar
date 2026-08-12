# astar brand

**The mark is a six-spoke asterisk washed in a rainbow gradient.** One mark,
everywhere: app icon, menu-bar item, Windows/Linux tray, in-app header, README.
There is no second logo, no wordmark variant, no alternate icon set. If you
need the mark somewhere new, render it from the masters below — do not draw a
new one and do not trace over a PNG.

## Where the masters live

The vector masters are the source of truth and are diffable in git. They
currently sit next to the macOS app because that is where the render script
lives:

| Master | Shape | Feeds |
|---|---|---|
| `apps/macos/art/icon.svg` | full-bleed square badge, dark slate, corner radius 224 | iOS AppIcon (the system masks the corners), the Iced client's window icon, the README logo |
| `apps/macos/art/icon-macos.svg` | rounded squircle with margin for the system drop shadow | macOS AppIcon |
| `apps/macos/art/menubar-rainbow.svg` | the bare asterisk, transparent background | macOS menu-bar idle item, the in-app header mark, all Iced tray states |

Everything else is a **rendered output**, not an asset to hand-edit:

```
apps/macos/Resources/Assets.xcassets/AppIcon.appiconset/     macOS + iOS icon slices
apps/macos/Resources/Assets.xcassets/MenuBarRainbow.imageset/ menu-bar idle image
apps/macos/Resources/Assets.xcassets/BrandAsterisk.imageset/  34pt in-app header mark
apps/gui/assets/icon/                                         Iced window + tray PNGs
```

Regenerate all of them from the masters with:

```bash
just icons          # apps/macos/Tools/render-icons.sh; needs rsvg-convert
```

> `rsvg-convert` comes from librsvg. Edit a master, run `just icons`, and
> commit the masters together with the re-rendered outputs so the two never
> drift apart.

## The mark

Three rounded capsules rotated 0°, 60° and 120° about the centre, giving six
spokes with **one point straight up**. On a 1024 canvas centred at (512, 512):

| | Spoke radius | Bar width | Corner radius |
|---|---|---|---|
| App icon (`icon.svg`, `icon-macos.svg`) | 400 | 150 | 75 |
| Small sizes (`menubar-rainbow.svg`) | 470 | 200 | 100 |

The small-size variant is deliberately larger and heavier — it nearly fills its
box — so the mark stays legible at the 18–20 pt menu-bar and tray display size.
That is the only sanctioned deviation in geometry.

Two rules that exist because breaking them caused real bugs:

* **The gradient is one wash across the whole mark**, produced by masking a
  single gradient-filled rect. Filling each capsule individually would rotate
  the gradient with each spoke and the mark would read as three separate
  objects.
* **Every status state shares one geometry.** The tray and menu-bar states
  (idle, connected, RX, TX) tint the same asterisk shape; they never swap in a
  different silhouette or a different size. An icon that changes size when the
  radio keys is visibly wrong in a menu bar.

## Colour

The badge background is **dark slate `#1E293B`**.

The gradient is a full-spectrum rainbow with its axis tilted **22.5° above
horizontal**, fitted to the asterisk's visible extent so red lands on one end
of the mark and violet on the other. Two tunings, same sequence and direction:

| Stop | App icon | Small sizes |
|---|---|---|
| 0.00 | `#FF2D2D` | `#FF1A33` |
| 0.17 | `#FF8A00` | `#FF8000` |
| 0.34 | `#FFD500` | `#FFE000` |
| 0.51 | `#2ECC40` | `#14E04A` |
| 0.68 | `#1E90FF` | `#00B4FF` |
| 0.84 | `#4B3BFF` | `#5B4BFF` |
| 1.00 | `#9B30FF` | `#C03BFF` |

The small-size tuning is more saturated because a 20 px mark loses chroma. Do
not use it at large sizes and do not use the icon tuning in a tray.

A solid-white fill of the same geometry is the template variant, used where the
host tints the icon itself (macOS template images, the Iced tray's per-state
tinting). `render-icons.sh` derives it from the master by substituting the
gradient reference, so it can never drift from the shape.

## Name

**astar**, lower case, always — in prose, in the menu bar, in the window title,
in the docs. Not "Astar", not "AStar", not "A*". The engine crates are prefixed
`astar-`; the node daemon is **astar-server**; the collection of engine crates
is referred to as **astar-lib**.

## Licence

These masters and their rendered outputs are part of astar and are covered by
the repository's AGPL-3.0-only licence. See `/LICENSE`.
