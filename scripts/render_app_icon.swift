#!/usr/bin/env swift

import AppKit
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let fileManager = FileManager.default
let root = URL(fileURLWithPath: fileManager.currentDirectoryPath)
let iconDir = root.appendingPathComponent("src-tauri/icons", isDirectory: true)
let masterURL = iconDir.appendingPathComponent("icon-master.png")
let iconsetDir = iconDir.appendingPathComponent("gtui.iconset", isDirectory: true)

struct RGBA {
    var r: UInt8
    var g: UInt8
    var b: UInt8
    var a: UInt8
}

func ensureDirectory(_ url: URL) throws {
    try fileManager.createDirectory(at: url, withIntermediateDirectories: true)
}

func removeIfExists(_ url: URL) throws {
    if fileManager.fileExists(atPath: url.path) {
        try fileManager.removeItem(at: url)
    }
}

func loadCGImage(from url: URL) throws -> CGImage {
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
    else {
        throw NSError(domain: "gtui.icon", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "Failed to load image at \(url.path)"
        ])
    }
    return image
}

func makeBitmapContext(width: Int, height: Int) throws -> CGContext {
    guard let context = CGContext(
        data: nil,
        width: width,
        height: height,
        bitsPerComponent: 8,
        bytesPerRow: width * 4,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        throw NSError(domain: "gtui.icon", code: 2, userInfo: [
            NSLocalizedDescriptionKey: "Failed to create bitmap context \(width)x\(height)"
        ])
    }
    context.setShouldAntialias(true)
    context.interpolationQuality = .high
    return context
}

func writePNG(_ image: CGImage, to url: URL) throws {
    guard let destination = CGImageDestinationCreateWithURL(
        url as CFURL,
        UTType.png.identifier as CFString,
        1,
        nil
    ) else {
        throw NSError(domain: "gtui.icon", code: 3, userInfo: [
            NSLocalizedDescriptionKey: "Failed to create PNG destination at \(url.path)"
        ])
    }
    CGImageDestinationAddImage(destination, image, nil)
    guard CGImageDestinationFinalize(destination) else {
        throw NSError(domain: "gtui.icon", code: 4, userInfo: [
            NSLocalizedDescriptionKey: "Failed to finalize PNG at \(url.path)"
        ])
    }
}

func clamp(_ value: Int, min: Int, max: Int) -> Int {
    Swift.max(min, Swift.min(max, value))
}

func colorDistance(_ lhs: RGBA, _ rhs: RGBA) -> Int {
    abs(Int(lhs.r) - Int(rhs.r))
        + abs(Int(lhs.g) - Int(rhs.g))
        + abs(Int(lhs.b) - Int(rhs.b))
}

func applyAlphaScale(
    pixels: UnsafeMutablePointer<UInt8>,
    offset: Int,
    newAlpha: UInt8
) {
    let oldAlpha = pixels[offset + 3]
    if oldAlpha == 0 {
        return
    }

    let scale = CGFloat(newAlpha) / CGFloat(oldAlpha)
    pixels[offset] = UInt8(clamp(Int((CGFloat(pixels[offset]) * scale).rounded()), min: 0, max: 255))
    pixels[offset + 1] = UInt8(clamp(Int((CGFloat(pixels[offset + 1]) * scale).rounded()), min: 0, max: 255))
    pixels[offset + 2] = UInt8(clamp(Int((CGFloat(pixels[offset + 2]) * scale).rounded()), min: 0, max: 255))
    pixels[offset + 3] = newAlpha
}

func sampleBackgroundColor(data: UnsafeMutablePointer<UInt8>, width: Int, height: Int) -> RGBA {
    let samplePoints = [
        (0, 0),
        (width - 1, 0),
        (0, height - 1),
        (width - 1, height - 1),
        (width / 2, 0),
        (width / 2, height - 1),
        (0, height / 2),
        (width - 1, height / 2),
    ]

    var totalR = 0
    var totalG = 0
    var totalB = 0
    var totalA = 0

    for (x, y) in samplePoints {
        let offset = (y * width + x) * 4
        totalR += Int(data[offset])
        totalG += Int(data[offset + 1])
        totalB += Int(data[offset + 2])
        totalA += Int(data[offset + 3])
    }

    let count = samplePoints.count
    return RGBA(
        r: UInt8(totalR / count),
        g: UInt8(totalG / count),
        b: UInt8(totalB / count),
        a: UInt8(totalA / count)
    )
}

