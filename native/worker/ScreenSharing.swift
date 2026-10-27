import CommonCrypto
import CryptoKit
import Foundation
import ImageIO

// Observation and input for macOS Guests through the Guest's own Screen Sharing
// (VNC/RFB) server. screensharingd is an Apple system daemon, so nothing in the
// Guest needs Screen Recording or Accessibility grants. The worker answers the
// Guest-service methods `info`, `screenshot` and `input_*` itself with the same
// envelopes and frame/operation rules as native/guest-macos, so the
// controller's frame and cleanup checks apply unchanged.

struct ScreenSharingError: LocalizedError {
    let code: String
    let message: String
    var errorDescription: String? { message }
}

/// Newest vmnet DHCP lease for `mac` ("1,a:2:..." drops leading zeros).
func dhcpLeaseAddress(mac: String) -> String? {
    func normalized(_ value: Substring) -> [UInt8]? {
        let octets = value.split(separator: ":").map { UInt8($0, radix: 16) }
        return octets.count == 6 && octets.allSatisfy({ $0 != nil }) ? octets.map { $0! } : nil
    }
    guard let wanted = normalized(Substring(mac)),
          let text = try? String(contentsOfFile: "/var/db/dhcpd_leases", encoding: .utf8) else { return nil }
    var best: (UInt64, String)?
    for block in text.split(separator: "}") {
        var address: String?, hardware: [UInt8]?, lease: UInt64 = 0
        for line in block.split(separator: "\n") {
            let parts = line.trimmingCharacters(in: .whitespaces).split(separator: "=", maxSplits: 1)
            guard parts.count == 2 else { continue }
            switch parts[0] {
            case "ip_address": address = String(parts[1])
            case "hw_address": hardware = parts[1].split(separator: ",", maxSplits: 1).last.flatMap(normalized)
            case "lease": lease = UInt64(parts[1].dropFirst(2), radix: 16) ?? 0
            default: break
            }
        }
        if let address, hardware == wanted, best == nil || lease >= best!.0 { best = (lease, address) }
    }
    return best?.1
}

/// Fixed-width modular exponentiation (Montgomery, odd modulus) for RFB DH.
struct Modulus {
    let p: [UInt32]
    let n0inv: UInt32
    let r2: [UInt32]
    init(bigEndian bytes: [UInt8]) {
        p = Modulus.limbs(bytes, count: (bytes.count + 3) / 4)
        var inverse: UInt32 = 1
        for _ in 0..<5 { inverse = inverse &* (2 &- p[0] &* inverse) }
        n0inv = 0 &- inverse
        var x = [UInt32](repeating: 0, count: p.count)
        x[0] = 1
        for _ in 0..<(64 * p.count) { x = Modulus.doubled(x, p) }
        r2 = x
    }
    static func limbs(_ bytes: [UInt8], count: Int) -> [UInt32] {
        var out = [UInt32](repeating: 0, count: count)
        for (i, byte) in bytes.reversed().enumerated() where i / 4 < count { out[i / 4] |= UInt32(byte) << (8 * UInt32(i % 4)) }
        return out
    }
    static func compare(_ a: [UInt32], _ b: [UInt32]) -> Int {
        for i in stride(from: a.count - 1, through: 0, by: -1) where a[i] != b[i] { return a[i] < b[i] ? -1 : 1 }
        return 0
    }
    static func subtract(_ a: inout [UInt32], _ b: [UInt32]) {
        var borrow: Int64 = 0
        for i in 0..<a.count {
            let d = Int64(a[i]) - Int64(b[i]) - borrow
            a[i] = UInt32(truncatingIfNeeded: d)
            borrow = d < 0 ? 1 : 0
        }
    }
    static func doubled(_ x: [UInt32], _ p: [UInt32]) -> [UInt32] {
        var out = x, carry: UInt32 = 0
        for i in 0..<x.count { out[i] = (x[i] << 1) | carry; carry = x[i] >> 31 }
        if carry != 0 || compare(out, p) >= 0 { subtract(&out, p) }
        return out
    }
    func multiply(_ a: [UInt32], _ b: [UInt32]) -> [UInt32] {
        let n = p.count
        var t = [UInt32](repeating: 0, count: n + 2)
        for i in 0..<n {
            var carry: UInt64 = 0
            for j in 0..<n {
                let s = UInt64(t[j]) + UInt64(a[j]) * UInt64(b[i]) + carry
                t[j] = UInt32(truncatingIfNeeded: s); carry = s >> 32
            }
            var s = UInt64(t[n]) + carry
            t[n] = UInt32(truncatingIfNeeded: s); t[n + 1] = UInt32(s >> 32)
            let m = t[0] &* n0inv
            s = UInt64(t[0]) + UInt64(m) * UInt64(p[0]); carry = s >> 32
            for j in 1..<n {
                s = UInt64(t[j]) + UInt64(m) * UInt64(p[j]) + carry
                t[j - 1] = UInt32(truncatingIfNeeded: s); carry = s >> 32
            }
            s = UInt64(t[n]) + carry
            t[n - 1] = UInt32(truncatingIfNeeded: s)
            t[n] = t[n + 1] &+ UInt32(s >> 32)
        }
        var result = Array(t[0..<n])
        if t[n] != 0 || Modulus.compare(result, p) >= 0 { Modulus.subtract(&result, p) }
        return result
    }
    /// base^exponent mod p as big-endian bytes of the modulus width.
    func power(_ base: [UInt8], _ exponent: [UInt8]) -> [UInt8] {
        var b = Modulus.limbs(base, count: p.count)
        while Modulus.compare(b, p) >= 0 { Modulus.subtract(&b, p) }
        var one = [UInt32](repeating: 0, count: p.count); one[0] = 1
        var x = multiply(one, r2)
        let mb = multiply(b, r2)
        for byte in exponent {
            for bit in stride(from: 7, through: 0, by: -1) {
                x = multiply(x, x)
                if (byte >> UInt8(bit)) & 1 == 1 { x = multiply(x, mb) }
            }
        }
        x = multiply(x, one)
        var out = [UInt8](repeating: 0, count: p.count * 4)
        for (i, limb) in x.enumerated() {
            for k in 0..<4 { out[out.count - 1 - (4 * i + k)] = UInt8(truncatingIfNeeded: limb >> (8 * UInt32(k))) }
        }
        return out
    }
}

