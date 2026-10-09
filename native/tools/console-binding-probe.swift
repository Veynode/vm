// Standalone read-only identity probe. No capture, input, TCC request, or listener.
import Foundation
import CoreGraphics
import Security
import Darwin

@main struct ConsoleBindingProbe {
    static func emit(_ value: [String: Any]) throws {
        let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        FileHandle.standardOutput.write(data); FileHandle.standardOutput.write(Data([10]))
    }

    static func selfTest() throws -> [String: Any] {
        var passed: [String] = []
        func check(_ name: String, _ value: Bool) throws {
            guard value else { throw NSError(domain: "ConsoleBindingSelfTest", code: 1, userInfo: [NSLocalizedDescriptionKey: name]) }
            passed.append(name)
        }
        func rejects(_ body: () throws -> Void) -> Bool { do { try body(); return false } catch { return true } }
        let hash = try consoleGeometryHash(displayID: 1, width: 2560, height: 1440, bounds: CGRect(x: -1280, y: 0, width: 1280, height: 720))
        let original = try GuestConsoleIdentityV1(securitySessionID: 42, uid: 501, consoleSet: 0, displayID: 1,
            physicalWidth: 2560, physicalHeight: 1440, displayGeometrySHA256: hash)
        let roundtrip = try JSONSerialization.jsonObject(with: JSONSerialization.data(withJSONObject: original.wire)) as! [String: Any]
        try check("strict_json_roundtrip", try GuestConsoleIdentityV1(wire: roundtrip) == original)
        try check("bounded_wire_roundtrip", try GuestConsoleIdentityV1.decode(original.encode()) == original)
        try check("oversize_wire_rejected", rejects { _ = try GuestConsoleIdentityV1.decode(Data(repeating: 32, count: 2_049)) })
        try check("non_object_wire_rejected", rejects { _ = try GuestConsoleIdentityV1.decode(Data("[]".utf8)) })
        let accepted = GuestConsoleIdentityReader.observe(read: { original })
        try accepted.requireIdentityMatch(original)
        try check("same_identity_matches", accepted.identityEligible)
        try check("probe_never_grants_input", accepted.wire["probe_grants_input"] as? Bool == false && accepted.wire["input_authorization"] as? String == "not_evaluated" && accepted.wire["lock_state"] as? String == "unknown")
        for (field, replacement) in [("security_session_id", 43 as Any), ("uid", 502), ("console_set", 1),
                                      ("display_id", 2), ("physical_width", 1280), ("physical_height", 720),
                                      ("display_geometry_sha256", String(repeating: "0", count: 64))] {
            var changed = original.wire; changed[field] = replacement
            let other = try GuestConsoleIdentityV1(wire: changed)
            try check("mismatch_" + field, rejects { try accepted.requireIdentityMatch(other) })
        }
        var absentConsole = original.wire; absentConsole["console_set"] = NSNull()
        let optionalIdentity = try GuestConsoleIdentityV1(wire: absentConsole)
        try check("optional_console_set_roundtrip", try GuestConsoleIdentityV1.decode(optionalIdentity.encode()) == optionalIdentity)
        try check("console_set_presence_mismatch", rejects { try accepted.requireIdentityMatch(optionalIdentity) })
        let absentObservation = GuestConsoleIdentityReader.observe(read: { optionalIdentity })
        try absentObservation.requireIdentityMatch(optionalIdentity)
        try check("both_missing_console_set_consistent", absentObservation.identityEligible)
        let invalid: [(String, Any)] = [("version", 2), ("version", true), ("console_set", false), ("console_set", "0"), ("uid", 0), ("uid", false),
            ("uid", -1), ("uid", 501.5), ("uid", "501"), ("uid", Double.nan), ("uid", Double.infinity),
            ("uid", UInt64(UInt32.max) + 1), ("security_session_id", 0), ("security_session_id", UInt32.max),
            ("security_session_id", UInt32.max - 1), ("display_id", 0), ("physical_width", 16_385),
            ("physical_height", 0), ("display_geometry_sha256", String(repeating: "A", count: 64))]
        for (index, pair) in invalid.enumerated() {
            var bad = original.wire; bad[pair.0] = pair.1
            try check("reject_invalid_field_\(index)", rejects { _ = try GuestConsoleIdentityV1(wire: bad) })
        }
        var extra = original.wire; extra["unlocked"] = true
        try check("reject_unknown_field", rejects { _ = try GuestConsoleIdentityV1(wire: extra) })
        var missing = original.wire; missing.removeValue(forKey: "uid")
        try check("reject_missing_field", rejects { _ = try GuestConsoleIdentityV1(wire: missing) })
        let positiveZero = try consoleGeometryHash(displayID: 1, width: 100, height: 100, bounds: CGRect(x: 0, y: 0, width: 100, height: 100))
        let negativeZero = try consoleGeometryHash(displayID: 1, width: 100, height: 100, bounds: CGRect(x: -0.0, y: -0.0, width: 100, height: 100))
        try check("negative_zero_normalized", positiveZero == negativeZero)
        try check("invalid_geometry_rejected", rejects { _ = try consoleGeometryHash(displayID: 1, width: 100, height: 100, bounds: CGRect(x: CGFloat.nan, y: 0, width: 100, height: 100)) })
        var reads = 0
        let transition = GuestConsoleIdentityReader.observe {
            reads += 1
            var changed = original.wire
            if reads == 2 { changed["security_session_id"] = 43 }
            return try GuestConsoleIdentityV1(wire: changed)
        }
        try check("transition_rejects_and_clears_identity", !transition.identityEligible && transition.identity == nil && transition.rejection == .observationChanged)
        try check("ineligible_cannot_match", rejects { try transition.requireIdentityMatch(original) })
        let authority = ConsoleSessionAuthority(securitySessionID: 42, attributes: [.sessionHasGraphicAccess],
            realUID: 501, effectiveUID: 501, windowSessionUID: 501, primaryConsoleUID: 501, onConsole: true, loginDone: true)
        try authority.validate()
        let mutations: [(String, (inout ConsoleSessionAuthority) -> Void)] = [
            ("effective_uid", { $0.effectiveUID = 0 }), ("different_window_user", { $0.windowSessionUID = 502 }),
            ("different_console_user", { $0.primaryConsoleUID = 502 }), ("switched_away", { $0.onConsole = false }),
            ("login_incomplete", { $0.loginDone = false }), ("no_graphics", { $0.attributes = [] }),
            ("remote_session", { $0.attributes.insert(.sessionIsRemote) }), ("root_session", { $0.attributes.insert(.sessionIsRoot) }),
            ("reserved_session", { $0.securitySessionID = UInt32.max }),
            ("all_root_uid", { $0.realUID = 0; $0.effectiveUID = 0; $0.windowSessionUID = 0; $0.primaryConsoleUID = 0 })]
        for (name, mutation) in mutations {
            var bad = authority; mutation(&bad)
            try check("authority_reject_" + name, rejects { try bad.validate() })
        }
        let failed = GuestConsoleIdentityReader.observe { throw ConsoleBindingError.securitySessionUnavailable }
        try check("api_failure_clears_identity", failed.identity == nil && failed.rejection == .securitySessionUnavailable && !failed.identityEligible)
        return ["passed": true, "checks": passed, "geometry_vector_sha256": hash,
                "host_capture_performed": false, "host_input_performed": false]
    }

    static func main() {
        do {
            switch Array(CommandLine.arguments.dropFirst()) {
            case ["--read-identity"]:
                try emit(GuestConsoleIdentityReader.observe().wire)
            case ["--self-test"]:
                try emit(selfTest())
            default:
                fputs("Usage: console-binding-probe --read-identity | --self-test\n", stderr); exit(2)
            }
        } catch {
            // Test errors contain only a fixed assertion name, not OS session dictionaries.
            fputs("console binding probe failed: \(error)\n", stderr); exit(1)
        }
    }
}
