import Foundation
import Darwin

struct GuestStreamBridgeError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

// The connector returns an owned close operation, never a stream to read/write.
// Late successful connects must still close after the reply deadline has passed.
final class GuestStreamProbe {
    typealias Connector = (@escaping (Result<() -> Void, Error>) -> Void) -> Void
    private let lock = NSLock()
    private var completion: (([String:Any]) -> Void)?
    private let deadline: DispatchTime

    static func response(_ error: String? = nil) -> [String:Any] {
        ["reachable":error == nil,"guest_port":5001,"error":error as Any? ?? NSNull()]
    }
    private init(timeoutMilliseconds: Int, completion: @escaping ([String:Any]) -> Void) {
        self.completion = completion
        deadline = .now() + .milliseconds(timeoutMilliseconds)
    }
    static func run(timeoutMilliseconds: Int = 2000, connector: Connector,
                    completion: @escaping ([String:Any]) -> Void) {
        let probe = GuestStreamProbe(timeoutMilliseconds:timeoutMilliseconds, completion:completion)
        DispatchQueue.main.asyncAfter(deadline:probe.deadline) { probe.finish("timeout") }
        connector { result in
            switch result {
            case .success(let close): close(); probe.finish(nil)
            case .failure: probe.finish("guest_unavailable")
            }
        }
    }
    private func finish(_ error: String?) {
        lock.lock()
        let callback = completion; completion = nil
        let code = DispatchTime.now() >= deadline ? "timeout" : error
        lock.unlock()
        callback?(Self.response(code))
    }
}

// Used on the VM's main queue, including connector callbacks. Reply deadlines
// and admission bounds are separate because VZ connect cannot be cancelled.
final class GuestStreamProbeCoordinator {
    static let pendingLifetimeNanoseconds: UInt64 = 30_000_000_000
    static let minimumIntervalNanoseconds: UInt64 = 5_000_000_000
    private let now: () -> UInt64
    private var generation: UInt64 = 0
    private var pending: (generation: UInt64, started: UInt64)?
    private var lastStarted: UInt64?
    private var lastResult: (response: [String:Any], received: UInt64)?

    init(now: @escaping () -> UInt64 = { DispatchTime.now().uptimeNanoseconds }) { self.now = now }
    func run(timeoutMilliseconds: Int = 2000, connector: GuestStreamProbe.Connector,
             completion: @escaping ([String:Any]) -> Void) {
        let instant = now()
        if let started = lastStarted, instant - started < Self.minimumIntervalNanoseconds {
            let cached = lastResult ?? (GuestStreamProbe.response("busy"), started)
            var response = cached.response
            response["cached"] = true
            response["age_ms"] = (instant - cached.received) / 1_000_000
            completion(response); return
        }
        if let pending, instant - pending.started < Self.pendingLifetimeNanoseconds {
            completion(GuestStreamProbe.response("busy")); return
        }
        generation += 1
        let current = generation
        pending = (current, instant); lastStarted = instant
        GuestStreamProbe.run(timeoutMilliseconds:timeoutMilliseconds, connector:{ complete in
            connector { result in
                guard self.pending?.generation == current else {
                    if case .success(let close) = result { close() }
                    return
                }
                self.pending = nil
                complete(result)
            }
        }, completion:{ response in
            if self.generation == current { self.lastResult = (response, self.now()) }
            completion(response)
        })
    }
}

// An owned duplicate descriptor. The original VZ connection stays retained until
// onClose runs on the VM queue; the forwarding loop never calls VZ APIs.
final class GuestStreamEndpoint {
    let fd: Int32
    private let onClose: () -> Void
    init(fd: Int32, onClose: @escaping () -> Void = {}) { self.fd = fd; self.onClose = onClose }
    deinit { Darwin.close(fd); onClose() }
}

private final class GuestStreamPending {
    let lock = NSLock()
    var result: Result<GuestStreamEndpoint, Error>?
    var cancelled = false
    func complete(_ result: Result<GuestStreamEndpoint, Error>) {
        lock.lock(); defer { lock.unlock() }
        if !cancelled { self.result = result }
    }
    func take() -> Result<GuestStreamEndpoint, Error>? {
        lock.lock(); defer { lock.unlock() }
        let value = result; result = nil; return value
    }
    func cancel() { lock.lock(); cancelled = true; result = nil; lock.unlock() }
}

