// Makes NexaDesk/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png from the desktop
// icon: `swift scripts/make-icon.swift ../../assets/icon/AppIcon-1024.png \
//   NexaDesk/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png`

import AppKit
import CoreGraphics

// The desktop icon (macOS style: margin, rounded corners, shadow) → an iOS icon:
// a full 1024 square, opaque; iOS rounds the corners itself.
let args = CommandLine.arguments
let src = NSImage(contentsOfFile: args[1])!
var rect = CGRect(x: 0, y: 0, width: 1024, height: 1024)
let image = src.cgImage(forProposedRect: &rect, context: nil, hints: nil)!
let w = 1024
let space = CGColorSpace(name: CGColorSpace.sRGB)!
// Read the background colour from inside the rounded square.
let probe = CGContext(data: nil, width: 1024, height: 1024, bitsPerComponent: 8, bytesPerRow: 1024 * 4,
                      space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
probe.draw(image, in: CGRect(x: 0, y: 0, width: 1024, height: 1024))
let px = probe.data!.assumingMemoryBound(to: UInt8.self)
let i = (512 * 1024 + 150) * 4   // row 512, x 150 (top-down memory, middle row either way)
let bg = CGColor(srgbRed: CGFloat(px[i]) / 255, green: CGFloat(px[i + 1]) / 255, blue: CGFloat(px[i + 2]) / 255, alpha: 1)
print("background", px[i], px[i + 1], px[i + 2])

let ctx = CGContext(data: nil, width: w, height: w, bitsPerComponent: 8, bytesPerRow: w * 4,
                    space: space, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
ctx.setFillColor(bg)
ctx.fill(CGRect(x: 0, y: 0, width: w, height: w))
// The square's inside (100…924) fills the icon; only its middle is drawn, away from
// the rounded corners and the shadow.
let k = 1024.0 / 824.0
let inner = CGRect(x: 190, y: 190, width: 644, height: 644)          // source pixels
let crop = image.cropping(to: inner)!
ctx.draw(crop, in: CGRect(x: (inner.minX - 100) * k, y: (1024 - inner.maxY - 100) * k,
                          width: inner.width * k, height: inner.height * k))
let out = ctx.makeImage()!
let rep = NSBitmapImageRep(cgImage: out)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[2]))
