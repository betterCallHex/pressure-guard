import AppKit

enum Palette {
    static let paper = rgb(0xF1_EBDD)
    static let ink = rgb(0x1A_1816)
    static let blade = rgb(0x2F_7FC4)
    static let bladeTip = rgb(0x4F_B3E0)

    static func rgb(_ hex: UInt32) -> NSColor {
        NSColor(
            srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
            green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255,
            alpha: 1
        )
    }
}

let art = [
    ".kkkkkkkkkkkkkk.",
    "kk............kk",
    "k.k..kkkkkk..k.k",
    "k..kks..bbbkk..k",
    "k..kb..bbs..k..k",
    "k.k.b..b.....k.k",
    "k.k.b..kkbbb.k.k",
    "k.k.bbkkkk.sbk.k",
    "k.k..bkkkk..bk.k",
    "k.ks...kksb..k.k",
    "k.kbbbbb.sb..k.k",
    "k..kbb...bb.k..k",
    "k..kk...sb.kk..k",
    "k.k..kkkkkk..k.k",
    "kk............kk",
    ".kkkkkkkkkkkkkk.",
]

let canvas = 1024
let cellSize = 48
let margin = (canvas - cellSize * art.count) / 2

func color(of pixel: Character) -> NSColor? {
    switch pixel {
    case "k": return Palette.ink
    case "b": return Palette.blade
    case "s": return Palette.bladeTip
    default: return nil
    }
}

func drawIcon() {
    Palette.paper.setFill()
    NSRect(x: 0, y: 0, width: canvas, height: canvas).fill()
    for (row, line) in art.enumerated() {
        for (column, pixel) in line.enumerated() {
            guard let fill = color(of: pixel) else { continue }
            fill.setFill()
            NSRect(
                x: margin + column * cellSize,
                y: canvas - margin - (row + 1) * cellSize,
                width: cellSize, height: cellSize
            ).fill()
        }
    }
}

let outputPath = CommandLine.arguments.dropFirst().first ?? "docs/icon.png"
guard let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: canvas, pixelsHigh: canvas,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
) else { fatalError("Could not allocate the icon bitmap") }
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
drawIcon()
NSGraphicsContext.restoreGraphicsState()
try bitmap.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: outputPath))