func stripEdgeBackground(from image: CGImage) throws -> CGImage {
    let width = image.width
    let height = image.height
    let context = try makeBitmapContext(width: width, height: height)
    context.clear(CGRect(x: 0, y: 0, width: width, height: height))
    context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))

    guard let raw = context.data else {
        throw NSError(domain: "gtui.icon", code: 5, userInfo: [
            NSLocalizedDescriptionKey: "No bitmap data"
        ])
    }
    let pixels = raw.bindMemory(to: UInt8.self, capacity: width * height * 4)
    let background = sampleBackgroundColor(data: pixels, width: width, height: height)

    // The generated source art sits on a light matte background, so edge-only
    // trimming is not enough. Key out pixels close to the sampled background
    // everywhere, with a soft ramp so the console edge still antialiases cleanly.
    let clearThreshold = 64
    let softenThreshold = 132
    let softenRange = CGFloat(softenThreshold - clearThreshold)

    for index in 0..<(width * height) {
        let offset = index * 4
        let alpha = pixels[offset + 3]
        if alpha == 0 {
            continue
        }

        let rgba = RGBA(
            r: pixels[offset],
            g: pixels[offset + 1],
            b: pixels[offset + 2],
            a: alpha
        )
        let distance = colorDistance(rgba, background)
        if distance <= clearThreshold {
            applyAlphaScale(pixels: pixels, offset: offset, newAlpha: 0)
            continue
        }
        if distance >= softenThreshold {
            continue
        }

        let t = CGFloat(distance - clearThreshold) / softenRange
        let eased = t * t * (3.0 - 2.0 * t)
        let newAlpha = UInt8(clamp(Int((CGFloat(alpha) * eased).rounded()), min: 0, max: 255))
        applyAlphaScale(pixels: pixels, offset: offset, newAlpha: newAlpha)
    }

    func isExteriorGlowCandidate(_ index: Int) -> Bool {
        let offset = index * 4
        let alpha = pixels[offset + 3]
        if alpha < 16 {
            return true
        }

        let red = Int(pixels[offset])
        let green = Int(pixels[offset + 1])
        let blue = Int(pixels[offset + 2])
        let high = max(red, green, blue)
        let low = min(red, green, blue)
        let saturation = high - low
        let luminance = (red + green + blue) / 3

        return luminance >= 108 && saturation <= 26
    }

    var visited = Array(repeating: false, count: width * height)
    var queue = Array(repeating: 0, count: width * height)
    var head = 0
    var tail = 0

    func enqueue(_ x: Int, _ y: Int) {
        let index = y * width + x
        if visited[index] || !isExteriorGlowCandidate(index) {
            return
        }
        visited[index] = true
        queue[tail] = index
        tail += 1
    }

    for x in 0..<width {
        enqueue(x, 0)
        enqueue(x, height - 1)
    }
    for y in 0..<height {
        enqueue(0, y)
        enqueue(width - 1, y)
    }

    while head < tail {
        let index = queue[head]
        head += 1
        let offset = index * 4
        pixels[offset + 3] = 0

        let x = index % width
        let y = index / width
        if x > 0 { enqueue(x - 1, y) }
        if x + 1 < width { enqueue(x + 1, y) }
        if y > 0 { enqueue(x, y - 1) }
        if y + 1 < height { enqueue(x, y + 1) }
    }

    let bottomGlowStart = Int(CGFloat(height) * 0.78)
    for y in bottomGlowStart..<height {
        for x in 0..<width {
            let offset = (y * width + x) * 4
            let alpha = pixels[offset + 3]
            if alpha == 0 {
                continue
            }

            let red = Int(pixels[offset])
            let green = Int(pixels[offset + 1])
            let blue = Int(pixels[offset + 2])
            let high = max(red, green, blue)
            let low = min(red, green, blue)
            let saturation = high - low
            let luminance = (red + green + blue) / 3

            if luminance >= 112 && saturation <= 28 {
                applyAlphaScale(pixels: pixels, offset: offset, newAlpha: 0)
            }
        }
    }

    guard let stripped = context.makeImage() else {
        throw NSError(domain: "gtui.icon", code: 6, userInfo: [
            NSLocalizedDescriptionKey: "Failed to render stripped image"
        ])
    }
    return stripped
}

