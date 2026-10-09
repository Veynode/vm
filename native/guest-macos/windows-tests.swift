import Foundation
import CoreGraphics

func windowsSelfTest() {
    // Synthetic metadata only. No SCK, CoreGraphics window enumeration, TCC
    // request, display capture, app launch or input is performed by these tests.
    let main=WindowDisplayObservation(id:7,main:true,active:true,asleep:false,bounds:CGRect(x:0,y:0,width:1280,height:720),pixelWidth:2560,pixelHeight:1440,rotation:0)
    let left=WindowDisplayObservation(id:8,main:false,active:true,asleep:false,bounds:CGRect(x:-800,y:-100,width:800,height:600),pixelWidth:800,pixelHeight:600,rotation:0)
    let asleep=WindowDisplayObservation(id:9,main:false,active:true,asleep:true,bounds:CGRect(x:1280,y:0,width:800,height:600),pixelWidth:1600,pixelHeight:1200,rotation:0)
    let displays=[main,left,asleep]
    let identity=try! GuestConsoleIdentityV1(securitySessionID:42,uid:501,consoleSet:nil,displayID:7,physicalWidth:2560,physicalHeight:1440,
        displayGeometrySHA256:consoleGeometryHash(displayID:7,width:2560,height:1440,bounds:main.bounds))
    let current=ConsoleIdentityObservation(identity:identity,rejection:nil)
    func candidate(_ id:UInt32,_ bounds:CGRect=CGRect(x:20,y:30,width:100,height:80),name:String="Fixture App",bundle:String="org.example.fixture")->WindowCandidate {
        WindowCandidate(id:id,pid:123,bundle:bundle,application:name,bounds:bounds)
    }
    func result(_ values:[WindowCandidate],limit:Int=maximumWindowResultBytes)->[String:Any] {
        try! makeWindowResult(identity:identity,displays:displays,scales:[7:2,8:1,9:2],candidates:values,omitted:0,
            requested:200,completed:100,monotonic:123456,snapshot:"12345678-1234-1234-1234-123456789abc",limit:limit)
    }
    func denied(_ code:String,_ body:()throws->Void) {
        do {try body();preconditionFailure("Expected \(code)")}
        catch let error as WindowObservationFailure {precondition(error.code==code,"Wrong typed failure")}
        catch {preconditionFailure("Wrong failure type")}
    }
    precondition(windowIntersections(CGRect(x:-20,y:10,width:50,height:70),displays)==[7,8])
    precondition(windowIntersections(CGRect(x:-200,y:-80,width:100,height:40),displays)==[8])
    precondition(windowIntersections(CGRect(x:1280,y:0,width:100,height:80),displays).isEmpty,"Touching main edge or a sleeping display is not visible intersection")
    precondition(!validWindowBounds(CGRect(x:Double.infinity,y:0,width:10,height:10)))
    precondition(!validWindowBounds(CGRect(x:0,y:0,width:0,height:10)))
    let wire=result([candidate(2,CGRect(x:-20,y:10,width:50,height:70)),candidate(1)])
    let windows=wire["windows"] as! [[String:Any]],displayWire=wire["displays"] as! [[String:Any]]
    precondition(windows.map{$0["window_id"] as! UInt32}==[1,2])
    precondition(windows[1]["display_ids"] as! [UInt32]==[7,8])
    precondition((windows[1]["bounds_points"] as! [String:Double])["x"] == -20)
    precondition(displayWire[0]["point_pixel_scale"] as? Double==2 && displayWire[1]["point_pixel_scale"] as? Double==1)
    precondition(displayWire[2]["point_pixel_scale"] is NSNull,"Asleep display must not report fabricated capture scale")
    precondition((wire["requested_at_us"] as! Int64)>(wire["completed_at_us"] as! Int64),"Wall-clock adjustment is not a false failure")
    for key in ["titles_included","accessibility_tree","input_authorization","truncated"] {
        precondition(wire[key] as? Bool==false)
    }
    let data=try! JSONSerialization.data(withJSONObject:wire,options:[.sortedKeys,.withoutEscapingSlashes])
    let decoded=try! JSONSerialization.jsonObject(with:data) as! [String:Any]
    precondition(data.count<=maximumWindowResultBytes)
    for key in ["titles_included","accessibility_tree","input_authorization","truncated"] {
        precondition(CFGetTypeID(decoded[key]! as AnyObject)==CFBooleanGetTypeID(),"Boolean wire type must remain boolean")
    }
    precondition(Set(windows[0].keys)==Set(["window_id","owner","bounds_points","display_ids"]))
    precondition(Set((windows[0]["owner"] as! [String:Any]).keys)==Set(["pid","bundle_identifier","application_name"]))
    let quoted=result([candidate(1,name:"\"/你好\\\n",bundle:"org.example/quoted")])
    let escaped=try! JSONSerialization.data(withJSONObject:quoted,options:[.withoutEscapingSlashes])
    let escapedValue=try! JSONSerialization.jsonObject(with:escaped) as! [String:Any]
    precondition(NSDictionary(dictionary:escapedValue).isEqual(to:quoted),"Quoted application metadata must preserve bounded JSON roundtrip")

    let many=result((1...200).map{candidate(UInt32($0))})
    precondition((many["windows"] as! [[String:Any]]).count==128 && many["eligible_window_count"] as? Int==200 && many["truncated"] as? Bool==true)
    let longNames=(1...128).map{candidate(UInt32($0),name:String(repeating:"界",count:85),bundle:String(repeating:"a",count:256))}
    let budget=result(longNames)
    precondition((budget["windows"] as! [[String:Any]]).count<128 && budget["truncated"] as? Bool==true)
    precondition(try! JSONSerialization.data(withJSONObject:budget,options:[.sortedKeys,.withoutEscapingSlashes]).count<=maximumWindowResultBytes)
    let invalid=result([candidate(1,name:String(repeating:"界",count:86)),candidate(2,CGRect(x:Double.nan,y:0,width:10,height:10)),candidate(3)])
    precondition((invalid["windows"] as! [[String:Any]]).count==1 && invalid["omitted_invalid_count"] as? Int==2 && invalid["truncated"] as? Bool==true)
    let noScale=try! makeWindowResult(identity:identity,displays:[main],scales:[7:Double.nan],candidates:[],omitted:0,requested:1,completed:1,monotonic:1)
    precondition((noScale["displays"] as! [[String:Any]])[0]["point_pixel_scale"] is NSNull)
    denied("unsupported_geometry") {_ = try makeWindowResult(identity:identity,displays:Array(repeating:main,count:17),scales:[:],candidates:[],omitted:0,requested:1,completed:1,monotonic:1)}
    denied("unsupported_geometry") {_ = try makeWindowResult(identity:identity,displays:[main],scales:[:],candidates:[],omitted:0,requested:1,completed:1,monotonic:1,limit:100)}

    try! validateWindowObservation(expected:identity,current:current,before:displays,after:displays,permission:true)
    denied("screen_recording_permission_required"){try validateWindowObservation(expected:identity,current:current,before:displays,after:displays,permission:false)}
    denied("console_identity_unavailable"){try validateWindowObservation(expected:identity,current:.init(identity:nil,rejection:.displayAsleep),before:displays,after:displays,permission:true)}
    var changed=identity.wire;changed["security_session_id"]=43
    denied("console_identity_changed"){try validateWindowObservation(expected:identity,current:.init(identity:try GuestConsoleIdentityV1(wire:changed),rejection:nil),before:displays,after:displays,permission:true)}
    denied("display_configuration_changed"){try validateWindowObservation(expected:identity,current:current,before:displays,after:[main,left],permission:true)}
    let slot=WindowObservationSlot(),first=slot.acquire()!
    precondition(slot.acquire()==nil,"Timed-out caller cannot admit a replacement while original callback is outstanding")
    slot.finish(UUID());precondition(slot.acquire()==nil,"Foreign callback must not release ownership")
    slot.finish(first);let second=slot.acquire()!;slot.finish(first)
    precondition(slot.acquire()==nil,"Late duplicate callback must not release next request")
    slot.finish(second);precondition(slot.acquire() != nil)
    for configuration in [(false,true,true,true),(true,false,true,true),(true,true,false,true),(true,true,true,false)] {
        let policy=windowObservationPolicy(enabled:configuration.0,guest:configuration.1,permission:configuration.2,identityEligible:configuration.3)
        precondition(policy["windows_available"] as? Bool==false)
    }
    precondition(windowObservationPolicy(enabled:true,guest:true,permission:true,identityEligible:true)["windows_available"] as? Bool==true)
    let disabled=handle(["id":"windows-denied","method":"windows"],allowExec:false)
    precondition((disabled["error"] as? [String:String])?["code"]=="forbidden")
    let bad=handle(["id":"windows-bad","method":"windows","arguments":[:]],allowExec:false)
    precondition((bad["error"] as? [String:String])?["code"]=="invalid_request")
    print("PASS: windows pure metadata: negative coordinates, mixed Retina, multi-display intersections, sleeping display, clock adjustment, strict JSON/no titles,128-window/48KiB limits, UTF8 escaping, permission/identity/display drift, outstanding callback ownership, and disabled method. No host window query or Guest acceptance.")
}
