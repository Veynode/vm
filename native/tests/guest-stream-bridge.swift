import Foundation
import Darwin

struct TestFailure: Error { let message: String }
func check(_ value: @autoclosure () throws -> Bool, _ message: String) throws {
    if try !value() { throw TestFailure(message: message) }
}
func socketPair() throws -> (Int32, Int32) {
    var fds: [Int32] = [-1, -1]
    try check(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0, "socketpair")
    for fd in fds {
        var one: Int32 = 1; _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, 4)
        var timeout = timeval(tv_sec: 3, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
    }
    return (fds[0], fds[1])
}
func connectLocal(_ path: String) throws -> Int32 {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    var address = sockaddr_un(); address.sun_family = sa_family_t(AF_UNIX); address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
    withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: Array(path.utf8) + [0]) }
    let connected = withUnsafePointer(to: &address) { pointer in pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } }
    if connected != 0 { close(fd); throw TestFailure(message: "connectLocal errno \(errno)") }
    var one: Int32 = 1; _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, 4)
    var timeout = timeval(tv_sec: 3, tv_usec: 0)
    _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
    return fd
}
func writeAll(_ fd: Int32, _ bytes: Data) throws {
    try bytes.withUnsafeBytes { raw in
        var offset = 0
        while offset < raw.count {
            let count = write(fd, raw.baseAddress!.advanced(by: offset), raw.count - offset)
            if count < 0 && errno == EINTR { continue }
            try check(count > 0, "write errno \(errno)"); offset += count
        }
    }
}
func readExact(_ fd: Int32, _ amount: Int) throws -> Data {
    var result = Data(), buffer = [UInt8](repeating: 0, count: 8192)
    while result.count < amount {
        let count = read(fd, &buffer, min(buffer.count, amount - result.count))
        if count < 0 && errno == EINTR { continue }
        try check(count > 0, "readExact \(result.count)/\(amount) errno \(errno)")
        result.append(contentsOf: buffer.prefix(count))
    }
    return result
}
func waitMain(_ timeout: TimeInterval = 3, _ predicate: () -> Bool) throws {
    let until = Date().addingTimeInterval(timeout)
    while !predicate() && Date() < until { RunLoop.current.run(until: Date().addingTimeInterval(0.01)) }
    try check(predicate(), "bounded main-loop wait timed out")
}
func stopBridge(_ bridge: GuestStreamBridge) throws {
    var done = false; bridge.stop { done = true }; try waitMain { done }
    try check(!FileManager.default.fileExists(atPath: bridge.socketPath), "socket cleanup")
}
@main struct Tests {
    static func main() {
        do { try run() } catch { fputs("FAIL \(error)\n", stderr); exit(1) }
    }
    static func run() throws {
        let root = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".vb-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: root) }
        func path(_ name: String) -> String { root.appendingPathComponent(name).path }
        // Identity, permissions, no replacement and symlink refusal.
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: root.path)
        do { _ = try GuestStreamBridge(path: path("unsafe"), connector: { _ in }); throw TestFailure(message: "accepted public parent") }
        catch is GuestStreamBridgeError {}
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: root.path)
        try Data("preserve".utf8).write(to: root.appendingPathComponent("existing"))
        do { _ = try GuestStreamBridge(path: path("existing"), connector: { _ in }); throw TestFailure(message: "overwrote file") }
        catch is GuestStreamBridgeError {}
        try check(try Data(contentsOf: root.appendingPathComponent("existing")) == Data("preserve".utf8), "preserved existing file")
        try FileManager.default.createSymbolicLink(atPath: path("link"), withDestinationPath: root.path)
        do { _ = try GuestStreamBridge(path: path("link/sock"), connector: { _ in }); throw TestFailure(message: "accepted symlink parent") }
        catch is GuestStreamBridgeError {}
        print("PASS private path ownership, permissions, existing-file and symlink refusal")

        let guestLock = NSLock(); var guests = [Int32](); var endpointClosed = 0
        let bridge = try GuestStreamBridge(path: path("stream"), connector: { completion in
            do {
                let (owned, peer) = try socketPair()
                guestLock.lock(); guests.append(peer); guestLock.unlock()
                completion(.success(GuestStreamEndpoint(fd: owned, onClose: { guestLock.lock(); endpointClosed += 1; guestLock.unlock() })))
            } catch { completion(.failure(error)) }
        })
        bridge.start()
        let proof=try bridge.binding(vmID:"fixture",workerInstance:String(repeating:"a",count:64),bridgeInstance:String(repeating:"b",count:64))
        try check(proof["vm_id"] as? String == "fixture" && proof["worker_pid"] as? Int32 == getpid(), "binding VM and worker")
        try check(proof["input_authorized"] as? Bool == false && proof["scope"] as? String == "host-worker-listener-only", "binding does not grant input or Guest identity")
        let mode = try FileManager.default.attributesOfItem(atPath: path("stream"))[.posixPermissions] as! NSNumber
        try check(mode.intValue == 0o600, "socket mode0600")
        let client = try connectLocal(path("stream"))
        try waitMain { guestLock.lock(); defer { guestLock.unlock() }; return guests.count == 1 }
        let guest = guests[0]
        let request = Data("{\"method\":\"start\"}\n".utf8)
        try writeAll(client, request); try check(try readExact(guest, request.count) == request, "forward JSON bytes")
        let extra = try connectLocal(path("stream")); var byte: UInt8 = 0
        try check(read(extra, &byte, 1) == 0, "reject concurrent client"); close(extra)
        let payload = Data((0..<(4 * 1024 * 1024)).map { UInt8($0 % 251) })
        let producer = DispatchGroup(); producer.enter()
        let resultLock = NSLock(); var producerError: Error?
        DispatchQueue.global().async {
            do { try writeAll(guest, payload); shutdown(guest, SHUT_WR) }
            catch { resultLock.lock(); producerError = error; resultLock.unlock() }
            producer.leave()
        }
        try check(try readExact(client, payload.count) == payload, "4MiB exact transfer including HUP buffered tail")
        try check(producer.wait(timeout: .now() + 3) == .success, "producer finished")
        resultLock.lock(); let error = producerError; resultLock.unlock(); if let error { throw error }
        try check(read(client, &byte, 1) == 0, "Guest EOF reaches client")
        close(client); close(guest)
        try waitMain { guestLock.lock(); defer { guestLock.unlock() }; return endpointClosed == 1 }
        let second = try connectLocal(path("stream"))
        try waitMain { guestLock.lock(); defer { guestLock.unlock() }; return guests.count == 2 }
        try writeAll(second, request); try check(try readExact(guests[1], request.count) == request, "fresh Guest connection")
        close(second)
        try waitMain { guestLock.lock(); defer { guestLock.unlock() }; return endpointClosed == 2 }
        close(guests[1])
        let third = try connectLocal(path("stream"))
        try waitMain { guestLock.lock(); defer { guestLock.unlock() }; return guests.count == 3 }
        let stopStarted = Date(); try stopBridge(bridge)
        try check(Date().timeIntervalSince(stopStarted) < 0.5, "stop active bridge bounded")
        try check(read(third, &byte, 1) == 0, "stop closes active client")
        try check(read(guests[2], &byte, 1) == 0, "stop closes active Guest endpoint")
        close(third); close(guests[2])
        do {_ = try bridge.binding(vmID:"fixture",workerInstance:String(repeating:"a",count:64),bridgeInstance:String(repeating:"b",count:64)); throw TestFailure(message:"stopped bridge provided provenance")}
        catch is GuestStreamBridgeError {}
        let replaced=try GuestStreamBridge(path:path("replaced"),connector:{_ in})
        replaced.start()
        try FileManager.default.removeItem(atPath:path("replaced"))
        try Data("replacement-preserved".utf8).write(to:URL(fileURLWithPath:path("replaced")))
        do {_ = try replaced.binding(vmID:"fixture",workerInstance:String(repeating:"a",count:64),bridgeInstance:String(repeating:"c",count:64)); throw TestFailure(message:"replaced socket provided provenance")}
        catch is GuestStreamBridgeError {}
        var replacementStopped=false; replaced.stop {replacementStopped=true}; try waitMain {replacementStopped}
        try check(try String(contentsOfFile:path("replaced"),encoding:.utf8)=="replacement-preserved","stop must preserve replaced path")
        print("PASS listener provenance, stopped/replaced socket rejection and replacement preservation")
        print("PASS exact duplex 4MiB, HUP drain, single-client rejection, reconnect and active stop")

        // A consumer that never reads cannot retain an unbounded pending frame.
        var stalledPeer: Int32 = -1; let stallLock = NSLock(); var reason = ""
        let stalled = try GuestStreamBridge(path: path("stalled"), connector: { completion in
            do { let (fd, peer) = try socketPair(); stallLock.lock(); stalledPeer = peer; stallLock.unlock(); completion(.success(GuestStreamEndpoint(fd: fd))) }
            catch { completion(.failure(error)) }
        }, event: { message in stallLock.lock(); reason = message; stallLock.unlock() })
        stalled.start(); let slow = try connectLocal(path("stalled"))
        try waitMain { stallLock.lock(); defer { stallLock.unlock() }; return stalledPeer >= 0 }
        let before = Date(); let peer = stalledPeer
        let stalledProducer = DispatchGroup(); stalledProducer.enter()
        DispatchQueue.global().async { try? writeAll(peer, payload); stalledProducer.leave() }
        try waitMain(2) { stallLock.lock(); defer { stallLock.unlock() }; return !reason.isEmpty }
        try check(reason.contains("write deadline"), "stalled consumer deadline: \(reason)")
        try check(Date().timeIntervalSince(before) < 1.5, "500ms deadline bounded")
        try check(stalledProducer.wait(timeout: .now() + 3) == .success, "stalled producer released")
        close(slow); close(peer); try stopBridge(stalled)
        print("PASS stalled consumer pending-write deadline")

        // stop while connect is outstanding; a late endpoint must be closed once.
        let pendingLock = NSLock(); var callback: ((Result<GuestStreamEndpoint, Error>) -> Void)?; var lateClosed = 0
        let pending = try GuestStreamBridge(path: path("pending"), connector: { cb in pendingLock.lock(); callback = cb; pendingLock.unlock() })
        pending.start(); let waiting = try connectLocal(path("pending"))
        try waitMain { pendingLock.lock(); defer { pendingLock.unlock() }; return callback != nil }
        try stopBridge(pending)
        let (late, remote) = try socketPair()
        callback!(.success(GuestStreamEndpoint(fd: late, onClose: { lateClosed += 1 })))
        try check(lateClosed == 1, "late endpoint closes exactly once")
        try check(read(remote, &byte, 1) == 0, "late descriptor closed")
        close(remote); close(waiting)
        print("PASS stop during pending connection and late-completion cleanup")

        var timedReason = ""; let timedLock = NSLock()
        let timed = try GuestStreamBridge(path: path("timed"), connectTimeoutMilliseconds: 100,
            connector: { _ in }, event: { message in timedLock.lock(); timedReason = message; timedLock.unlock() })
        timed.start(); let timedClient = try connectLocal(path("timed"))
        try check(read(timedClient, &byte, 1) == 0, "connect timeout closes client")
        timedLock.lock(); let observedReason = timedReason; timedLock.unlock()
        try check(observedReason.contains("connection deadline"), "connect timeout reported")
        close(timedClient); try stopBridge(timed)
        print("PASS Guest connection deadline")

        var disconnected = false; var discardedLate = 0
        let disconnectedLock = NSLock(); var disconnectedCompletion: ((Result<GuestStreamEndpoint, Error>) -> Void)?
        let abandoned = try GuestStreamBridge(path: path("abandoned"), connector: { cb in
            disconnectedLock.lock(); disconnectedCompletion = cb; disconnectedLock.unlock()
        }, event: { reason in disconnectedLock.lock(); disconnected = reason.contains("Client disconnected"); disconnectedLock.unlock() })
        abandoned.start(); let abandonedClient = try connectLocal(path("abandoned"))
        try waitMain { disconnectedLock.lock(); defer { disconnectedLock.unlock() }; return disconnectedCompletion != nil }
        close(abandonedClient)
        try waitMain { disconnectedLock.lock(); defer { disconnectedLock.unlock() }; return disconnected }
        let (lateFD, latePeer) = try socketPair()
        disconnectedCompletion!(.success(GuestStreamEndpoint(fd: lateFD, onClose: { discardedLate += 1 })))
        try check(discardedLate == 1, "disconnected client's late Guest link closed")
        try check(read(latePeer, &byte, 1) == 0, "late Guest descriptor is gone")
        close(latePeer); try stopBridge(abandoned)
        print("PASS client disconnect during pending Guest connection")
        print("PASS all host-only bridge tests; no VZ/Guest capture acceptance")
    }
}
