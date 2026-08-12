# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# dmgbuild settings for the styled astar.dmg — driven headlessly (writes the
# window's .DS_Store directly, so it works in CI with no Finder/GUI).
#
#   dmgbuild -s Tools/dmg/dmg-settings.py \
#            -D app=<path/to/astar.app> -D background=Tools/dmg/background.png \
#            "astar" build/astar.dmg
#
# Layout: astar.app on the left, the /Applications drop target on the right, a
# background arrow pointing app -> Applications, and large (160px) icons. Keep the
# window size + icon centers in sync with Tools/dmg/make-background.swift.
import os

app = defines.get("app", "astar.app")
appname = os.path.basename(app)

# DMG contents.
format = "UDZO"
files = [app]
symlinks = {"Applications": "/Applications"}

# Window + icon styling.
background = defines.get("background", "Tools/dmg/background.png")
window_rect = ((200, 200), (640, 400))   # (x, y), (w, h) — matches the background
default_view = "icon-view"
icon_size = 160
text_size = 13
icon_locations = {
    appname: (160, 200),       # left
    "Applications": (480, 200),  # right
}
