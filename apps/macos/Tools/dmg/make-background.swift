// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

// Generates the DMG window background: a light canvas with a gray arrow pointing
// from the astar.app icon (left) toward the Applications drop target (right).
// Run: swift Tools/dmg/make-background.swift  → writes Tools/dmg/background.png
// Keep the canvas size in sync with window_rect/icon_locations in dmg-settings.py.
import AppKit

let W = 640, H = 400
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: W, pixelsHigh: H,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// Background fill (subtle light gray).
NSColor(calibratedWhite: 0.93, alpha: 1).setFill()
NSBezierPath(rect: NSRect(x: 0, y: 0, width: W, height: H)).fill()

// Small horizontal arrow that fits in the gap BETWEEN the two 160px icons
// (app right edge ~x240, Applications left edge ~x400) so neither icon covers it.
let midY: CGFloat = 200
let shaftStartX: CGFloat = 266, shaftEndX: CGFloat = 356, tipX: CGFloat = 380
let headHalf: CGFloat = 13
let arrow = NSColor(calibratedWhite: 0.55, alpha: 1)
arrow.setStroke()
arrow.setFill()

let shaft = NSBezierPath()
shaft.lineWidth = 6
shaft.lineCapStyle = .round
shaft.move(to: NSPoint(x: shaftStartX, y: midY))
shaft.line(to: NSPoint(x: shaftEndX, y: midY))
shaft.stroke()

let head = NSBezierPath()
head.move(to: NSPoint(x: tipX, y: midY))                 // tip
head.line(to: NSPoint(x: shaftEndX, y: midY + headHalf))
head.line(to: NSPoint(x: shaftEndX, y: midY - headHalf))
head.close()
head.fill()

// Caption near the bottom, clear of the icon name labels.
let caption = "Drag astar to Applications"
let style = NSMutableParagraphStyle(); style.alignment = .center
let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.systemFont(ofSize: 14, weight: .medium),
    .foregroundColor: NSColor(calibratedWhite: 0.5, alpha: 1),
    .paragraphStyle: style,
]
let text = NSAttributedString(string: caption, attributes: attrs)
text.draw(in: NSRect(x: 0, y: 34, width: CGFloat(W), height: 20))

NSGraphicsContext.restoreGraphicsState()

let out = "Tools/dmg/background.png"
guard let png = rep.representation(using: .png, properties: [:]) else {
    FileHandle.standardError.write("failed to encode PNG\n".data(using: .utf8)!); exit(1)
}
try! png.write(to: URL(fileURLWithPath: out))
print("wrote \(out) (\(W)x\(H))")
