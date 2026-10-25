// Draws the placeholder app icon, a sail over a hull on a blue tile, as a
// 1024 px PNG: `swift make-icon.swift <out.png>`. build-app.sh turns it
// into Dhow.icns with sips and iconutil (no actool without Xcode). A
// designed icon replaces it later.
import AppKit

let size = 1024.0
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
guard
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0)
else { fatalError("no bitmap") }
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// The macOS icon grid: an 824 px tile centred in 1024.
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
let shape = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
NSGradient(
    starting: NSColor(srgbRed: 0.36, green: 0.49, blue: 0.98, alpha: 1),
    ending: NSColor(srgbRed: 0.18, green: 0.30, blue: 0.78, alpha: 1))!
    .draw(in: shape, angle: -90)

NSColor.white.setFill()
let sail = NSBezierPath()
sail.move(to: NSPoint(x: 500, y: 790))
sail.curve(
    to: NSPoint(x: 300, y: 400), controlPoint1: NSPoint(x: 420, y: 650),
    controlPoint2: NSPoint(x: 330, y: 500))
sail.line(to: NSPoint(x: 720, y: 400))
sail.close()
sail.fill()

let hull = NSBezierPath()
hull.move(to: NSPoint(x: 230, y: 350))
hull.line(to: NSPoint(x: 800, y: 350))
hull.curve(
    to: NSPoint(x: 620, y: 230), controlPoint1: NSPoint(x: 760, y: 280),
    controlPoint2: NSPoint(x: 700, y: 230))
hull.line(to: NSPoint(x: 360, y: 230))
hull.curve(
    to: NSPoint(x: 230, y: 350), controlPoint1: NSPoint(x: 300, y: 230),
    controlPoint2: NSPoint(x: 250, y: 280))
hull.close()
hull.fill()

NSGraphicsContext.current = nil
guard let png = rep.representation(using: .png, properties: [:]) else { fatalError("no PNG") }
try png.write(to: URL(fileURLWithPath: out))
