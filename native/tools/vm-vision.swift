// Screen text recognition for MCP (vm_screen_text). Reads one PNG or JPEG
// screenshot on stdin and writes the recognized lines as JSON with boxes in
// image pixels (origin top-left), so a caller can act on them with the
// screenshot's frame anchor. It runs on the host with Apple Vision, so it
// works for every guest and backend with nothing installed in the guest.
//
// usage: vm-vision text [--fast] [--language CODE]... [--find TEXT] < image
//   --fast      fast recognizer, no language correction
//   --language  BCP 47 recognition language (repeatable); default automatic
//   --find      also report each case-insensitive occurrence of TEXT with
//               its own box and center point
//   --crop      read only X,Y,W,H of the image; boxes stay in image pixels
//
// usage: vm-vision image [--crop X,Y,W,H] [--max-width N] [--max-height N]
//                        [--format jpeg|png] [--quality 1-100] < image
//   the crop (default: whole image) scaled down, never up, by one factor so it
//   fits both maxima, encoded as JPEG (default when scaling) or PNG; the JSON
//   carries the bytes, the output size and the scale (output / source pixels)
//
// usage: vm-vision diff --before PATH [--cell N] < image
//   the regions where the image differs from the one at PATH (same size), as
//   boxes of N x N cells (default 16) that hold a changed pixel, merged when
//   they touch; at most 16 regions, largest first, and the changed fraction
//
// usage: vm-vision row --y Y --from X < image
//   the runs of pixels that differ from the background along row Y, right of
//   X (the background is the color just right of X): finds the control on the
//   same line as recognized text, such as a switch at the end of a list row
import Foundation
import ImageIO
import Vision

struct Failure: Error { let message: String }

let maxImageBytes = 8 * 1024 * 1024

func readImage() throws -> CGImage {
    try decodeImage(FileHandle.standardInput.readDataToEndOfFile(), "stdin")
}

func decodeImage(_ data: Data, _ origin: String) throws -> CGImage {
    guard !data.isEmpty, data.count <= maxImageBytes,
          let source = CGImageSourceCreateWithData(data as CFData, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
        throw Failure(message: "\(origin) is not a PNG or JPEG image within 8 MiB")
    }
    return image
}

/// "X,Y,W,H" inside a width x height image.
func cropRect(_ text: String, _ width: Int, _ height: Int) throws -> CGRect {
    let parts = text.split(separator: ",").compactMap { Int($0) }
    guard parts.count == 4, parts[0] >= 0, parts[1] >= 0, parts[2] >= 1, parts[3] >= 1,
          parts[0] + parts[2] <= width, parts[1] + parts[3] <= height else {
        throw Failure(message: "crop must be X,Y,W,H inside the \(width)x\(height) image")
    }
    return CGRect(x: parts[0], y: parts[1], width: parts[2], height: parts[3])
}

/// Flag values after the mode word, each flag at most once.
func flags(_ allowed: [String], _ usage: Failure) throws -> [String: String] {
    var args = Array(CommandLine.arguments.dropFirst(2)), out: [String: String] = [:]
    while let flag = args.first {
        args.removeFirst()
        guard allowed.contains(flag), out[flag] == nil, let value = args.first, !value.isEmpty else { throw usage }
        args.removeFirst()
        out[flag] = value
    }
    return out
}

