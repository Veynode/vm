import Foundation
import Virtualization

// The host end of a macOS Guest's port relay (native/guest-relay/relay.c):
// connections to the Guest's own loopback services (sshd, Screen Sharing,
// published ports) ride virtio-vsock to the relay at port 5022 instead of the
// Guest's vmnet address, which macOS's Local Network privacy keeps from a
// launchd-started daemon and its workers. Each connection opens with one line
// naming the Guest TCP port ("22\n").

let guestRelayPort: UInt32 = 5022

struct GuestRelayError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// A connected descriptor to the Guest relay, with `header` already sent
/// when given. `device` runs on the main queue. Blocks; never call it there.
func guestRelayConnect(device: @escaping () -> VZVirtioSocketDevice?, header: String?, timeout: TimeInterval = 10) throws -> Int32 {
    let done = DispatchSemaphore(value: 0)
    var outcome: Result<Int32, Error> = .failure(GuestRelayError(message: "Guest relay connection timed out"))
    DispatchQueue.main.async {
        guard let socket = device() else {
            outcome = .failure(GuestRelayError(message: "the macOS Guest is not running")); done.signal(); return
        }
        socket.connect(toPort: guestRelayPort) { result in
            outcome = result.flatMap { connection in
                // The duplicate keeps the stream; the connection object is released here.
                let descriptor = dup(connection.fileDescriptor)
                connection.close()
                return descriptor >= 0 ? .success(descriptor) : .failure(GuestRelayError(message: "Cannot retain the Guest relay descriptor"))
            }.mapError { GuestRelayError(message: "Guest relay unreachable: \($0.localizedDescription)") }
            done.signal()
        }
    }
    guard done.wait(timeout: .now() + timeout) == .success else { throw GuestRelayError(message: "Guest relay connection timed out") }
    let fd = try outcome.get()
    var one: Int32 = 1
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
    if let header {
        let bytes = Array(header.utf8)
        guard bytes.withUnsafeBytes({ Darwin.write(fd, $0.baseAddress!, bytes.count) }) == bytes.count else {
            close(fd); throw GuestRelayError(message: "Guest relay write failed")
        }
    }
    return fd
}

/// Copies `from` to `to` until EOF, then half-closes.
private func spliceHalf(_ from: Int32, _ to: Int32) {
    var buffer = [UInt8](repeating: 0, count: 65536)
    outer: while true {
        let n = buffer.withUnsafeMutableBytes { Darwin.read(from, $0.baseAddress!, $0.count) }
        if n < 0 && errno == EINTR { continue }
        if n <= 0 { break }
        var done = 0
        while done < n {
            let w = buffer.withUnsafeBytes { Darwin.write(to, $0.baseAddress! + done, n - done) }
            if w < 0 && errno == EINTR { continue }
            if w <= 0 { break outer }
            done += w
        }
    }
    shutdown(to, SHUT_WR)
    shutdown(from, SHUT_RD)
}

/// A Unix socket the daemon connects to for the Guest relay: each accepted
/// connection is spliced to a new relay connection, the client sending the
/// port line itself. The path's directory is the daemon's, private (0700).
final class GuestPorts {
    let path: String
    private let listener: Int32
    private let device: () -> VZVirtioSocketDevice?

    init(path: String, device: @escaping () -> VZVirtioSocketDevice?) throws {
        var address = sockaddr_un()
        let bytes = Array(path.utf8) + [0]
        guard path.hasPrefix("/"), bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            throw GuestRelayError(message: "guest_ports_socket must be an absolute path under 104 bytes")
        }
        var parent = stat()
        guard stat((path as NSString).deletingLastPathComponent, &parent) == 0, parent.st_uid == getuid(), parent.st_mode & 0o077 == 0 else {
            throw GuestRelayError(message: "guest_ports_socket's directory must be private to this user")
        }
        // A socket left by an earlier worker of this machine.
        var existing = stat()
        if lstat(path, &existing) == 0 {
            guard existing.st_mode & S_IFMT == S_IFSOCK, existing.st_uid == getuid() else {
                throw GuestRelayError(message: "guest_ports_socket exists and is not this user's socket")
            }
            unlink(path)
        }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw GuestRelayError(message: "Cannot create the guest ports socket") }
        address.sun_family = sa_family_t(AF_UNIX)
        address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: bytes) }
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard bound == 0, chmod(path, 0o600) == 0, listen(fd, 64) == 0 else {
            close(fd); throw GuestRelayError(message: "Cannot listen on \(path): \(String(cString: strerror(errno)))")
        }
        self.path = path; listener = fd; self.device = device
    }

    func start() {
        Thread.detachNewThread { [self] in
            while true {
                let client = accept(listener, nil, nil)
                if client < 0 { if errno == EINTR || errno == ECONNABORTED { continue }; return }
                var one: Int32 = 1
                setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
                Thread.detachNewThread { [device] in
                    guard let guest = try? guestRelayConnect(device: device, header: nil) else { close(client); return }
                    // Both halves end before the descriptors close.
                    let inward = DispatchSemaphore(value: 0)
                    Thread.detachNewThread { spliceHalf(client, guest); inward.signal() }
                    spliceHalf(guest, client)
                    inward.wait()
                    close(client); close(guest)
                }
            }
        }
    }

    deinit { close(listener); unlink(path) }
}