final class GuestStreamBridge {
    typealias Connector = (@escaping (Result<GuestStreamEndpoint, Error>) -> Void) -> Void
    let socketPath: String
    private let directoryFD: Int32
    private let socketName: String
    private let socketDevice: dev_t
    private let socketInode: ino_t
    private let listener: Int32
    private let connector: Connector
    private let event: (String) -> Void
    private let queue = DispatchQueue(label: "veynode.guest-stream-bridge", qos: .userInitiated)
    private let lock = NSLock()
    private var stopping = false
    private var stopped = false
    private var completions: [() -> Void] = []
    private let writeTimeout: UInt64
    private let connectTimeout: UInt64
    private static let capacity = 64 * 1024

    init(path: String, writeTimeoutMilliseconds: UInt64 = 500,
         connectTimeoutMilliseconds: UInt64 = 5000,
         connector: @escaping Connector, event: @escaping (String) -> Void = { _ in }) throws {
        func invalid(_ message: String) -> GuestStreamBridgeError { GuestStreamBridgeError(message: message) }
        guard path.hasPrefix("/"), !path.utf8.contains(0) else { throw invalid("Bridge socket path must be absolute without NUL") }
        let parts = path.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count > 2, !parts.dropFirst().contains(where: { $0.isEmpty || $0 == "." || $0 == ".." }) else {
            throw invalid("Bridge path must have canonical components")
        }
        var address = sockaddr_un()
        let bytes = Array(path.utf8) + [0]
        guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else { throw invalid("Bridge Unix socket path is too long") }
        // Traverse without symlinks, including intermediate directories.
        var parent = Darwin.open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard parent >= 0 else { throw invalid("Cannot open socket root") }
        for component in parts.dropFirst().dropLast() {
            let next = Darwin.openat(parent, String(component), O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
            Darwin.close(parent); parent = next
            guard parent >= 0 else { throw invalid("Bridge parent must be a real directory without symlinks") }
        }
        var metadata = stat()
        guard fstat(parent, &metadata) == 0, metadata.st_uid == getuid(), metadata.st_mode & 0o777 == 0o700 else {
            Darwin.close(parent); throw invalid("Bridge parent must be owned by current UID with mode 0700")
        }
        let name = String(parts.last!)
        var existing = stat()
        guard fstatat(parent, name, &existing, AT_SYMLINK_NOFOLLOW) != 0, errno == ENOENT else {
            Darwin.close(parent); throw invalid("Bridge refuses an existing socket path or symlink")
        }
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { Darwin.close(parent); throw invalid("Cannot create bridge socket") }
        var bound = false
        do {
            try Self.configure(fd)
            address.sun_family = sa_family_t(AF_UNIX)
            address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
            withUnsafeMutableBytes(of: &address.sun_path) { destination in
                destination.copyBytes(from: bytes)
            }
            let result = withUnsafePointer(to: &address) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            guard result == 0 else { throw invalid("Cannot bind bridge socket; existing paths are never replaced") }
            bound = true
            guard fchmodat(parent, name, 0o600, 0) == 0,
                  fstatat(parent, name, &metadata, AT_SYMLINK_NOFOLLOW) == 0,
                  metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFSOCK), metadata.st_uid == getuid(),
                  metadata.st_mode & 0o777 == 0o600, Darwin.listen(fd, 1) == 0 else {
                throw invalid("Cannot secure or listen on bridge socket")
            }
        } catch {
            Darwin.close(fd)
            if bound { unlinkat(parent, name, 0) }
            Darwin.close(parent); throw error
        }
        socketPath = path; directoryFD = parent; socketName = name
        socketDevice = metadata.st_dev; socketInode = metadata.st_ino; listener = fd
        self.connector = connector; self.event = event
        writeTimeout = writeTimeoutMilliseconds * 1_000_000
        connectTimeout = connectTimeoutMilliseconds * 1_000_000
    }

