import Foundation
import CoreGraphics

func consoleIntegrationSelfTest() {
    let bounds=CGRect(x:-1280,y:90,width:1280,height:720)
    let identity=try! GuestConsoleIdentityV1(securitySessionID:42,uid:501,consoleSet:nil,displayID:7,
        physicalWidth:2560,physicalHeight:1440,displayGeometrySHA256:consoleGeometryHash(displayID:7,width:2560,height:1440,bounds:bounds))
    let valid=ConsoleIdentityObservation(identity:identity,rejection:nil)
    func rejected(_ code:String,_ body:()throws->Void) {
        do {try body();preconditionFailure("expected \(code)")}
        catch let error as CaptureFailure {precondition(error.code==code,"wrong capture rejection")}
        catch {preconditionFailure("wrong error type")}
    }
    try! validateCaptureConsole(identity,observation:valid,display:7,width:2560,height:1440,bounds:bounds)
    for key in ["uid","security_session_id","display_id","physical_width","physical_height","display_geometry_sha256","console_set"] {
        var wire=identity.wire
        switch key {case "display_geometry_sha256":wire[key]=String(repeating:"0",count:64)
        case "console_set":wire[key]=0
        default:wire[key]=(wire[key] as! NSNumber).uint32Value+1}
        let changed=ConsoleIdentityObservation(identity:try! GuestConsoleIdentityV1(wire:wire),rejection:nil)
        rejected("console_identity_changed"){try validateCaptureConsole(identity,observation:changed,display:7,width:2560,height:1440,bounds:bounds)}
    }
    for failure in [ConsoleBindingError.consoleNotActive,.identityUIDMismatch,.observationChanged,.displayAsleep,.securitySessionUnavailable] {
        let unavailable=ConsoleIdentityObservation(identity:nil,rejection:failure)
        rejected("console_identity_unavailable"){_=try requireConsoleIdentity(unavailable)}
        rejected("console_identity_unavailable"){try validateCaptureConsole(identity,observation:unavailable,display:7,width:2560,height:1440,bounds:bounds)}
        let fields=consoleInfoFields(unavailable,observation:["screenshot_available":true,"screen_recording_permission":"granted"],input:["available":true,"permission":"granted"])
        precondition(fields["console_identity"] is NSNull)
        let observe=fields["observation"] as! [String:Any],input=fields["input_policy"] as! [String:Any]
        precondition(observe["screenshot_available"] as? Bool==false && input["available"] as? Bool==false)
        precondition(observe["screen_recording_permission"] as? String=="granted" && input["permission"] as? String=="granted")
    }
    rejected("console_geometry_mismatch"){try validateCaptureConsole(identity,observation:valid,display:8,width:2560,height:1440,bounds:bounds)}
    rejected("console_geometry_mismatch"){try validateCaptureConsole(identity,observation:valid,display:7,width:1280,height:720,bounds:bounds)}
    rejected("console_geometry_mismatch"){try validateCaptureConsole(identity,observation:valid,display:7,width:2560,height:1440,bounds:CGRect(x:0,y:90,width:1280,height:720))}
    let permitted=consoleInfoFields(valid,observation:["screenshot_available":true],input:["available":true])
    precondition((permitted["input_policy"] as! [String:Any])["available"] as? Bool==true,"unknown lock state must not erase existing authorization")
    precondition((permitted["console_identity_status"] as! [String:Any])["lock_state"] as? String=="unknown")
    let actualHandler=handle(["id":"identity-info","method":"info"],allowExec:false,consoleRead:{valid})
    let actualResult=actualHandler["result"] as! [String:Any]
    precondition(try! GuestConsoleIdentityV1(wire:actualResult["console_identity"] as! [String:Any])==identity)
    let deniedHandler=handle(["id":"identity-denied","method":"info"],allowExec:false,consoleRead:{ConsoleIdentityObservation(identity:nil,rejection:.identityUIDMismatch)})
    let denied=deniedHandler["result"] as! [String:Any],caps=denied["capabilities"] as! [String:Any]
    precondition(denied["console_identity"] is NSNull && caps["input"] as? Bool==false && caps["screenshot"] as? Bool==false)
    print("PASS: console identity info handler, unavailable capability fence, capture pre/post identity and geometry drift rejection; unknown lock state preserves existing permission policy. Synthetic identity fixtures only, no host capture/input.")
}