func randomBytes(_ count: Int) -> [UInt8] {
    var bytes = [UInt8](repeating: 0, count: count)
    _ = SecRandomCopyBytes(kSecRandomDefault, count, &bytes)
    return bytes
}

/// One authenticated RFB 3.8 session (Apple Diffie-Hellman, security type 30).
final class RFBClient {
    let fd: Int32
    private(set) var width = 0
    private(set) var height = 0
    private var framebuffer: [UInt8] = []

    init(host: String, user: String, password: String) throws {
        fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw ScreenSharingError(code: "screen_sharing_unavailable", message: "socket failed") }
        var address = sockaddr_in()
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = UInt16(5900).bigEndian
        guard inet_pton(AF_INET, host, &address.sin_addr) == 1 else { close(fd); throw ScreenSharingError(code: "screen_sharing_unavailable", message: "invalid Guest address") }
        // Bounded well inside the controller's 17 s screenshot wait.
        var timeout = timeval(tv_sec: 5, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, socklen_t(MemoryLayout<Int32>.size))
        let flags = fcntl(fd, F_GETFL)
        _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK)
        var connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
        }
        if connected != 0 && errno == EINPROGRESS {
            var descriptor = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
            var error: Int32 = 0
            var length = socklen_t(MemoryLayout<Int32>.size)
            if poll(&descriptor, 1, 5000) == 1, getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) == 0, error == 0 { connected = 0 }
        }
        _ = fcntl(fd, F_SETFL, flags)
        guard connected == 0 else {
            close(fd)
            throw ScreenSharingError(code: "screen_sharing_unavailable", message: "Guest Screen Sharing (port 5900) is not reachable; enable it in the Guest (System Settings > General > Sharing)")
        }
        do { try handshake(user: user, password: password) } catch { close(fd); throw error }
    }
    deinit { close(fd) }

    func read(_ count: Int) throws -> [UInt8] {
        var out = [UInt8](repeating: 0, count: count)
        var offset = 0
        while offset < count {
            let n = out.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress! + offset, count - offset) }
            if n < 0 && errno == EINTR { continue }
            guard n > 0 else { throw ScreenSharingError(code: "screen_sharing_disconnected", message: "Screen Sharing connection closed") }
            offset += n
        }
        return out
    }
    func u16(_ b: [UInt8], _ at: Int) -> Int { Int(b[at]) << 8 | Int(b[at + 1]) }
    func u32(_ b: [UInt8], _ at: Int) -> UInt32 { UInt32(b[at]) << 24 | UInt32(b[at + 1]) << 16 | UInt32(b[at + 2]) << 8 | UInt32(b[at + 3]) }
    func write(_ bytes: [UInt8]) throws {
        var offset = 0
        while offset < bytes.count {
            let n = bytes.withUnsafeBytes { Darwin.write(fd, $0.baseAddress! + offset, bytes.count - offset) }
            if n < 0 && errno == EINTR { continue }
            guard n > 0 else { throw ScreenSharingError(code: "screen_sharing_disconnected", message: "Screen Sharing write failed") }
            offset += n
        }
    }
    static func be16(_ v: Int) -> [UInt8] { [UInt8(truncatingIfNeeded: v >> 8), UInt8(truncatingIfNeeded: v)] }
    static func be32(_ v: Int32) -> [UInt8] { let u = UInt32(bitPattern: v); return [UInt8(u >> 24), UInt8(truncatingIfNeeded: u >> 16), UInt8(truncatingIfNeeded: u >> 8), UInt8(truncatingIfNeeded: u)] }

    private func handshake(user: String, password: String) throws {
        let version = try read(12)
        guard String(bytes: version, encoding: .ascii)?.hasPrefix("RFB 003.") == true else { throw ScreenSharingError(code: "screen_sharing_protocol", message: "not an RFB server") }
        try write(Array("RFB 003.008\n".utf8))
        let count = Int(try read(1)[0])
        guard count > 0 else {
            let length = Int(u32(try read(4), 0))
            throw ScreenSharingError(code: "screen_sharing_refused", message: String(decoding: try read(min(length, 4096)), as: UTF8.self))
        }
        let types = try read(count)
        guard types.contains(30) else { throw ScreenSharingError(code: "screen_sharing_auth_unsupported", message: "Screen Sharing offers security types \(types), not Apple authentication (30)") }
        try write([30])
        let header = try read(4)
        let generator = Array(header[0..<2])
        let keyLength = u16(header, 2)
        guard (16...512).contains(keyLength) else { throw ScreenSharingError(code: "screen_sharing_protocol", message: "invalid DH key length") }
        let prime = try read(keyLength)
        let serverPublic = try read(keyLength)
        let modulus = Modulus(bigEndian: prime)
        let secret = randomBytes(keyLength)
        let clientPublic = modulus.power(generator, secret)
        let shared = modulus.power(serverPublic, secret)
        let key = Array(Insecure.MD5.hash(data: Data(shared.suffix(keyLength))))
        var credentials = randomBytes(128)
        for (offset, field) in [(0, user), (64, password)] {
            let bytes = Array(field.utf8)
            guard bytes.count < 64 else { throw ScreenSharingError(code: "screen_sharing_auth", message: "login name or password exceeds 63 bytes") }
            credentials.replaceSubrange(offset..<offset + bytes.count + 1, with: bytes + [0])
        }
        var encrypted = [UInt8](repeating: 0, count: 128)
        var moved = 0
        let status = CCCrypt(CCOperation(kCCEncrypt), CCAlgorithm(kCCAlgorithmAES128), CCOptions(kCCOptionECBMode), key, 16, nil, credentials, 128, &encrypted, 128, &moved)
        guard status == kCCSuccess, moved == 128 else { throw ScreenSharingError(code: "screen_sharing_auth", message: "credential encryption failed") }
        try write(encrypted + Array(clientPublic.suffix(keyLength)))
        guard u32(try read(4), 0) == 0 else { throw ScreenSharingError(code: "screen_sharing_auth", message: "Screen Sharing rejected the Guest login") }
        try write([1]) // shared session: the native window and other viewers stay connected
        let initial = try read(24)
        width = u16(initial, 0); height = u16(initial, 2)
        _ = try read(Int(min(u32(initial, 20), 4096)))
        // 32-bit little-endian true colour, red<<16 green<<8 blue: BGRA in memory.
        try write([0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])
        try write([2, 0] + RFBClient.be16(2) + RFBClient.be32(0) + RFBClient.be32(-223))
        framebuffer = [UInt8](repeating: 0, count: width * height * 4)
    }

    /// Full non-incremental framebuffer read (raw encoding).
    func capture() throws -> (width: Int, height: Int, pixels: [UInt8]) {
        try write([3, 0, 0, 0, 0, 0] + RFBClient.be16(width) + RFBClient.be16(height))
        var covered = 0
        let deadline = Date().addingTimeInterval(10)
        while covered < width * height {
            guard Date() < deadline else { throw ScreenSharingError(code: "screen_sharing_timeout", message: "framebuffer update incomplete") }
            let type = try read(1)[0]
            switch type {
            case 0:
                let header = try read(3)
                for _ in 0..<u16(header, 1) {
                    let r = try read(12)
                    let (x, y, w, h) = (u16(r, 0), u16(r, 2), u16(r, 4), u16(r, 6))
                    let encoding = Int32(bitPattern: u32(r, 8))
                    if encoding == -223 {
                        width = w; height = h; covered = 0
                        framebuffer = [UInt8](repeating: 0, count: w * h * 4)
                        try write([3, 0, 0, 0, 0, 0] + RFBClient.be16(w) + RFBClient.be16(h))
                        continue
                    }
                    guard encoding == 0, x + w <= width, y + h <= height else { throw ScreenSharingError(code: "screen_sharing_protocol", message: "unexpected encoding \(encoding)") }
                    let data = try read(w * h * 4)
                    for row in 0..<h {
                        let target = ((y + row) * width + x) * 4
                        framebuffer.replaceSubrange(target..<target + w * 4, with: data[(row * w * 4)..<((row + 1) * w * 4)])
                    }
                    covered += w * h
                }
            case 1:
                let header = try read(5)
                _ = try read(u16(header, 3) * 6)
            case 2: break
            case 3:
                let header = try read(7)
                _ = try read(Int(min(u32(header, 3), 1 << 20)))
            default: throw ScreenSharingError(code: "screen_sharing_protocol", message: "unexpected server message \(type)")
            }
        }
        return (width, height, framebuffer)
    }
    /// Whether the server has something to read within `milliseconds`.
    func readable(milliseconds: Int32) -> Bool {
        var descriptor = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
        return poll(&descriptor, 1, milliseconds) > 0
    }
    func requestUpdate(incremental: Bool) throws {
        try write([3, incremental ? 1 : 0, 0, 0, 0, 0] + RFBClient.be16(width) + RFBClient.be16(height))
    }
    /// One server message, for the browser desktop. A raw rectangle reaches `rows`
    /// in bands of at most `band` rows (BGRA, packed); a desktop-size change calls
    /// `resized` after the new size is in `width`/`height`. True when the message
    /// was a framebuffer update, which ends a frame.
    ///
    /// `early` runs once per update, as soon as its first pixel rectangle's header is
    /// read: the next incremental request goes out while this update's pixels are
    /// still being read and forwarded, so the server prepares the next frame in
    /// parallel instead of after the whole transfer.
    func stream(band: Int, early: () throws -> Void, rows: (Int, Int, Int, Int, [UInt8]) throws -> Void, resized: () throws -> Void) throws -> Bool {
        switch try read(1)[0] {
        case 0:
            let header = try read(3)
            var requested = false
            for _ in 0..<u16(header, 1) {
                let r = try read(12)
                let (x, y, w, h) = (u16(r, 0), u16(r, 2), u16(r, 4), u16(r, 6))
                let encoding = Int32(bitPattern: u32(r, 8))
                if encoding == -223 {
                    width = w; height = h
                    try resized()
                    continue
                }
                guard encoding == 0, x + w <= width, y + h <= height else { throw ScreenSharingError(code: "screen_sharing_protocol", message: "unexpected encoding \(encoding)") }
                if !requested { requested = true; try early() }
                var done = 0
                while done < h {
                    let count = min(band, h - done)
                    try rows(x, y + done, w, count, try read(w * count * 4))
                    done += count
                }
            }
            return true
        case 1:
            let header = try read(5)
            _ = try read(u16(header, 3) * 6)
        case 2: break
        case 3:
            let header = try read(7)
            _ = try read(Int(min(u32(header, 3), 1 << 20)))
        default: throw ScreenSharingError(code: "screen_sharing_protocol", message: "unexpected server message")
        }
        return false
    }
    func pointer(x: Int, y: Int, mask: UInt8) throws { try write([5, mask] + RFBClient.be16(x) + RFBClient.be16(y)) }
    func key(_ keysym: UInt32, down: Bool) throws {
        try write([4, down ? 1 : 0, 0, 0, UInt8(keysym >> 24), UInt8(truncatingIfNeeded: keysym >> 16), UInt8(truncatingIfNeeded: keysym >> 8), UInt8(truncatingIfNeeded: keysym)])
    }
}