/// RGBA bytes of an image, rows top to bottom.
func rgba(_ image: CGImage) throws -> [UInt8] {
    var pixels = [UInt8](repeating: 0, count: image.width * image.height * 4)
    guard let context = CGContext(data: &pixels, width: image.width, height: image.height, bitsPerComponent: 8,
                                  bytesPerRow: image.width * 4, space: CGColorSpaceCreateDeviceRGB(),
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else {
        throw Failure(message: "cannot read pixels")
    }
    context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
    return pixels
}

func transform() throws -> [String: Any] {
    let usage = Failure(message: "usage: vm-vision image [--crop X,Y,W,H] [--max-width N] [--max-height N] [--format jpeg|png] [--quality 1-100] < image")
    let args = try flags(["--crop", "--max-width", "--max-height", "--format", "--quality"], usage)
    func number(_ flag: String, _ range: ClosedRange<Int>) throws -> Int? {
        guard let text = args[flag] else { return nil }
        guard let value = Int(text), range.contains(value) else { throw usage }
        return value
    }
    let maxWidth = try number("--max-width", 16...8192), maxHeight = try number("--max-height", 16...8192)
    let quality = try number("--quality", 1...100) ?? 80
    let format = args["--format"] ?? (maxWidth != nil || maxHeight != nil ? "jpeg" : "png")
    guard ["jpeg", "png"].contains(format) else { throw usage }
    let image = try readImage()
    let crop = try args["--crop"].map { try cropRect($0, image.width, image.height) }
        ?? CGRect(x: 0, y: 0, width: image.width, height: image.height)
    guard let cropped = image.cropping(to: crop) else { throw Failure(message: "cannot crop") }
    let factor = min(1, Double(maxWidth ?? Int.max) / crop.width, Double(maxHeight ?? Int.max) / crop.height)
    let width = max(1, Int((crop.width * factor).rounded())), height = max(1, Int((crop.height * factor).rounded()))
    var output = cropped
    if width != cropped.width || height != cropped.height {
        guard let context = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                      space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else {
            throw Failure(message: "cannot scale")
        }
        context.interpolationQuality = .high
        context.draw(cropped, in: CGRect(x: 0, y: 0, width: width, height: height))
        guard let scaled = context.makeImage() else { throw Failure(message: "cannot scale") }
        output = scaled
    }
    let data = NSMutableData()
    guard let destination = CGImageDestinationCreateWithData(data, (format == "jpeg" ? "public.jpeg" : "public.png") as CFString, 1, nil) else {
        throw Failure(message: "cannot encode")
    }
    let options: [CFString: Any] = format == "jpeg" ? [kCGImageDestinationLossyCompressionQuality: Double(quality) / 100] : [:]
    CGImageDestinationAddImage(destination, output, options as CFDictionary)
    guard CGImageDestinationFinalize(destination) else { throw Failure(message: "cannot encode") }
    return ["source_width": image.width, "source_height": image.height, "image_width": width, "image_height": height,
            "crop": ["x": Int(crop.minX), "y": Int(crop.minY), "width": Int(crop.width), "height": Int(crop.height)],
            "scale": Double(width) / crop.width, "mime_type": format == "jpeg" ? "image/jpeg" : "image/png",
            "byte_length": data.length, "data_base64": (data as Data).base64EncodedString()]
}

func difference() throws -> [String: Any] {
    let usage = Failure(message: "usage: vm-vision diff --before PATH [--cell N] < image")
    let args = try flags(["--before", "--cell"], usage)
    guard let path = args["--before"] else { throw usage }
    let cell = try args["--cell"].map { text -> Int in
        guard let value = Int(text), (4...256).contains(value) else { throw usage }
        return value
    } ?? 16
    let after = try readImage()
    guard let data = FileManager.default.contents(atPath: path) else { throw Failure(message: "cannot read the before image") }
    let before = try decodeImage(data, "the before image")
    let width = after.width, height = after.height
    guard before.width == width, before.height == height else {
        return ["width": width, "height": height, "resized": true, "changed_fraction": 1.0,
                "regions": [["x": 0, "y": 0, "width": width, "height": height, "center_x": width / 2, "center_y": height / 2]]]
    }
    let a = try rgba(before), b = try rgba(after)
    let columns = (width + cell - 1) / cell, rows = (height + cell - 1) / cell
    var changed = [Bool](repeating: false, count: columns * rows)
    // JPEG re-encoding moves untouched pixels by a few steps; a redraw moves them further.
    let threshold = 24
    for y in 0..<height {
        let row = y * width * 4, cellRow = (y / cell) * columns
        var x = 0
        while x < width {
            if changed[cellRow + x / cell] { x = (x / cell + 1) * cell; continue }
            let i = row + x * 4
            if abs(Int(a[i]) - Int(b[i])) > threshold || abs(Int(a[i + 1]) - Int(b[i + 1])) > threshold
                || abs(Int(a[i + 2]) - Int(b[i + 2])) > threshold {
                changed[cellRow + x / cell] = true
                x = (x / cell + 1) * cell
                continue
            }
            x += 1
        }
    }
    // Touching cells (8-neighbourhood) form one region.
    var seen = [Bool](repeating: false, count: changed.count)
    var regions: [(x0: Int, y0: Int, x1: Int, y1: Int, cells: Int)] = []
    for start in changed.indices where changed[start] && !seen[start] {
        var stack = [start], box = (x0: columns, y0: rows, x1: -1, y1: -1, cells: 0)
        seen[start] = true
        while let index = stack.popLast() {
            let cx = index % columns, cy = index / columns
            box = (min(box.x0, cx), min(box.y0, cy), max(box.x1, cx), max(box.y1, cy), box.cells + 1)
            for dy in -1...1 {
                for dx in -1...1 {
                    let nx = cx + dx, ny = cy + dy
                    guard nx >= 0, ny >= 0, nx < columns, ny < rows else { continue }
                    let next = ny * columns + nx
                    if changed[next] && !seen[next] { seen[next] = true; stack.append(next) }
                }
            }
        }
        regions.append(box)
    }
    let total = regions.reduce(0) { $0 + $1.cells }
    regions.sort { ($0.x1 - $0.x0 + 1) * ($0.y1 - $0.y0 + 1) > ($1.x1 - $1.x0 + 1) * ($1.y1 - $1.y0 + 1) }
    let boxes = regions.prefix(16).map { r -> [String: Int] in
        let x = r.x0 * cell, y = r.y0 * cell
        let w = min(width, (r.x1 + 1) * cell) - x, h = min(height, (r.y1 + 1) * cell) - y
        return ["x": x, "y": y, "width": w, "height": h, "center_x": x + w / 2, "center_y": y + h / 2]
    }
    return ["width": width, "height": height, "resized": false, "cell": cell, "region_count": regions.count,
            "changed_fraction": (Double(total) / Double(changed.count) * 10000).rounded() / 10000, "regions": Array(boxes)]
}

func rowRuns() throws -> [String: Any] {
    let args = Array(CommandLine.arguments.dropFirst(2))
    let usage = Failure(message: "usage: vm-vision row --y Y --from X < image")
    guard args.count == 4, args[0] == "--y", args[2] == "--from", let y = Int(args[1]), let from = Int(args[3]) else { throw usage }
    let image = try readImage()
    let width = image.width, height = image.height
    guard y >= 2, y < height - 2, from >= 0, from < width - 16 else { throw Failure(message: "row outside the image") }
    // Five rows around Y, as RGBA.
    let rows = 5, bytesPerRow = width * 4
    var pixels = [UInt8](repeating: 0, count: rows * bytesPerRow)
    guard let context = CGContext(data: &pixels, width: width, height: rows, bitsPerComponent: 8, bytesPerRow: bytesPerRow,
                                  space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else {
        throw Failure(message: "cannot read pixels")
    }
    // CoreGraphics draws with the origin at the bottom: shift the image so rows y-2...y+2 land in the context.
    context.draw(image, in: CGRect(x: 0, y: -(height - (y + 3)), width: width, height: height))
    func color(_ x: Int) -> [Int] {
        var sum = [0, 0, 0]
        for r in 0..<rows {
            for c in 0..<3 { sum[c] += Int(pixels[r * bytesPerRow + x * 4 + c]) }
        }
        return sum.map { $0 / rows }
    }
    var background = [0, 0, 0]
    for x in from..<(from + 6) {
        let c = color(x)
        for i in 0..<3 { background[i] += c[i] }
    }
    background = background.map { $0 / 6 }
    var runs: [[String: Int]] = []
    var start: Int?, last = 0
    for x in from..<width {
        let c = color(x)
        // A switch that is off is a light gray track with a white knob on a
        // near-white row, and a sheet over the window dims all of it.
        let differs = (0..<3).map { abs(c[$0] - background[$0]) }.max()! > 7
        if differs {
            if start == nil || x - last > 3 {
                if let s = start { runs.append(["x": s, "width": last - s + 1]) }
                start = x
            }
            last = x
        }
        if runs.count >= 64 { break }
    }
    if let s = start, runs.count < 64 { runs.append(["x": s, "width": last - s + 1]) }
    return ["width": width, "height": height, "y": y, "background": background,
            "runs": runs.filter { $0["width"]! >= 4 }.map { run -> [String: Int] in
                var out = run
                out["center_x"] = run["x"]! + run["width"]! / 2
                return out
            }]
}

func recognize() throws -> [String: Any] {
    var args = Array(CommandLine.arguments.dropFirst())
    let usage = Failure(message: "usage: vm-vision text [--fast] [--language CODE]... [--find TEXT] [--crop X,Y,W,H] < image")
    guard args.first == "text" else { throw usage }
    args.removeFirst()
    var fast = false, languages: [String] = [], find: String?, cropText: String?
    while let flag = args.first {
        args.removeFirst()
        switch flag {
        case "--fast": fast = true
        case "--language", "--find", "--crop":
            guard let value = args.first, !value.isEmpty, value.count <= 256 else { throw usage }
            args.removeFirst()
            if flag == "--find" { find = value } else if flag == "--crop" { cropText = value } else { languages.append(value) }
        default: throw usage
        }
    }
    let full = try readImage()
    var image = full, origin = CGPoint.zero
    if let cropText {
        let crop = try cropRect(cropText, full.width, full.height)
        guard let cropped = full.cropping(to: crop) else { throw Failure(message: "cannot crop") }
        image = cropped
        origin = crop.origin
    }
    let width = Double(image.width), height = Double(image.height)
    func request() -> VNRecognizeTextRequest {
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = fast ? .fast : .accurate
        request.usesLanguageCorrection = !fast
        if languages.isEmpty {
            request.automaticallyDetectsLanguage = true
        } else {
            request.recognitionLanguages = languages
        }
        return request
    }
    // Vision misses much of a busy full desktop (it drops whole rows of small
    // text) and reads 10-12 px glyphs reliably only when upscaled. So the
    // screen is read in overlapping full-width bands, each upscaled twice.
    // A line touching a band's inner cut edge is left to the neighbouring
    // band, which holds it whole; the overlap exceeds any desktop text line.
    let bandHeight = 360.0, overlap = 96.0
    var bands: [CGRect] = []
    if height <= bandHeight + overlap {
        bands = [CGRect(x: 0, y: 0, width: width, height: height)]
    } else {
        var top = 0.0
        while true {
            let y = min(top, height - bandHeight)
            bands.append(CGRect(x: 0, y: y, width: width, height: bandHeight))
            if y + bandHeight >= height { break }
            top += bandHeight - overlap
        }
    }
    struct Line { let rect: CGRect; let text: String; let confidence: Float; let candidate: VNRecognizedText; let band: CGRect }
    // Normalized Vision box (origin bottom left) within a band, to image pixels (origin top left).
    func pixels(_ r: CGRect, _ band: CGRect) -> CGRect {
        CGRect(x: band.minX + r.minX * band.width, y: band.minY + (1 - r.maxY) * band.height,
               width: r.width * band.width, height: r.height * band.height)
    }
    var found: [Line] = []
    for band in bands {
        guard let crop = image.cropping(to: band) else { continue }
        var input = crop
        let scale = crop.width <= 2048 ? 2 : 1
        if scale > 1, let context = CGContext(data: nil, width: crop.width * scale, height: crop.height * scale, bitsPerComponent: 8,
                                              bytesPerRow: 0, space: CGColorSpaceCreateDeviceRGB(),
                                              bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) {
            context.interpolationQuality = .high
            context.draw(crop, in: CGRect(x: 0, y: 0, width: crop.width * scale, height: crop.height * scale))
            input = context.makeImage() ?? crop
        }
        let recognizer = request()
        try VNImageRequestHandler(cgImage: input).perform([recognizer])
        for observation in recognizer.results ?? [] {
            guard let candidate = observation.topCandidates(1).first else { continue }
            let rect = pixels(observation.boundingBox, band)
            if band.minY > 0 && rect.minY < band.minY + 2 || band.maxY < height && rect.maxY > band.maxY - 2 { continue }
            found.append(Line(rect: rect, text: candidate.string, confidence: candidate.confidence, candidate: candidate, band: band))
        }
    }
    // Lines inside an overlap are read twice; keep the better reading.
    func better(_ a: Line, _ b: Line) -> Bool {
        a.confidence != b.confidence ? a.confidence > b.confidence : a.text.count > b.text.count
    }
    var kept: [Line] = []
    for line in found.sorted(by: better) {
        let duplicate = kept.contains { other in
            let shared = other.rect.intersection(line.rect)
            return !shared.isNull && shared.width * shared.height >= 0.5 * min(other.rect.width * other.rect.height, line.rect.width * line.rect.height)
        }
        if !duplicate { kept.append(line) }
    }
    func box(_ rect: CGRect) -> [String: Int] {
        let x0 = max(0, min(width, rect.minX)), x1 = max(0, min(width, rect.maxX))
        let y0 = max(0, min(height, rect.minY)), y1 = max(0, min(height, rect.maxY))
        let ox = Int(origin.x), oy = Int(origin.y)
        return ["x": ox + Int(x0.rounded(.down)), "y": oy + Int(y0.rounded(.down)),
                "width": Int((x1 - x0).rounded(.up)), "height": Int((y1 - y0).rounded(.up)),
                "center_x": ox + Int(((x0 + x1) / 2).rounded(.down)), "center_y": oy + Int(((y0 + y1) / 2).rounded(.down))]
    }
    var lines: [[String: Any]] = [], matches: [[String: Any]] = []
    for found in kept {
        var line: [String: Any] = box(found.rect)
        line["text"] = found.text
        line["confidence"] = (Double(found.confidence) * 100).rounded() / 100
        lines.append(line)
        guard let find else { continue }
        let text = found.text
        var search = text.startIndex..<text.endIndex
        while let range = text.range(of: find, options: [.caseInsensitive, .diacriticInsensitive], range: search) {
            let rect = (try? found.candidate.boundingBox(for: range)).map { pixels($0.boundingBox, found.band) } ?? found.rect
            var match: [String: Any] = box(rect)
            match["text"] = String(text[range])
            match["line"] = text
            matches.append(match)
            search = range.upperBound..<text.endIndex
        }
    }
    // Reading order: top to bottom, then left to right within a row.
    func order(_ a: [String: Any], _ b: [String: Any]) -> Bool {
        let ay = a["center_y"] as! Int, by = b["center_y"] as! Int
        let rowHeight = max(4, min(a["height"] as! Int, b["height"] as! Int) / 2)
        if abs(ay - by) > rowHeight { return ay < by }
        return (a["x"] as! Int) < (b["x"] as! Int)
    }
    lines.sort(by: order)
    matches.sort(by: order)
    var result: [String: Any] = ["width": full.width, "height": full.height, "lines": lines,
                                 "recognizer": fast ? "vision-fast" : "vision-accurate"]
    if find != nil { result["matches"] = matches }
    return result
}

do {
    let result: [String: Any]
    switch CommandLine.arguments.dropFirst().first {
    case "row": result = try rowRuns()
    case "image": result = try transform()
    case "diff": result = try difference()
    default: result = try recognize()
    }
    let json = try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys])
    FileHandle.standardOutput.write(json)
    FileHandle.standardOutput.write(Data([0x0a]))
} catch let failure as Failure {
    FileHandle.standardError.write(Data((failure.message + "\n").utf8))
    exit(2)
} catch {
    FileHandle.standardError.write(Data(("text recognition failed: \(error.localizedDescription)\n").utf8))
    exit(1)
}
