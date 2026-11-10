// Draws NexaDesk's app icon, 1024×1024, as a PNG. Run through scripts/make-app-icon.sh,
// which turns it into assets/icon/AppIcon.icns.
//
//   swift scripts/make-app-icon.swift <out.png>
//
// The look follows the UI (IBM Carbon): a flat Gray 100 plate on the macOS icon grid
// (an 824 pt rounded square in a 1024 pt canvas, with the system's drop shadow), a Blue 60
// remote screen with a pointer on it, and the local screen in front, outlined in white.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: make-app-icon.swift <out.png>\n".data(using: .utf8)!)
    exit(2)
}
let out = URL(fileURLWithPath: CommandLine.arguments[1])

let size = 1024
let space = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
// Draw with the origin at the top left, as in the comments' coordinates.
ctx.translateBy(x: 0, y: CGFloat(size))
ctx.scaleBy(x: 1, y: -1)

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
}
let gray100: UInt32 = 0x161616
let blue60: UInt32 = 0x0F62FE
let white: UInt32 = 0xFFFFFF

// Plate, with the shadow macOS icons carry. Shadow offsets are in unflipped space: -y is down.
let plate = CGPath(roundedRect: CGRect(x: 100, y: 100, width: 824, height: 824),
                   cornerWidth: 185, cornerHeight: 185, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 28, color: color(0x000000, 0.35))
ctx.addPath(plate)
ctx.setFillColor(color(gray100))
ctx.fillPath()
ctx.restoreGState()
// A faint rim so the plate holds its edge on a dark Dock.
ctx.addPath(CGPath(roundedRect: CGRect(x: 101, y: 101, width: 822, height: 822),
                   cornerWidth: 184, cornerHeight: 184, transform: nil))
ctx.setStrokeColor(color(white, 0.08))
ctx.setLineWidth(2)
ctx.strokePath()

// The two screens together are centred on the plate: nudged right, and a little high so
// the stand balances them.
ctx.translateBy(x: 10, y: 0)
let dy: CGFloat = -14

// Remote screen: Blue 60, behind, to the upper right.
ctx.setFillColor(color(blue60))
ctx.fill(CGRect(x: 400, y: 260 + dy, width: 400, height: 280))

// Pointer on the remote screen: you are controlling that one.
let pointer: [CGPoint] = [(0, 0), (0, 66), (17, 51), (30, 79), (41, 74), (28, 47), (50, 47)]
    .map { CGPoint(x: 676 + $0.0 * 1.45, y: 292 + dy + $0.1 * 1.45) }
ctx.addLines(between: pointer)
ctx.closePath()
ctx.setFillColor(color(white))
ctx.fillPath()

// Local screen: in front, to the lower left; filled with the plate colour so it covers the
// blue, outlined in white with square corners. Then its stand.
let local = CGRect(x: 224, y: 400 + dy, width: 400, height: 280)
ctx.setFillColor(color(gray100))
ctx.fill(local)
ctx.setStrokeColor(color(white))
ctx.setLineWidth(40)
ctx.setLineJoin(.miter)
ctx.setLineCap(.square)
ctx.stroke(local)
ctx.setLineCap(.butt)
ctx.move(to: CGPoint(x: 424, y: 700 + dy))
ctx.addLine(to: CGPoint(x: 424, y: 770 + dy))
ctx.strokePath()
ctx.move(to: CGPoint(x: 334, y: 770 + dy))
ctx.addLine(to: CGPoint(x: 514, y: 770 + dy))
ctx.strokePath()

let image = ctx.makeImage()!
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, image, nil)
guard CGImageDestinationFinalize(dest) else {
    FileHandle.standardError.write("could not write \(out.path)\n".data(using: .utf8)!)
    exit(1)
}