/// QEMU qcode names (crates/control/src/input.rs) → X keysyms. Physical keys,
/// no text or layout translation; meta is Command on the Apple server.
let screenSharingKeysyms: [String: UInt32] = {
    var map: [String: UInt32] = [:]
    for c in UInt32(0x61)...0x7a { map[String(UnicodeScalar(c)!)] = c }
    for c in UInt32(0x30)...0x39 { map[String(UnicodeScalar(c)!)] = c }
    for (i, n) in (1...12).enumerated() { map["f\(n)"] = 0xffbe + UInt32(i) }
    let named: [String: UInt32] = ["ret": 0xff0d, "esc": 0xff1b, "tab": 0xff09, "spc": 0x20, "backspace": 0xff08, "delete": 0xffff,
        "insert": 0xff63, "home": 0xff50, "end": 0xff57, "pgup": 0xff55, "pgdn": 0xff56, "up": 0xff52, "down": 0xff54, "left": 0xff51,
        "right": 0xff53, "shift": 0xffe1, "shift_r": 0xffe2, "ctrl": 0xffe3, "ctrl_r": 0xffe4, "alt": 0xffe9, "alt_r": 0xffea,
        "meta_l": 0xffe7, "meta_r": 0xffe8, "minus": 0x2d, "equal": 0x3d, "bracket_left": 0x5b, "bracket_right": 0x5d,
        "semicolon": 0x3b, "apostrophe": 0x27, "grave_accent": 0x60, "backslash": 0x5c, "comma": 0x2c, "dot": 0x2e, "slash": 0x2f]
    map.merge(named) { $1 }
    return map
}()
let screenSharingButtons: [String: UInt8] = ["left": 1, "middle": 2, "right": 4, "wheel-up": 8, "wheel-down": 16, "wheel-left": 32, "wheel-right": 64]

