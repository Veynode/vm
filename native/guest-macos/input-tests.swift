import Foundation
import CoreGraphics

// Pure policy/coordinate fixtures. This sink never creates or posts a host CGEvent.
private final class InputTestSink:GuestInputSink {
    var display:GuestDisplayGeometry?=GuestDisplayGeometry(display:7,width:2560,height:1440,x:-1280,y:90,pointsWidth:1280,pointsHeight:720)
    var permission=true, failPost=false
    var identityAvailable=true,sessionID:UInt32=42,uid:UInt32=501,identityReads=0
    var afterPost:(()->Void)?
    var events:[GuestInputEvent]=[]
    func geometry()->GuestDisplayGeometry? {display}
    func consoleIdentity()->GuestConsoleIdentityV1? {
        identityReads+=1
        guard identityAvailable,let d=display else{return nil}
        return try! GuestConsoleIdentityV1(securitySessionID:sessionID,uid:uid,consoleSet:nil,displayID:d.display,
            physicalWidth:UInt32(d.width),physicalHeight:UInt32(d.height),displayGeometrySHA256:consoleGeometryHash(displayID:d.display,width:UInt32(d.width),height:UInt32(d.height),bounds:CGRect(x:d.x,y:d.y,width:d.pointsWidth,height:d.pointsHeight)))
    }
    func available()->Bool {permission}
    func post(_ event:GuestInputEvent)throws {if failPost {throw GuestInputFailure(code:"test",message:"test sink failure")};events.append(event);afterPost?()}
}
func inputSelfTest() {
    let sink=InputTestSink();var clock:UInt64=10_000_000_000
    let service=GuestInputCoordinator(sink:sink,now:{clock})
    let op=String(repeating:"b",count:32)
    var frame="12345678-1234-1234-1234-123456789abc"
    func capture(_ id:String?=nil)throws -> UInt64 {
        if id==nil {frame=UUID().uuidString.lowercased()}
        let d=sink.display!
        return try service.record(["frame_id":id ?? frame,"display_id":d.display,"width":d.width,"height":d.height,"captured_monotonic_us":clock/1000,"desktop_bounds_points":["x":CGFloat(d.x),"y":CGFloat(d.y),"width":CGFloat(d.pointsWidth),"height":CGFloat(d.pointsHeight)],"console_identity":sink.consoleIdentity()!.wire])
    }
    func args(_ events:[[String:Any]],begin:Bool=true,operation:String=op)->[String:Any] {
        ["operation_id":operation,"begin":begin,"frame_id":frame,"source_generation":1,"width":2560,"height":1440,"events":events]
    }
    func key(_ name:String,_ down:Bool)->[String:Any] {["type":"key","data":["key":["type":"qcode","data":name],"down":down]]}
    func button(_ down:Bool)->[String:Any] {["type":"btn","data":["button":"left","down":down]]}
    func denied(_ body:()throws->Void){do {try body();preconditionFailure("expected rejection")}catch{}}
    precondition(try! capture()==1)
    // Every pixel centre round trips exactly at 2x scale and a negative display origin.
    let d=sink.display!
    for pixel in 0..<2560 {
        let normalized=UInt64((2*pixel+1)*32768/(2*2560))
        let point=d.point(normalized,0)
        precondition(abs(point.x-(-1280+(Double(pixel)+0.5)/2))<0.000001)
    }
    _=try! service.step(args([key("ctrl",true),key("a",true)]))
    precondition(sink.events.count==2)
    // A stale operation's cleanup cannot release the current owner.
    let stale=try! service.cleanup(["operation_id":String(repeating:"c",count:32)])
    precondition(stale["released"] as? Bool==false && sink.events.count==2)
    _=try! service.cleanup(["operation_id":op])
    precondition(sink.events.count==4)
    _=try! service.step(args([key("a",true)]))
    // Watchdog releases synthetic state without waiting for the host client.
    clock+=1_100_000_000;service.tick()
    precondition(sink.events.count==6)
    _=try! service.cleanup(["operation_id":op])
    _=try! capture()
    denied {_=try service.step(args([button(true)]))}
    let abs:[[String:Any]]=[["type":"abs","data":["axis":"x","value":16384]],["type":"abs","data":["axis":"y","value":16384]]]
    _=try! service.step(args(abs))
    _=try! service.step(args([button(true)],begin:false))
    _=try! service.cleanup(["operation_id":op])
    let before=sink.events.count
    denied {_=try service.step(args([key("a",true),key("insert",true)]))}
    precondition(sink.events.count==before,"invalid batch must post nothing")
    _=try! service.step(args([key("a",true)]))
    var wrongOperation=args([key("a",false)],begin:false);wrongOperation["operation_id"]=String(repeating:"c",count:32)
    denied {_=try service.step(wrongOperation)}
    _=try! service.cleanup(["operation_id":op])
    clock+=5_000_000_001
    denied {_=try service.step(args([key("a",true)]))}
    _=try! capture()
    _=try! service.step(args([key("a",true)]))
    sink.display=GuestDisplayGeometry(display:8,width:2560,height:1440,x:0,y:0,pointsWidth:1280,pointsHeight:720)
    service.tick()
    denied {_=try service.step(args([key("a",false)],begin:false))}
    precondition(try! capture()==2,"display change invalidates generation")
    // Old frame removed after the bounded metadata cache is full.
    for n in 0..<17 {_=try! capture("cache-\(n)")}
    denied {_=try service.step(args([button(true)]))}
    let oldToken=service.captureToken()
    service.invalidate()
    denied {_=try service.record([:],captureToken:oldToken)}
    precondition(service.shutdown())
    denied {_=try service.step(args([key("a",true)]))}
    let failedSink=InputTestSink()
    failedSink.permission=false
    let permissionDenied=GuestInputCoordinator(sink:failedSink,now:{clock})
    denied {_=try permissionDenied.step(args([key("a",true)]))}
    precondition(failedSink.events.isEmpty)
    let errorSink=InputTestSink()
    let failService=GuestInputCoordinator(sink:errorSink,now:{clock})
    let g=errorSink.display!
    _=try! failService.record(["frame_id":frame,"display_id":g.display,"width":g.width,"height":g.height,"captured_monotonic_us":clock/1000,"desktop_bounds_points":["x":g.x,"y":g.y,"width":g.pointsWidth,"height":g.pointsHeight],"console_identity":errorSink.consoleIdentity()!.wire])
    _=try! failService.step(args([key("a",true)]))
    errorSink.failPost=true
    denied {_=try failService.cleanup(["operation_id":op])}
    precondition(failService.blockedReason()=="synthetic_release_unconfirmed")
    errorSink.failPost=false
    denied {_=try failService.step(args([key("b",true)]))} // cleanup failure remains latched
    precondition(failService.shutdown()) // explicit retry releases only its recorded key
    precondition(errorSink.events.count==2 && failService.blockedReason()=="synthetic_release_unconfirmed")
    let geometryService=GuestInputCoordinator(sink:InputTestSink(),now:{clock})
    for invalid:Any in [true,Double.nan,Double.infinity,"1280"] {
        denied {_=try geometryService.record(["frame_id":frame,"display_id":g.display,"width":g.width,"height":g.height,"captured_monotonic_us":clock/1000,"desktop_bounds_points":["x":invalid,"y":g.y,"width":g.pointsWidth,"height":g.pointsHeight]])}
    }
    // Same-size desktops belonging to a different login must not reuse anchors.
    for changedSession in [true,false] {
        let identitySink=InputTestSink()
        let guarded=GuestInputCoordinator(sink:identitySink,now:{clock})
        let d=identitySink.display!,originalIdentity=identitySink.consoleIdentity()!
        let metadata:[String:Any]=["frame_id":frame,"display_id":d.display,"width":d.width,"height":d.height,"captured_monotonic_us":clock/1000,"desktop_bounds_points":["x":CGFloat(d.x),"y":CGFloat(d.y),"width":CGFloat(d.pointsWidth),"height":CGFloat(d.pointsHeight)],"console_identity":originalIdentity.wire]
        _=try! guarded.record(metadata)
        _=try! guarded.step(args([key("a",true)]))
        if changedSession {identitySink.sessionID=43} else {identitySink.uid=502}
        let sent=identitySink.events.count
        denied {_=try guarded.step(args([key("a",false)],begin:false))}
        precondition(identitySink.events.count==sent,"Neither input nor release may target a different login")
        precondition(guarded.blockedReason()=="synthetic_release_unconfirmed")
        denied {_=try guarded.cleanup(["operation_id":op])}
        identitySink.sessionID=42;identitySink.uid=501
        precondition(guarded.shutdown(),"Original console restored permits explicit release retry")
        precondition(identitySink.events.count==sent+1 && guarded.blockedReason() != nil)
    }
    let sampledSink=InputTestSink(),sampled=GuestInputCoordinator(sink:sampledSink,now:{clock})
    let sd=sampledSink.display!
    var sampleMetadata:[String:Any]=["frame_id":frame,"display_id":sd.display,"width":sd.width,"height":sd.height,"captured_monotonic_us":clock/1000,"desktop_bounds_points":["x":CGFloat(sd.x),"y":CGFloat(sd.y),"width":CGFloat(sd.pointsWidth),"height":CGFloat(sd.pointsHeight)],"console_identity":sampledSink.consoleIdentity()!.wire]
    let snapshot=sampleMetadata.removeValue(forKey:"console_identity")!
    denied {_=try sampled.record(sampleMetadata)}
    sampleMetadata["console_identity"]=snapshot
    _=try! sampled.record(sampleMetadata)
    let initialReads=sampledSink.identityReads
    for _ in 0..<10 {sampled.tick();clock+=100_000_000}
    precondition(sampledSink.identityReads-initialReads==2,"Idle anchor identity polling must be at most2Hz")
    sampledSink.identityAvailable=false
    clock+=500_000_000;sampled.tick()
    denied {_=try sampled.step(args([key("a",true)]))}
    precondition(sampledSink.events.isEmpty,"No input from an unavailable identity")
    let idleReads=sampledSink.identityReads
    for _ in 0..<10 {clock+=100_000_000;sampled.tick()}
    precondition(sampledSink.identityReads==idleReads,"No repeated authority reads after anchors are invalidated")
    // Drift after the first post must stop a valid, already-validated batch.
    // Also verify release's second element cannot escape its original session.
    for switchDuringRelease in [false,true] {
        let raceSink=InputTestSink()
        let coordinator=GuestInputCoordinator(sink:raceSink,now:{clock}),d=raceSink.display!
        _=try! coordinator.record(["frame_id":frame,"display_id":d.display,"width":d.width,"height":d.height,"captured_monotonic_us":clock/1000,"desktop_bounds_points":["x":CGFloat(d.x),"y":CGFloat(d.y),"width":CGFloat(d.pointsWidth),"height":CGFloat(d.pointsHeight)],"console_identity":raceSink.consoleIdentity()!.wire])
        if switchDuringRelease {
            _=try! coordinator.step(args([key("a",true),key("b",true)]))
            raceSink.afterPost={raceSink.sessionID=43}
            denied {_=try coordinator.cleanup(["operation_id":op])}
            precondition(raceSink.events.count==3,"Only first release may post before session drift")
        } else {
            raceSink.afterPost={raceSink.sessionID=43}
            denied {_=try coordinator.step(args([key("a",true),key("b",true)]))}
            precondition(raceSink.events.count==1,"Second batch element and wrong-session cleanup must not post")
        }
        precondition(coordinator.blockedReason()=="synthetic_release_unconfirmed")
        raceSink.afterPost=nil;raceSink.sessionID=42
        let count=raceSink.events.count
        precondition(coordinator.shutdown() && raceSink.events.count==count+1,"Only the still-held original key is released on explicit retry")
    }
    let disabled=handle(["id":"test","method":"input_cleanup","arguments":["operation_id":op]],allowExec:false)
    precondition((disabled["error"] as? [String:String])?["code"]=="forbidden")
    if !systemString("hw.model").hasPrefix("VirtualMac") {
        let guarded=handle(["id":"test","method":"input_cleanup","arguments":["operation_id":op]],allowExec:false,allowObserve:true,allowInput:true)
        precondition((guarded["error"] as? [String:String])?["code"]=="virtual_guest_required")
    }
    print("PASS: synthetic input policy: Retina pixel centres/origin, whole-batch validation, stale metadata, bounded cache, operation fencing, watchdog release, geometry/console invalidation, same-geometry login/UID mismatch, unsafe cleanup refusal/original-session retry, at-most2Hz active-anchor identity polling and zero idle polling, disabled/physical-host guards. No desktop input posted.")
}
