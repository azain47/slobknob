// Renders SF Symbols to white PNGs for KnobWheel (KW_DARK=1: near-black, "-dark" suffix).
// Usage: swift make-icons.swift <out-dir> <file-name>=<sf-symbol-name> ...
import AppKit

let args = CommandLine.arguments
let outDir = URL(fileURLWithPath: args[1])
let px: CGFloat = 128 // canvas size in pixels; drawn at up to 64pt, so crisp on Retina
let dark = ProcessInfo.processInfo.environment["KW_DARK"] == "1"

for pair in args.dropFirst(2) {
    let parts = pair.split(separator: "=").map(String.init)
    let (file, symbol) = (parts[0], parts[1])
    let config = NSImage.SymbolConfiguration(pointSize: 88, weight: .medium)
    guard let base = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)?
        .withSymbolConfiguration(config) else {
        print("missing symbol: \(symbol)"); continue
    }
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(px), pixelsHigh: Int(px),
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = base.size
    let scale = min(px * 0.9 / s.width, px * 0.9 / s.height)
    let r = NSRect(x: (px - s.width * scale) / 2, y: (px - s.height * scale) / 2,
                   width: s.width * scale, height: s.height * scale)
    base.draw(in: r)
    (dark ? NSColor(white: 0.08, alpha: 1) : NSColor.white).set()
    r.fill(using: .sourceAtop)
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!
        .write(to: outDir.appendingPathComponent(file + (dark ? "-dark" : "") + ".png"))
}
