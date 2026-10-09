// Public, read-only OS identity. This does not establish an unlocked desktop.
import Foundation
import CoreFoundation
import Security
import CoreGraphics
import SystemConfiguration
import CryptoKit
import Darwin

enum ConsoleBindingError: String, Error {
    case invalidWire = "invalid_wire"
    case invalidGeometry = "invalid_geometry"
    case securitySessionUnavailable = "security_session_unavailable"
    case ineligibleSecuritySession = "ineligible_security_session"
    case windowSessionUnavailable = "window_session_unavailable"
    case primaryConsoleUnavailable = "primary_console_unavailable"
    case consoleNotActive = "console_not_active"
    case identityUIDMismatch = "identity_uid_mismatch"
    case displayUnavailable = "display_unavailable"
    case displayInactive = "display_inactive"
    case displayAsleep = "display_asleep"
    case observationChanged = "observation_changed"
    case identityMismatch = "identity_mismatch"
    case identityIneligible = "identity_ineligible"
}

private func exactUInt32(_ value: Any?) -> UInt32? {
    guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID() else { return nil }
    let d = n.doubleValue
    guard d.isFinite, d >= 0, d <= Double(UInt32.max), d.rounded(.towardZero) == d else { return nil }
    return UInt32(d)
}

private func exactBool(_ value: Any?) -> Bool? {
    guard let n = value as? NSNumber, CFGetTypeID(n) == CFBooleanGetTypeID() else { return nil }
    return n.boolValue
}

struct GuestConsoleIdentityV1: Equatable {
    let securitySessionID: UInt32
    let uid: UInt32
    let consoleSet: UInt32?
    let displayID: UInt32
    let physicalWidth: UInt32
    let physicalHeight: UInt32
    let displayGeometrySHA256: String

    static let wireKeys: Set<String> = ["version", "security_session_id", "uid", "console_set", "display_id", "physical_width", "physical_height", "display_geometry_sha256"]
    static func validSessionID(_ id: UInt32) -> Bool { id > 0 && id < UInt32.max - 1 }

    init(securitySessionID: UInt32, uid: UInt32, consoleSet: UInt32?, displayID: UInt32,
         physicalWidth: UInt32, physicalHeight: UInt32, displayGeometrySHA256: String) throws {
        guard Self.validSessionID(securitySessionID), uid > 0, uid < UInt32.max,
              displayID > 0, physicalWidth > 0, physicalWidth <= 16_384,
              physicalHeight > 0, physicalHeight <= 16_384,
              displayGeometrySHA256.utf8.count == 64,
              displayGeometrySHA256.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else {
            throw ConsoleBindingError.invalidWire
        }
        self.securitySessionID = securitySessionID; self.uid = uid; self.consoleSet = consoleSet
        self.displayID = displayID; self.physicalWidth = physicalWidth; self.physicalHeight = physicalHeight
        self.displayGeometrySHA256 = displayGeometrySHA256
    }

    init(wire: [String: Any]) throws {
        guard Set(wire.keys) == Self.wireKeys, exactUInt32(wire["version"]) == 1,
              let session = exactUInt32(wire["security_session_id"]), let uid = exactUInt32(wire["uid"]),
              let display = exactUInt32(wire["display_id"]),
              let width = exactUInt32(wire["physical_width"]), let height = exactUInt32(wire["physical_height"]),
              let hash = wire["display_geometry_sha256"] as? String else { throw ConsoleBindingError.invalidWire }
        let console = exactUInt32(wire["console_set"])
        guard console != nil || wire["console_set"] is NSNull else { throw ConsoleBindingError.invalidWire }
        try self.init(securitySessionID: session, uid: uid, consoleSet: console, displayID: display,
                      physicalWidth: width, physicalHeight: height, displayGeometrySHA256: hash)
    }

    static func decode(_ data: Data) throws -> GuestConsoleIdentityV1 {
        guard data.count <= 2_048,
              let object = try? JSONSerialization.jsonObject(with: data),
              let fields = object as? [String: Any] else { throw ConsoleBindingError.invalidWire }
        return try GuestConsoleIdentityV1(wire: fields)
    }

    func encode() throws -> Data { try JSONSerialization.data(withJSONObject: wire, options: [.sortedKeys]) }

    var wire: [String: Any] {
        ["version": 1, "security_session_id": securitySessionID, "uid": uid, "console_set": consoleSet.map { $0 as Any } ?? NSNull(),
         "display_id": displayID, "physical_width": physicalWidth, "physical_height": physicalHeight,
         "display_geometry_sha256": displayGeometrySHA256]
    }
}

