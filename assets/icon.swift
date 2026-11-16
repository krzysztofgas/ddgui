import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size = 1024
let space = CGColorSpace(name: CGColorSpace.sRGB)!

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: icon.swift <output.png>\n".data(using: .utf8)!)
    exit(2)
}

guard let ctx = CGContext(
    data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else {
    FileHandle.standardError.write("could not create context\n".data(using: .utf8)!)
    exit(1)
}

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        colorSpace: space,
        components: [
            CGFloat((hex >> 16) & 0xff) / 255, CGFloat((hex >> 8) & 0xff) / 255,
            CGFloat(hex & 0xff) / 255, alpha,
        ])!
}

let tile = CGRect(x: 100, y: 100, width: 824, height: 824)
let tilePath = CGPath(roundedRect: tile, cornerWidth: 185, cornerHeight: 185, transform: nil)

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: color(0x000000, 0.35))
ctx.addPath(tilePath)
ctx.setFillColor(color(0x1f2a44))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(tilePath)
ctx.clip()
let gradient = CGGradient(
    colorsSpace: space, colors: [color(0x4f8cff), color(0x2340a8)] as CFArray,
    locations: [0, 1])!
ctx.drawLinearGradient(
    gradient, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
ctx.restoreGState()

let card = CGMutablePath()
let left: CGFloat = 352
let right: CGFloat = 672
let bottom: CGFloat = 210
let top: CGFloat = 610
let cut: CGFloat = 86
let r: CGFloat = 34
card.move(to: CGPoint(x: left + r, y: bottom))
card.addArc(tangent1End: CGPoint(x: right, y: bottom), tangent2End: CGPoint(x: right, y: top), radius: r)
card.addArc(tangent1End: CGPoint(x: right, y: top - cut), tangent2End: CGPoint(x: right - cut, y: top), radius: 14)
card.addArc(tangent1End: CGPoint(x: right - cut, y: top), tangent2End: CGPoint(x: left, y: top), radius: 14)
card.addArc(tangent1End: CGPoint(x: left, y: top), tangent2End: CGPoint(x: left, y: bottom), radius: r)
card.addArc(tangent1End: CGPoint(x: left, y: bottom), tangent2End: CGPoint(x: right, y: bottom), radius: r)
card.closeSubpath()

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -8), blur: 18, color: color(0x0b1640, 0.35))
ctx.addPath(card)
ctx.setFillColor(color(0xffffff))
ctx.fillPath()
ctx.restoreGState()

ctx.setFillColor(color(0x2f5bd0))
for i in 0..<4 {
    let x = left + 48 + CGFloat(i) * 52
    let bar = CGRect(x: x, y: top - 150, width: 30, height: 104)
    ctx.addPath(CGPath(roundedRect: bar, cornerWidth: 10, cornerHeight: 10, transform: nil))
}
ctx.fillPath()

let arrow = CGMutablePath()
let cx: CGFloat = 512
let shaft: CGFloat = 30
arrow.move(to: CGPoint(x: cx - shaft, y: 845))
arrow.addLine(to: CGPoint(x: cx + shaft, y: 845))
arrow.addLine(to: CGPoint(x: cx + shaft, y: 735))
arrow.addLine(to: CGPoint(x: cx + 92, y: 735))
arrow.addLine(to: CGPoint(x: cx, y: 640))
arrow.addLine(to: CGPoint(x: cx - 92, y: 735))
arrow.addLine(to: CGPoint(x: cx - shaft, y: 735))
arrow.closeSubpath()
ctx.addPath(arrow)
ctx.setFillColor(color(0xffffff))
ctx.fillPath()

guard let image = ctx.makeImage(),
    let dest = CGImageDestinationCreateWithURL(
        URL(fileURLWithPath: CommandLine.arguments[1]) as CFURL, UTType.png.identifier as CFString,
        1, nil)
else {
    FileHandle.standardError.write("could not create image\n".data(using: .utf8)!)
    exit(1)
}
CGImageDestinationAddImage(dest, image, nil)
guard CGImageDestinationFinalize(dest) else {
    FileHandle.standardError.write("could not write image\n".data(using: .utf8)!)
    exit(1)
}