final class ScreenSharingGuest {
    let mac: String
    let loginFile: String
    let queue = DispatchQueue(label: "veynode.screen-sharing")
    private var client: RFBClient?
    private struct Frame { let id: String; let generation: UInt64; let width: Int; let height: Int; let captured: UInt64 }
    private var frames: [Frame] = []
    private var generation: UInt64 = 0
    private var geometry: (Int, Int)?
    private var operation: String?
    private var deadline: UInt64 = 0
    private var keys: Set<String> = []
    private var mask: UInt8 = 0
    private var location: (Int, Int)?
    private var positioned = false

    init(mac: String, loginFile: String) { self.mac = mac; self.loginFile = loginFile }
    static func handles(_ method: String) -> Bool { ["info", "screenshot", "input_step", "input_cleanup"].contains(method) }
    func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

    func session() throws -> RFBClient {
        if let client { return client }
        let connected = try connect()
        client = connected
        return connected
    }
    /// A new Screen Sharing connection with the Guest login; the browser desktop keeps its own.
    func connect() throws -> RFBClient {
        guard let address = dhcpLeaseAddress(mac: mac) else { throw ScreenSharingError(code: "guest_unavailable", message: "Guest has no DHCP lease yet") }
        guard let text = try? String(contentsOfFile: loginFile, encoding: .utf8) else { throw ScreenSharingError(code: "screen_sharing_auth", message: "Guest login file unreadable") }
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        guard lines.count >= 2 else { throw ScreenSharingError(code: "screen_sharing_auth", message: "Guest login file must contain user and password lines") }
        return try RFBClient(host: address, user: String(lines[0]), password: String(lines[1]))
    }
    /// One reconnect on a dropped session; authentication errors are final.
    func withSession<T>(_ body: (RFBClient) throws -> T) throws -> T {
        do { return try body(try session()) } catch let error as ScreenSharingError where error.code == "screen_sharing_disconnected" {
            client = nil
            return try body(try session())
        }
    }

    func handle(_ request: [String: Any], reply: @escaping ([String: Any]) -> Void) {
        queue.async {
            let id = request["id"] ?? NSNull()
            do {
                let result: [String: Any]
                switch request["method"] as? String {
                case "info": result = self.info()
                case "screenshot": result = try self.screenshot()
                case "input_step": result = try self.step(try self.arguments(request))
                case "input_cleanup": result = try self.cleanup(try self.arguments(request))
                default: throw ScreenSharingError(code: "unsupported", message: "method unsupported over Screen Sharing")
                }
                reply(["id": id, "result": result])
            } catch let error as ScreenSharingError {
                if error.code.hasPrefix("screen_sharing_") { self.client = nil }
                reply(["id": id, "error": ["code": error.code, "message": error.message]])
            } catch {
                self.client = nil
                reply(["id": id, "error": ["code": "screen_sharing_failed", "message": error.localizedDescription]])
            }
        }
    }