func trimTransparentBounds(of image: CGImage) throws -> CGImage {
    let width = image.width
    let height = image.height
    let context = try makeBitmapContext(width: width, height: height)
    context.clear(CGRect(x: 0, y: 0, width: width, height: height))
    context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))

    guard let raw = context.data else {
        throw NSError(domain: "gtui.icon", code: 7, userInfo: [
            NSLocalizedDescriptionKey: "No bitmap data for trimming"
        ])
    }
    let pixels = raw.bindMemory(to: UInt8.self, capacity: width * height * 4)

    var minX = width
    var minY = height
    var maxX = -1
    var maxY = -1

    for y in 0..<height {
        for x in 0..<width {
            let offset = (y * width + x) * 4
            if pixels[offset + 3] < 8 {
                continue
            }
            minX = Swift.min(minX, x)
            minY = Swift.min(minY, y)
            maxX = Swift.max(maxX, x)
            maxY = Swift.max(maxY, y)
        }
    }

    guard maxX >= minX, maxY >= minY else {
        throw NSError(domain: "gtui.icon", code: 8, userInfo: [
            NSLocalizedDescriptionKey: "Trim produced an empty image"
        ])
    }

    let paddingX = max(1, Int(CGFloat(maxX - minX + 1) * 0.02))
    let paddingY = max(1, Int(CGFloat(maxY - minY + 1) * 0.02))
    let cropRect = CGRect(
        x: clamp(minX - paddingX, min: 0, max: width - 1),
        y: clamp(minY - paddingY, min: 0, max: height - 1),
        width: clamp(maxX - minX + 1 + paddingX * 2, min: 1, max: width),
        height: clamp(maxY - minY + 1 + paddingY * 2, min: 1, max: height)
    )

    guard let cropped = image.cropping(to: cropRect) else {
        throw NSError(domain: "gtui.icon", code: 9, userInfo: [
            NSLocalizedDescriptionKey: "Failed to crop image"
        ])
    }
    return cropped
}

func renderSquareIcon(from image: CGImage, size: Int) throws -> CGImage {
    let context = try makeBitmapContext(width: size, height: size)
    context.clear(CGRect(x: 0, y: 0, width: size, height: size))

    let sizeF = CGFloat(size)
    let insetRatio: CGFloat = size <= 32 ? 0.06 : 0.08
    let maxExtent = sizeF * (1.0 - insetRatio * 2.0)
    let srcWidth = CGFloat(image.width)
    let srcHeight = CGFloat(image.height)
    let scale = min(maxExtent / srcWidth, maxExtent / srcHeight)
    let drawWidth = srcWidth * scale
    let drawHeight = srcHeight * scale
    let drawRect = CGRect(
        x: (sizeF - drawWidth) / 2.0,
        y: (sizeF - drawHeight) / 2.0,
        width: drawWidth,
        height: drawHeight
    )
    context.draw(image, in: drawRect)

    guard let rendered = context.makeImage() else {
        throw NSError(domain: "gtui.icon", code: 10, userInfo: [
            NSLocalizedDescriptionKey: "Failed to render output icon"
        ])
    }
    return rendered
}

let iconsetFiles: [(String, Int)] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
]

let outputFiles: [(String, Int)] = [
    ("32x32.png", 32),
    ("128x128.png", 128),
    ("128x128@2x.png", 256),
    ("icon.png", 1024),
]

do {
    try ensureDirectory(iconDir)
    try removeIfExists(iconsetDir)
    try ensureDirectory(iconsetDir)

    let master = try loadCGImage(from: masterURL)
    let stripped = try stripEdgeBackground(from: master)
    let trimmed = try trimTransparentBounds(of: stripped)

    for (filename, size) in iconsetFiles {
        let rendered = try renderSquareIcon(from: trimmed, size: size)
        try writePNG(rendered, to: iconsetDir.appendingPathComponent(filename))
    }

    for (filename, size) in outputFiles {
        let rendered = try renderSquareIcon(from: trimmed, size: size)
        try writePNG(rendered, to: iconDir.appendingPathComponent(filename))
    }

    let iconutil = Process()
    iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
    iconutil.arguments = [
        "-c", "icns",
        iconsetDir.lastPathComponent,
        "-o", iconDir.appendingPathComponent("icon.icns").path,
    ]
    iconutil.currentDirectoryURL = iconDir
    try iconutil.run()
    iconutil.waitUntilExit()
    guard iconutil.terminationStatus == 0 else {
        throw NSError(domain: "gtui.icon", code: 11, userInfo: [
            NSLocalizedDescriptionKey: "iconutil failed"
        ])
    }

    try removeIfExists(iconsetDir)
    print("Rendered GTUI icon assets from \(masterURL.lastPathComponent)")
} catch {
    fputs("render_app_icon.swift: \(error.localizedDescription)\n", stderr)
    exit(1)
}
