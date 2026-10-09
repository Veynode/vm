import Foundation
import CoreGraphics

// These helpers bind one info/capture/input observation, not the separate video
// process or a browser session. No lock-state policy is inferred from identity.
func requireConsoleIdentity(_ observation:ConsoleIdentityObservation) throws -> GuestConsoleIdentityV1 {
    guard observation.identityEligible,let identity=observation.identity else {
        throw CaptureFailure(code:"console_identity_unavailable",message:"Public console identity unavailable: \(observation.rejection?.rawValue ?? "ineligible")")
    }
    return identity
}
func validateCaptureConsole(_ expected:GuestConsoleIdentityV1,observation:ConsoleIdentityObservation,
                            display:UInt32,width:Int,height:Int,bounds:CGRect) throws {
    let current=try requireConsoleIdentity(observation)
    guard current==expected else {throw CaptureFailure(code:"console_identity_changed",message:"Console session or display changed during capture; image discarded")}
    guard let w=UInt32(exactly:width),let h=UInt32(exactly:height),
          display==expected.displayID,w==expected.physicalWidth,h==expected.physicalHeight,
          let hash=try? consoleGeometryHash(displayID:display,width:w,height:h,bounds:bounds),
          hash==expected.displayGeometrySHA256 else {
        throw CaptureFailure(code:"console_geometry_mismatch",message:"Captured console geometry differs from the public identity; image discarded")
    }
}
func consoleInfoFields(_ console:ConsoleIdentityObservation,observation:[String:Any],input:[String:Any]) -> [String:Any] {
    var observation=observation,input=input,status=console.wire
    status.removeValue(forKey:"identity")
    if !console.identityEligible {
        observation["screenshot_available"]=false
        observation["console_identity_rejection"]=console.rejection?.rawValue ?? "ineligible"
        input["available"]=false
        // Preserve the distinct permission result and existing cleanup fault.
        input["console_identity_rejection"]=console.rejection?.rawValue ?? "ineligible"
    }
    return ["console_identity":console.identity.map{$0.wire as Any} ?? NSNull(),
            "console_identity_status":status,"observation":observation,"input_policy":input]
}