    /// Input requests carry their fields under "arguments", as for the Guest service.
    func arguments(_ request: [String: Any]) throws -> [String: Any] {
        guard Set(request.keys) == ["id", "method", "arguments"], let args = request["arguments"] as? [String: Any] else {
            throw ScreenSharingError(code: "invalid_request", message: "input requires id, method, arguments")
        }
        return args
    }
    func info() -> [String: Any] {
        ["protocol_version": 1, "os": "darwin", "arch": "arm64", "transport": "screen-sharing-rfb",
         "capabilities": ["exec": false, "async_exec": false, "shutdown": false, "desktop": true, "input": true, "screenshot": true, "files": false],
         "input_policy": ["enabled": true, "available": true, "api": "RFB PointerEvent/KeyEvent (Guest Screen Sharing)", "max_step_events": 16, "max_operation_ms": 4000,
                          "physical_keys": screenSharingKeysyms.keys.sorted(), "text_input": false, "delivery_confirmation": false],
         "observation": ["screenshot_available": true, "source": "Guest Screen Sharing framebuffer; no Guest TCC permission required"]]
    }

    func screenshot() throws -> [String: Any] {
        let requested = Date()
        let start = now()
        let frame = try withSession { try $0.capture() }
        let end = now()
        if geometry == nil || geometry! != (frame.width, frame.height) {
            release()
            frames.removeAll()
            generation += 1
            geometry = (frame.width, frame.height)
        }
        guard let provider = CGDataProvider(data: Data(frame.pixels) as CFData),
              let image = CGImage(width: frame.width, height: frame.height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: frame.width * 4,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGBitmapInfo(rawValue: CGBitmapInfo.byteOrder32Little.rawValue | CGImageAlphaInfo.noneSkipFirst.rawValue),
                                  provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent) else {
            throw ScreenSharingError(code: "screenshot_failed", message: "framebuffer conversion failed")
        }
        var jpeg = Data(), used = 0.0
        for quality in [0.8, 0.65, 0.5, 0.35] {
            let output = NSMutableData()
            guard let destination = CGImageDestinationCreateWithData(output, "public.jpeg" as CFString, 1, nil) else { break }
            CGImageDestinationAddImage(destination, image, [kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary)
            guard CGImageDestinationFinalize(destination) else { break }
            jpeg = output as Data; used = quality
            if jpeg.count <= 700 * 1024 { break }
        }
        guard !jpeg.isEmpty, jpeg.count <= 700 * 1024 else { throw ScreenSharingError(code: "screenshot_failed", message: "JPEG exceeds 700 KiB") }
        let id = UUID().uuidString.lowercased()
        frames.append(Frame(id: id, generation: generation, width: frame.width, height: frame.height, captured: end))
        if frames.count > 16 { frames.removeFirst() }
        return ["frame_id": id, "mime_type": "image/jpeg", "data_base64": jpeg.base64EncodedString(), "byte_length": jpeg.count,
                "sha256": SHA256.hash(data: jpeg).map { String(format: "%02x", $0) }.joined(), "width": frame.width, "height": frame.height,
                "source_width": frame.width, "source_height": frame.height, "image_width": frame.width, "image_height": frame.height,
                "coordinate_space": "guest-display-physical-pixels", "source_generation": generation, "input_anchor_available": true,
                "capture_requested_at_us": Int64(requested.timeIntervalSince1970 * 1_000_000), "captured_at_us": Int64(Date().timeIntervalSince1970 * 1_000_000),
                "captured_monotonic_us": end / 1000, "capture_duration_ms": Double(end - start) / 1_000_000, "jpeg_quality": used,
                "includes_cursor": false, "capture_source": "guest-screen-sharing-rfb", "resized": false]
    }

    private func hex(_ value: Any?, _ count: Int) -> String? {
        guard let s = value as? String, s.count == count, s.allSatisfy({ $0.isHexDigit }) else { return nil }
        return s
    }
    private func unsigned(_ value: Any?) -> UInt64? {
        guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID(), n.doubleValue >= 0, n.doubleValue <= 9_007_199_254_740_991,
              n.doubleValue == n.doubleValue.rounded() else { return nil }
        return n.uint64Value
    }
    /// Release everything this operation holds (key-ups, then all buttons up).
    @discardableResult func release() -> Bool {
        defer { operation = nil; keys.removeAll(); mask = 0; positioned = false }
        guard let client, operation != nil || !keys.isEmpty || mask != 0 else { return true }
        do {
            for key in keys { if let sym = screenSharingKeysyms[key] { try client.key(sym, down: false) } }
            if let location { try client.pointer(x: location.0, y: location.1, mask: 0) }
            return true
        } catch { self.client = nil; return false }
    }
    func cleanup(_ args: [String: Any]) throws -> [String: Any] {
        guard let op = hex(args["operation_id"], 32) else { throw ScreenSharingError(code: "invalid_request", message: "operation_id required for cleanup") }
        guard operation == op else { return ["released": false, "reason": "operation is no longer active"] }
        guard release() else { throw ScreenSharingError(code: "input_cleanup_unconfirmed", message: "synthetic release could not be sent") }
        return ["released": true, "synthetic_release_dispatched": true, "guest_delivery_confirmed": false]
    }
    func step(_ args: [String: Any]) throws -> [String: Any] {
        guard let op = hex(args["operation_id"], 32), let begin = args["begin"] as? Bool, let frameID = args["frame_id"] as? String,
              let requestedGeneration = unsigned(args["source_generation"]), let width = unsigned(args["width"]), let height = unsigned(args["height"]),
              let events = args["events"] as? [[String: Any]], !events.isEmpty, events.count <= 16 else {
            throw ScreenSharingError(code: "invalid_request", message: "Invalid bounded physical input request")
        }
        guard let frame = frames.first(where: { $0.id == frameID }), now() - frame.captured <= 5_000_000_000, frame.generation == requestedGeneration,
              UInt64(frame.width) == width, UInt64(frame.height) == height, let client, client.width == frame.width, client.height == frame.height else {
            throw ScreenSharingError(code: "stale_frame", message: "Capture a fresh screenshot after display or geometry changes")
        }
        guard begin ? operation == nil : (operation == op && now() < deadline) else {
            throw ScreenSharingError(code: "input_busy_or_expired", message: "Input operation is active, replaced, or expired")
        }
        // Validate the whole batch before sending any element.
        enum Plan { case move(Int, Int), button(UInt8), key(UInt32, Bool, String) }
        var plans: [Plan] = [], nextKeys = keys, nextMask = mask
        let hasPoint = !begin && positioned
        if events.contains(where: { $0["type"] as? String == "abs" }) {
            var axes: [String: UInt64] = [:]
            for event in events {
                guard event["type"] as? String == "abs", let d = event["data"] as? [String: Any], let axis = d["axis"] as? String, ["x", "y"].contains(axis),
                      axes[axis] == nil, let v = unsigned(d["value"]), v < 32768 else {
                    throw ScreenSharingError(code: "invalid_request", message: "Absolute x/y coordinates must be paired and normalized")
                }
                axes[axis] = v
            }
            guard events.count == 2, let x = axes["x"], let y = axes["y"] else { throw ScreenSharingError(code: "invalid_request", message: "Both axes required") }
            plans.append(.move(Int(x * width / 32768), Int(y * height / 32768)))
        } else {
            for event in events {
                guard let type = event["type"] as? String, let d = event["data"] as? [String: Any], let down = d["down"] as? Bool else {
                    throw ScreenSharingError(code: "invalid_request", message: "Invalid physical input event")
                }
                if type == "key" {
                    guard let key = d["key"] as? [String: Any], key["type"] as? String == "qcode", let name = key["data"] as? String,
                          let sym = screenSharingKeysyms[name] else {
                        throw ScreenSharingError(code: "unsupported_key", message: "Only supported physical keys are accepted; no Unicode/layout translation")
                    }
                    if down { guard nextKeys.insert(name).inserted else { throw ScreenSharingError(code: "invalid_request", message: "Key already pressed") } }
                    else { guard nextKeys.remove(name) != nil else { throw ScreenSharingError(code: "invalid_request", message: "Key is not held by this operation") } }
                    plans.append(.key(sym, down, name))
                } else if type == "btn", let name = d["button"] as? String, let bit = screenSharingButtons[name] {
                    guard hasPoint else { throw ScreenSharingError(code: "invalid_request", message: "Mouse action requires absolute position in this operation") }
                    if bit >= 8 {
                        if down { plans.append(.button(nextMask | bit)); plans.append(.button(nextMask)) }
                    } else {
                        if down { guard nextMask & bit == 0 else { throw ScreenSharingError(code: "invalid_request", message: "Button already pressed") }; nextMask |= bit }
                        else { guard nextMask & bit != 0 else { throw ScreenSharingError(code: "invalid_request", message: "Button is not held by this operation") }; nextMask &= ~bit }
                        plans.append(.button(nextMask))
                    }
                } else {
                    throw ScreenSharingError(code: "invalid_request", message: "Unsupported input event")
                }
            }
        }
        if begin { positioned = false; operation = op; deadline = now() + 4_000_000_000 }
        do {
            for plan in plans {
                switch plan {
                case .move(let x, let y):
                    try client.pointer(x: x, y: y, mask: mask); location = (x, y); positioned = true
                case .button(let next):
                    guard let location else { continue }
                    try client.pointer(x: location.0, y: location.1, mask: next); mask = next
                case .key(let sym, let down, let name):
                    try client.key(sym, down: down)
                    if down { keys.insert(name) } else { keys.remove(name) }
                }
            }
        } catch {
            if !release() { throw ScreenSharingError(code: "input_cleanup_unconfirmed", message: "Input was interrupted and release could not be sent") }
            throw error
        }
        return ["dispatched": true, "source_generation": frame.generation, "guest_delivery_confirmed": false, "coordinate_space": "captured-display-pixels"]
    }
}


/// Linux evdev key codes (what the browser desktop sends) → X keysyms. Apple's
/// Screen Sharing server takes Alt_L/Alt_R as Command (a chord with Alt_L clears
/// Terminal with Command+K); Meta_L/Meta_R and Super do nothing, and no keysym
/// reached Option, so the browser's Meta (Command) keys send Alt and its Alt keys Meta.
let screenSharingEvdev: [Int: UInt32] = {
    var map: [Int: UInt32] = [1: 0xff1b, 12: 0x2d, 13: 0x3d, 14: 0xff08, 15: 0xff09, 26: 0x5b, 27: 0x5d, 28: 0xff0d, 29: 0xffe3,
        39: 0x3b, 40: 0x27, 41: 0x60, 42: 0xffe1, 43: 0x5c, 51: 0x2c, 52: 0x2e, 53: 0x2f, 54: 0xffe2, 55: 0xffaa, 56: 0xffe7,
        57: 0x20, 58: 0xffe5, 69: 0xff7f, 70: 0xff14, 74: 0xffad, 78: 0xffab, 83: 0xffae, 96: 0xff8d, 97: 0xffe4, 98: 0xffaf,
        100: 0xffe8, 102: 0xff50, 103: 0xff52, 104: 0xff55, 105: 0xff51, 106: 0xff53, 107: 0xff57, 108: 0xff54, 109: 0xff56,
        110: 0xff63, 111: 0xffff, 125: 0xffe9, 126: 0xffea, 127: 0xff67]
    for (i, c) in "1234567890".unicodeScalars.enumerated() { map[2 + i] = c.value }
    for (i, c) in "qwertyuiop".unicodeScalars.enumerated() { map[16 + i] = c.value }
    for (i, c) in "asdfghjkl".unicodeScalars.enumerated() { map[30 + i] = c.value }
    for (i, c) in "zxcvbnm".unicodeScalars.enumerated() { map[44 + i] = c.value }
    for i in 0..<10 { map[59 + i] = 0xffbe + UInt32(i) }
    map[87] = 0xffc8; map[88] = 0xffc9
    for (i, code) in [71, 72, 73].enumerated() { map[code] = 0xffb7 + UInt32(i) }
    for (i, code) in [75, 76, 77].enumerated() { map[code] = 0xffb4 + UInt32(i) }
    for (i, code) in [79, 80, 81].enumerated() { map[code] = 0xffb1 + UInt32(i) }
    map[82] = 0xffb0
    return map
}()

/// Browser desktop for a macOS Guest: the Guest's Screen Sharing server, read
/// with incremental raw updates, is turned into the record protocol the Linux
/// guest agent speaks on vsock 5001 (see guest/veynode-agent/desktop_linux.go),
/// so the same bridge and `vz-desktop-stream` encoder serve both. The worker
/// end of a socketpair stands in for the vsock connection.
///   host -> here: JSON lines {"method":"start"} then {"input":[...]}
///   here -> host: u32 BE length, u8 kind, body (1 event, 2 rectangle, 3 frame end)
final class MacDesktopStream {
    private let fd: Int32
    private let connect: () throws -> RFBClient
    private let output = NSLock()
    private let rfbWrites = NSLock()
    private let state = NSLock()
    private var ended = false
    private var client: RFBClient?
    private var location = (x: 0, y: 0)
    private var mask: UInt8 = 0
    private var held = Set<UInt32>()
    private let epoch = DispatchTime.now().uptimeNanoseconds
    // Every 5 s a stats event: frames, megabytes and the time spent writing to the bridge.
    private var writeNanoseconds: UInt64 = 0
    private var sentBytes: UInt64 = 0

