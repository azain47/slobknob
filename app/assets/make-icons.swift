// Draws the app icon (1024px) and the monochrome tray icon (44px template).
// Usage: swift make-icons.swift <out-dir>
import AppKit

func png(_ size: Int, _ path: String, _ draw: (CGRect) -> Void) {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    draw(CGRect(x: 0, y: 0, width: size, height: size))
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: path))
}

func ring(in r: CGRect, slices: Int, gap: CGFloat, inner: CGFloat, outer: CGFloat, color: (Int) -> NSColor) {
    let c = CGPoint(x: r.midX, y: r.midY)
    for i in 0..<slices {
        let span = 360.0 / CGFloat(slices)
        let mid = 90 - CGFloat(i) * span
        let p = NSBezierPath()
        p.appendArc(withCenter: c, radius: outer, startAngle: mid - span / 2 + gap, endAngle: mid + span / 2 - gap)
        p.appendArc(withCenter: c, radius: inner, startAngle: mid + span / 2 - gap, endAngle: mid - span / 2 + gap, clockwise: true)
        p.close()
        color(i).setFill()
        p.fill()
    }
}

let out = CommandLine.arguments[1]
png(1024, out + "/app-icon.png") { r in
    let bg = NSBezierPath(roundedRect: r.insetBy(dx: 100, dy: 100), xRadius: 185, yRadius: 185)
    NSGradient(starting: NSColor(white: 0.16, alpha: 1), ending: NSColor(white: 0.05, alpha: 1))!.draw(in: bg, angle: -90)
    ring(in: r, slices: 8, gap: 3, inner: 170, outer: 330) { i in
        i == 0 ? NSColor(srgbRed: 0.2, green: 0.47, blue: 0.96, alpha: 1) : NSColor(white: 1, alpha: 0.14)
    }
    NSColor(white: 0.04, alpha: 1).setFill()
    NSBezierPath(ovalIn: CGRect(x: 512 - 140, y: 512 - 140, width: 280, height: 280)).fill()
    NSColor(white: 1, alpha: 0.9).setFill()
    NSBezierPath(ovalIn: CGRect(x: 512 - 28, y: 512 + 60, width: 56, height: 56)).fill()
}
png(44, out + "/tray.png") { r in
    ring(in: r, slices: 6, gap: 6, inner: 9, outer: 20) { i in NSColor(white: 0, alpha: i == 0 ? 1 : 0.55) }
    NSColor.black.setFill()
    NSBezierPath(ovalIn: CGRect(x: 22 - 3, y: 22 - 3, width: 6, height: 6)).fill()
}