// The canonical hash can also be computed by independent implementations without
// Swift/JSON floating-point formatting. -0 has the same geometry meaning as +0.
func consoleGeometryHash(displayID: UInt32, width: UInt32, height: UInt32, bounds: CGRect) throws -> String {
    let values = [Double(bounds.origin.x), Double(bounds.origin.y), Double(bounds.width), Double(bounds.height)]
    guard displayID != 0, width > 0, height > 0, width <= 16_384, height <= 16_384,
          values.allSatisfy({ $0.isFinite && abs($0) <= 10_000_000 }), values[2] > 0, values[3] > 0 else {
        throw ConsoleBindingError.invalidGeometry
    }
    var data = Data("Veynode/GuestDisplay/v1\0".utf8)
    for value in [displayID, width, height] {
        var big = value.bigEndian
        withUnsafeBytes(of: &big) { data.append(contentsOf: $0) }
    }
    for value in values {
        var big = (value == 0 ? Double(0) : value).bitPattern.bigEndian
        withUnsafeBytes(of: &big) { data.append(contentsOf: $0) }
    }
    return SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

struct ConsoleIdentityObservation {
    let identity: GuestConsoleIdentityV1?
    let rejection: ConsoleBindingError?
    var identityEligible: Bool { identity != nil && rejection == nil }

    var wire: [String: Any] {
        ["version": 1, "identity": identity.map { $0.wire as Any } ?? NSNull(),
         "identity_eligible": identityEligible,
         "identity_rejection": rejection.map { $0.rawValue as Any } ?? NSNull(),
         "probe_grants_input": false, "input_authorization": "not_evaluated",
         "lock_state": "unknown", "lock_state_basis": "no_verified_public_lock_predicate",
         "observation_basis": "two_consecutive_public_api_reads",
         "transition_race_excluded": false]
    }

    // Matching is deliberately NOT an authorization function. Consumers still
    // need worker/bridge provenance, lifecycle invalidation, leases and lock state.
    func requireIdentityMatch(_ expected: GuestConsoleIdentityV1) throws {
        guard identityEligible, let actual = identity else { throw ConsoleBindingError.identityIneligible }
        guard actual == expected else { throw ConsoleBindingError.identityMismatch }
    }

}

// Kept pure so real/effective/process/WindowServer/current-console disagreement
// is testable without changing the host login or creating a different user.
struct ConsoleSessionAuthority {
    var securitySessionID: UInt32
    var attributes: SessionAttributeBits
    var realUID: UInt32
    var effectiveUID: UInt32
    var windowSessionUID: UInt32
    var primaryConsoleUID: UInt32
    var onConsole: Bool
    var loginDone: Bool

    func validate() throws {
        guard GuestConsoleIdentityV1.validSessionID(securitySessionID), attributes.contains(.sessionHasGraphicAccess),
              !attributes.contains(.sessionIsRoot), !attributes.contains(.sessionIsRemote) else {
            throw ConsoleBindingError.ineligibleSecuritySession
        }
        guard onConsole && loginDone else { throw ConsoleBindingError.consoleNotActive }
        guard realUID > 0, realUID < UInt32.max, realUID == effectiveUID,
              realUID == primaryConsoleUID, realUID == windowSessionUID else {
            throw ConsoleBindingError.identityUIDMismatch
        }
    }
}

enum GuestConsoleIdentityReader {
    private static func once() throws -> GuestConsoleIdentityV1 {
        var session: SecuritySessionId = 0
        var attributes: SessionAttributeBits = []
        guard SessionGetInfo(callerSecuritySession, &session, &attributes) == errSecSuccess else {
            throw ConsoleBindingError.securitySessionUnavailable
        }
        guard let fields = CGSessionCopyCurrentDictionary() as? [String: Any],
              let sessionUID = exactUInt32(fields[kCGSessionUserIDKey as String]),
              let onConsole = exactBool(fields[kCGSessionOnConsoleKey as String]),
              let loginDone = exactBool(fields[kCGSessionLoginDoneKey as String]) else {
            throw ConsoleBindingError.windowSessionUnavailable
        }
        // Current macOS can omit this optional hardware-set property. Absence
        // remains null, never a fabricated set ID; malformed present data fails.
        let consoleValue = fields[kCGSessionConsoleSetKey as String]
        let consoleSet = exactUInt32(consoleValue)
        guard consoleValue == nil || consoleSet != nil else { throw ConsoleBindingError.windowSessionUnavailable }
        var consoleUID: uid_t = 0, unusedGID: gid_t = 0
        // Discard the returned name. No username or entire session dictionary is serialized.
        guard SCDynamicStoreCopyConsoleUser(nil, &consoleUID, &unusedGID) != nil else {
            throw ConsoleBindingError.primaryConsoleUnavailable
        }
        let real = getuid(), effective = geteuid()
        try ConsoleSessionAuthority(securitySessionID: session, attributes: attributes, realUID: real,
            effectiveUID: effective, windowSessionUID: sessionUID, primaryConsoleUID: consoleUID,
            onConsole: onConsole, loginDone: loginDone).validate()
        let display = CGMainDisplayID()
        guard display != 0, let mode = CGDisplayCopyDisplayMode(display),
              let width = UInt32(exactly: mode.pixelWidth), let height = UInt32(exactly: mode.pixelHeight) else {
            throw ConsoleBindingError.displayUnavailable
        }
        guard CGDisplayIsActive(display) != 0 else { throw ConsoleBindingError.displayInactive }
        guard CGDisplayIsAsleep(display) == 0 else { throw ConsoleBindingError.displayAsleep }
        let hash = try consoleGeometryHash(displayID: display, width: width, height: height, bounds: CGDisplayBounds(display))
        return try GuestConsoleIdentityV1(securitySessionID: session, uid: real, consoleSet: consoleSet,
            displayID: display, physicalWidth: width, physicalHeight: height, displayGeometrySHA256: hash)
    }

    // A bounded observation, not a watcher. Two reads catch observed transitions,
    // but cannot exclude an intervening switch-away-and-back or a subsequent switch.
    static func observe(read: () throws -> GuestConsoleIdentityV1 = once) -> ConsoleIdentityObservation {
        do {
            let first = try read(), second = try read()
            guard first == second else { throw ConsoleBindingError.observationChanged }
            return ConsoleIdentityObservation(identity: second, rejection: nil)
        } catch let error as ConsoleBindingError {
            return ConsoleIdentityObservation(identity: nil, rejection: error)
        } catch {
            return ConsoleIdentityObservation(identity: nil, rejection: .securitySessionUnavailable)
        }
    }
}