    private init(fd: Int32, connect: @escaping () throws -> RFBClient) { self.fd = fd; self.connect = connect }

    /// Takes ownership of `fd` and serves it on its own threads.
    static func start(fd: Int32, connect: @escaping () throws -> RFBClient) {
        var noSignal: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, socklen_t(MemoryLayout<Int32>.size))
        let stream = MacDesktopStream(fd: fd, connect: connect)
        Thread.detachNewThread { stream.run() }
    }

    private var finished: Bool { state.lock(); defer { state.unlock() }; return ended }
    private func finish() { state.lock(); ended = true; state.unlock() }

    private func writeAll(_ bytes: UnsafeRawBufferPointer) throws {
        var offset = 0
        while offset < bytes.count {
            let n = Darwin.write(fd, bytes.baseAddress! + offset, bytes.count - offset)
            if n < 0 && errno == EINTR { continue }
            guard n > 0 else { throw ScreenSharingError(code: "stream_closed", message: "desktop stream closed") }
            offset += n
        }
    }
    private func record(_ kind: UInt8, _ head: [UInt8], _ body: [UInt8] = []) throws {
        let length = UInt32(1 + head.count + body.count)
        let prefix: [UInt8] = [UInt8(length >> 24), UInt8(truncatingIfNeeded: length >> 16), UInt8(truncatingIfNeeded: length >> 8), UInt8(truncatingIfNeeded: length), kind] + head
        output.lock(); defer { output.unlock() }
        let began = DispatchTime.now().uptimeNanoseconds
        defer { writeNanoseconds += DispatchTime.now().uptimeNanoseconds - began }
        try prefix.withUnsafeBytes { try writeAll($0) }
        if !body.isEmpty { try body.withUnsafeBytes { try writeAll($0) } }
        sentBytes += UInt64(prefix.count + body.count)
    }
    private func event(_ fields: [String: Any]) throws {
        try record(1, [UInt8](try JSONSerialization.data(withJSONObject: fields)))
    }
    private func mode(_ client: RFBClient) throws {
        try event(["event": "mode", "width": client.width, "height": client.height, "format": "bgrx"])
    }