    // Listener provenance only. It does not identify or authorize a Guest desktop.
    func binding(vmID:String,workerInstance:String,bridgeInstance:String) throws -> [String:Any] {
        lock.lock();defer{lock.unlock()}
        guard !stopping && !stopped else {throw GuestStreamBridgeError(message:"Bridge stopped or stopping")}
        var current=stat()
        guard fstatat(directoryFD,socketName,&current,AT_SYMLINK_NOFOLLOW)==0,
              current.st_dev==socketDevice,current.st_ino==socketInode,
              current.st_mode & mode_t(S_IFMT)==mode_t(S_IFSOCK),current.st_uid==getuid(),current.st_mode & 0o777==0o600 else {
            throw GuestStreamBridgeError(message:"Bridge socket incarnation changed")
        }
        return ["version":1,"vm_id":vmID,"worker_instance":workerInstance,"bridge_instance":bridgeInstance,
                "worker_pid":getpid(),"socket_path":socketPath,"socket_device":UInt64(socketDevice),"socket_inode":UInt64(socketInode),
                "guest_port":5001,"scope":"host-worker-listener-only","input_authorized":false]
    }

    private static func configure(_ fd: Int32) throws {
        let flags = fcntl(fd, F_GETFL)
        var noSignal: Int32 = 1
        guard flags >= 0, fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0,
              fcntl(fd, F_SETFD, FD_CLOEXEC) == 0,
              setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, socklen_t(MemoryLayout<Int32>.size)) == 0 else {
            throw GuestStreamBridgeError(message: "Cannot configure nonblocking bridge descriptor")
        }
    }
    func start() { queue.async { self.run() } }
    func stop(completion: @escaping () -> Void = {}) {
        lock.lock()
        if stopped { lock.unlock(); DispatchQueue.main.async(execute: completion); return }
        stopping = true; completions.append(completion); lock.unlock()
    }
    private var shouldStop: Bool { lock.lock(); defer { lock.unlock() }; return stopping }
    private func rejectWaitingClients() {
        // Bound each admission drain so a local connection flood cannot starve stop.
        for _ in 0..<8 {
            let fd = Darwin.accept(listener, nil, nil)
            if fd < 0 { return }
            Darwin.close(fd)
        }
    }
    private func connectGuest(client: Int32) throws -> GuestStreamEndpoint {
        let pending = GuestStreamPending()
        connector { result in pending.complete(result) }
        defer { pending.cancel() }
        let deadline = DispatchTime.now().uptimeNanoseconds + connectTimeout
        while !shouldStop {
            var peer = pollfd(fd: client, events: Int16(POLLIN), revents: 0)
            if Darwin.poll(&peer, 1, 0) > 0 {
                if peer.revents & Int16(POLLHUP | POLLERR | POLLNVAL) != 0 {
                    throw GuestStreamBridgeError(message: "Client disconnected during Guest connection")
                }
                if peer.revents & Int16(POLLIN) != 0 {
                    var byte: UInt8 = 0
                    if recv(client, &byte, 1, MSG_PEEK) == 0 {
                        throw GuestStreamBridgeError(message: "Client disconnected during Guest connection")
                    }
                }
            }
            if let result = pending.take() { return try result.get() }
            if DispatchTime.now().uptimeNanoseconds >= deadline { throw GuestStreamBridgeError(message: "Guest stream connection deadline") }
            rejectWaitingClients()
            // Background queue only; VZ connect itself always runs on main.
            usleep(10_000)
        }
        throw GuestStreamBridgeError(message: "Bridge stopped during connection")
    }
    private func run() {
        defer {
            Darwin.close(listener)
            var metadata = stat()
            if fstatat(directoryFD, socketName, &metadata, AT_SYMLINK_NOFOLLOW) == 0,
               metadata.st_dev == socketDevice, metadata.st_ino == socketInode {
                unlinkat(directoryFD, socketName, 0)
            }
            Darwin.close(directoryFD)
            lock.lock(); stopped = true; let callbacks = completions; completions.removeAll(); lock.unlock()
            DispatchQueue.main.async { callbacks.forEach { $0() } }
        }
        while !shouldStop {
            var item = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
            let result = Darwin.poll(&item, 1, 50)
            if result < 0 && errno == EINTR { continue }
            if result < 0 || item.revents & Int16(POLLERR | POLLNVAL) != 0 { event("listener_failed"); return }
            guard result > 0 else { continue }
            let client = Darwin.accept(listener, nil, nil)
            if client < 0 { continue }
            do {
                try Self.configure(client)
                let guest = try connectGuest(client: client)
                try Self.configure(guest.fd)
                try forward(client: client, guest: guest.fd)
                // guest endpoint release closes its duplicate and VZ original.
            } catch { event(error.localizedDescription) }
            Darwin.close(client)
            // A client queued while this session was ending must retry instead
            // of silently becoming the next Guest stream owner.
            rejectWaitingClients()
        }
    }
    private func forward(client: Int32, guest: Int32) throws {
        struct PendingBytes {
            var bytes = [UInt8](repeating: 0, count: GuestStreamBridge.capacity)
            var offset = 0
            var count = 0
            var since: UInt64?
            var isEmpty: Bool { count == 0 }
        }
        var toGuest = PendingBytes(), toClient = PendingBytes()
        var clientEOF = false, guestEOF = false
        func failure(_ message: String) -> GuestStreamBridgeError { GuestStreamBridgeError(message: message) }
        func writePending(_ fd: Int32, _ pending: inout PendingBytes) throws {
            guard !pending.isEmpty else { return }
            let now = DispatchTime.now().uptimeNanoseconds
            guard now - pending.since! < writeTimeout else { throw failure("Bridge pending write deadline") }
            let amount = pending.bytes.withUnsafeBytes { Darwin.write(fd, $0.baseAddress!.advanced(by: pending.offset), pending.count - pending.offset) }
            if amount < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) { return }
            guard amount > 0 else { throw failure("Bridge destination disconnected") }
            pending.offset += amount
            if pending.offset == pending.count { pending.offset = 0; pending.count = 0; pending.since = nil }
        }
        func readAvailable(_ fd: Int32, _ pending: inout PendingBytes, _ eof: inout Bool) throws {
            guard !eof, pending.isEmpty else { return }
            let amount = pending.bytes.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress!, $0.count) }
            if amount < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) { return }
            guard amount >= 0 else { throw failure("Bridge source read failed") }
            if amount == 0 { eof = true; return }
            pending.since = DispatchTime.now().uptimeNanoseconds
            pending.offset = 0; pending.count = amount
        }
        while !shouldStop {
            try writePending(guest, &toGuest); try writePending(client, &toClient)
            // A disconnected endpoint terminates the session after flushing its
            // already-read bytes. The next client gets a fresh fixed-port VZ link.
            if (clientEOF && toGuest.isEmpty) || (guestEOF && toClient.isEmpty) { return }
            var descriptors = [pollfd(fd: listener, events: Int16(POLLIN), revents: 0),
                pollfd(fd: client, events: Int16((!clientEOF && toGuest.isEmpty ? POLLIN : 0) | (!toClient.isEmpty ? POLLOUT : 0)), revents: 0),
                pollfd(fd: guest, events: Int16((!guestEOF && toClient.isEmpty ? POLLIN : 0) | (!toGuest.isEmpty ? POLLOUT : 0)), revents: 0)]
            let result = Darwin.poll(&descriptors, nfds_t(descriptors.count), 25)
            if result < 0 && errno == EINTR { continue }
            guard result >= 0 else { throw failure("Bridge poll failed") }
            if descriptors[0].revents & Int16(POLLIN) != 0 { rejectWaitingClients() }
            if descriptors.dropFirst().contains(where: { $0.revents & Int16(POLLNVAL) != 0 }) { throw failure("Bridge invalid descriptor") }
            // Read on HUP to drain kernel-buffered data before observing EOF.
            if descriptors[1].revents & Int16(POLLIN | POLLHUP | POLLERR) != 0 { try readAvailable(client, &toGuest, &clientEOF) }
            if descriptors[2].revents & Int16(POLLIN | POLLHUP | POLLERR) != 0 { try readAvailable(guest, &toClient, &guestEOF) }
        }
    }
}
