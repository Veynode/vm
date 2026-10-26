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
    let data = FileHandle.standardInput.readDataToEndOfFile()
    guard !data.isEmpty, data.count <= maxImageBytes,
          let source = CGImageSourceCreateWithData(data as CFData, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
        throw Failure(message: "stdin is not a PNG or JPEG image within 8 MiB")
    }
    return image
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
    let usage = Failure(message: "usage: vm-vision text [--fast] [--language CODE]... [--find TEXT] < image")
    guard args.first == "text" else { throw usage }
    args.removeFirst()
    var fast = false, languages: [String] = [], find: String?
    while let flag = args.first {
        args.removeFirst()
        switch flag {
        case "--fast": fast = true
        case "--language", "--find":
            guard let value = args.first, !value.isEmpty, value.count <= 256 else { throw usage }
            args.removeFirst()
            if flag == "--find" { find = value } else { languages.append(value) }
        default: throw usage
        }
    }
    let image = try readImage()
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
        return ["x": Int(x0.rounded(.down)), "y": Int(y0.rounded(.down)),
                "width": Int((x1 - x0).rounded(.up)), "height": Int((y1 - y0).rounded(.up)),
                "center_x": Int(((x0 + x1) / 2).rounded(.down)), "center_y": Int(((y0 + y1) / 2).rounded(.down))]
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
    var result: [String: Any] = ["width": image.width, "height": image.height, "lines": lines,
                                 "recognizer": fast ? "vision-fast" : "vision-accurate"]
    if find != nil { result["matches"] = matches }
    return result
}

do {
    let result = CommandLine.arguments.dropFirst().first == "row" ? try rowRuns() : try recognize()
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