    /// One JSON line of at most 4 KiB from the host, or nil on EOF, timeout or overflow.
    private func line(timeoutMilliseconds: Int32?, pending: inout [UInt8]) -> [UInt8]? {
        while true {
            if let newline = pending.firstIndex(of: 10) {
                let found = Array(pending[..<newline]); pending.removeSubrange(...newline); return found
            }
            guard pending.count < 4096 else { return nil }
            if let timeoutMilliseconds {
                var descriptor = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
                guard poll(&descriptor, 1, timeoutMilliseconds) > 0 else { return nil }
            }
            var buffer = [UInt8](repeating: 0, count: 4096)
            let n = Darwin.read(fd, &buffer, buffer.count)
            if n < 0 && errno == EINTR { continue }
            guard n > 0 else { return nil }
            pending += buffer[..<n]
        }
    }

    private func run() {
        let inputDone = DispatchSemaphore(value: 0)
        var inputStarted = false
        defer {
            finish()
            releaseInput()
            shutdown(fd, SHUT_RDWR)
            if inputStarted { _ = inputDone.wait(timeout: .now() + 1) }
            Darwin.close(fd)
        }
        var pending: [UInt8] = []
        guard let start = line(timeoutMilliseconds: 5000, pending: &pending),
              (try? JSONSerialization.jsonObject(with: Data(start)) as? [String: Any])?["method"] as? String == "start" else {
            try? event(["event": "error", "message": "expected start"]); return
        }
        let rfb: RFBClient
        do { rfb = try connect() } catch {
            try? event(["event": "error", "message": (error as? ScreenSharingError)?.message ?? error.localizedDescription]); return
        }
        client = rfb
        inputStarted = true
        let leftover = pending
        Thread.detachNewThread { self.readInput(initial: leftover); inputDone.signal() }
        do {
            try mode(rfb)
            try locked { try rfb.requestUpdate(incremental: false) }
            var frames = 0, reported = DispatchTime.now().uptimeNanoseconds
            while !finished {
                let now = DispatchTime.now().uptimeNanoseconds
                if now - reported >= 5_000_000_000 {
                    try event(["event": "stats", "frames": frames, "mb": Double(sentBytes) / 1e6, "write_ms": Double(writeNanoseconds) / 1e6,
                               "seconds": Double(now - reported) / 1e9])
                    frames = 0; sentBytes = 0; writeNanoseconds = 0; reported = now
                }
                guard rfb.readable(milliseconds: 200) else { continue }
                var resized = false
                var requestedEarly = false
                let frame = try rfb.stream(band: 64, early: {
                    requestedEarly = true
                    try locked { try rfb.requestUpdate(incremental: true) }
                }, rows: { x, y, w, h, data in
                    try record(2, RFBClient.be16(x) + RFBClient.be16(y) + RFBClient.be16(w) + RFBClient.be16(h), data)
                }, resized: { resized = true; try mode(rfb) })
                guard frame || resized else { continue }
                frames += 1
                let stamp = (DispatchTime.now().uptimeNanoseconds - epoch) / 1000
                try record(3, (0..<8).map { UInt8(truncatingIfNeeded: stamp >> UInt64(56 - 8 * $0)) })
                if resized || !requestedEarly { try locked { try rfb.requestUpdate(incremental: !resized) } }
            }
        } catch {
            if !finished { try? event(["event": "error", "message": (error as? ScreenSharingError)?.message ?? error.localizedDescription]) }
        }
    }

