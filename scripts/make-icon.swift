import AppKit

enum Palette {
    static let backgroundTop = rgb(0x1E_2B2A)
    static let backgroundBottom = rgb(0x26_3634)
    static let shield = rgb(0xF4_E3BF)
    static let shieldShade = rgb(0xD8_C49C)
    static let mercury = rgb(0xE2_4B3B)

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
    "##############",
    "######dd######",
    "#####dddd#####",
    "#####dddd#d###",
    "#####dddd#####",
    "#####dddd#d###",
    "#####drrd#####",
    "#####drrd#d###",
    "#####drrd#####",
    "####ddrrdd####",
    "####drrrrd####",
    "####drrrrd####",
    ".###ddrrdd###.",
    "..###dddd###..",
    "...########...",
    "....######....",
    ".....####.....",
    "......##......",
]

let canvas = 1024
let gridSize = 32
let cellSize = canvas / gridSize
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
let tileCornerRadius: CGFloat = 185
let bayer4x4: [[Double]] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]]
let artWidth = art[0].count
let artColumn = (gridSize - artWidth) / 2
let artRow = (gridSize - art.count) / 2

func backgroundColor(x: Int, y: Int) -> NSColor {
    let progress = Double(y) / Double(gridSize - 1)
    let threshold = (bayer4x4[y % 4][x % 4] + 0.5) / 16
    return progress > threshold ? Palette.backgroundBottom : Palette.backgroundTop
}

func color(x: Int, y: Int) -> NSColor {
    let row = y - artRow, column = x - artColumn
    guard art.indices.contains(row), (0..<artWidth).contains(column) else { return backgroundColor(x: x, y: y) }
    switch Array(art[row])[column] {
    case "#": return column < artWidth / 2 ? Palette.shield : Palette.shieldShade
    case "r": return Palette.mercury
    case "d": return Palette.backgroundTop
    default: return backgroundColor(x: x, y: y)
    }
}

func drawIcon() {
    let shape = NSBezierPath(roundedRect: tile, xRadius: tileCornerRadius, yRadius: tileCornerRadius)
    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.3)
    shadow.shadowBlurRadius = 20
    shadow.shadowOffset = NSSize(width: 0, height: -10)
    shadow.set()
    Palette.backgroundTop.setFill()
    shape.fill()
    NSGraphicsContext.restoreGraphicsState()

    shape.addClip()
    NSGraphicsContext.current?.shouldAntialias = false
    for y in 0..<gridSize {
        for x in 0..<gridSize {
            color(x: x, y: y).setFill()
            NSRect(x: x * cellSize, y: canvas - (y + 1) * cellSize, width: cellSize, height: cellSize).fill()
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