    private func locked<T>(_ body: () throws -> T) rethrows -> T { rfbWrites.lock(); defer { rfbWrites.unlock() }; return try body() }

    private func readInput(initial: [UInt8]) {
        var pending = initial
        while !finished, let bytes = line(timeoutMilliseconds: nil, pending: &pending) {
            guard let message = (try? JSONSerialization.jsonObject(with: Data(bytes))) as? [String: Any], let events = message["input"] as? [[String: Any]] else { continue }
            do { try locked { try apply(events) } } catch {
                try? event(["event": "input_error", "message": (error as? ScreenSharingError)?.message ?? error.localizedDescription])
            }
        }
        finish()
    }

    /// {"t":"abs","x","y"} 0..32767, {"t":"key","code":evdev,"down"} (BTN_LEFT..BTN_MIDDLE are the
    /// buttons) and {"t":"wheel","dx","dy"}; same shapes as the Linux agent's.
    private func apply(_ events: [[String: Any]]) throws {
        guard let client else { return }
        for event in events {
            switch event["t"] as? String {
            case "abs":
                guard let x = event["x"] as? Int, let y = event["y"] as? Int, (0..<32768).contains(x), (0..<32768).contains(y) else {
                    throw ScreenSharingError(code: "invalid_request", message: "abs outside 0..32767")
                }
                location = (x * client.width / 32768, y * client.height / 32768)
                try client.pointer(x: location.x, y: location.y, mask: mask)
            case "key":
                guard let code = event["code"] as? Int, let down = event["down"] as? Bool else { throw ScreenSharingError(code: "invalid_request", message: "invalid key event") }
                if let bit = [0x110: UInt8(1), 0x111: 4, 0x112: 2][code] {
                    mask = down ? mask | bit : mask & ~bit
                    try client.pointer(x: location.x, y: location.y, mask: mask)
                } else if let symbol = screenSharingEvdev[code] {
                    if down { held.insert(symbol) } else { held.remove(symbol) }
                    try client.key(symbol, down: down)
                } else {
                    throw ScreenSharingError(code: "unsupported_key", message: "key code \(code) has no macOS mapping")
                }
            case "wheel":
                guard let dx = event["dx"] as? Int, let dy = event["dy"] as? Int, abs(dx) <= 10, abs(dy) <= 10 else {
                    throw ScreenSharingError(code: "invalid_request", message: "wheel outside bound")
                }
                // Browser dy > 0 scrolls down; the RFB wheel buttons are 4/5 (8, 16) up and down, 6/7 (32, 64) left and right.
                for (count, negative, positive) in [(dy, UInt8(8), UInt8(16)), (dx, UInt8(32), UInt8(64))] where count != 0 {
                    for _ in 0..<abs(count) {
                        try client.pointer(x: location.x, y: location.y, mask: mask | (count < 0 ? negative : positive))
                        try client.pointer(x: location.x, y: location.y, mask: mask)
                    }
                }
            default:
                throw ScreenSharingError(code: "invalid_request", message: "unknown input type")
            }
        }
    }

    /// A closed viewer must not leave keys or buttons down in the Guest.
    private func releaseInput() {
        locked {
            guard let client else { return }
            for symbol in held { try? client.key(symbol, down: false) }
            if mask != 0 { try? client.pointer(x: location.x, y: location.y, mask: 0) }
            held.removeAll(); mask = 0
        }
    }
}
