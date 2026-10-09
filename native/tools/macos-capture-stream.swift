import Foundation
import Darwin
import CoreGraphics
import CoreMedia
import CoreVideo
import ScreenCaptureKit
import AppKit

// Guest diagnostic only: one trusted host connection, no authentication beyond
// CID2. The host bridge must supply authorization. Never run capture on host.
let maximumBody = 8 * 1024 * 1024
let clockName = "CMSampleBuffer presentationTimeStamp nanoseconds; Guest-local, not synchronized to host"
func log(_ event:String) { FileHandle.standardError.write(Data(("{\"event\":\"" + event + "\"}\n").utf8)) }
func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }
func appendBE<T:FixedWidthInteger>(_ value:T, to data:inout Data) { var value=value.bigEndian;withUnsafeBytes(of:&value){data.append(contentsOf:$0)} }
func jsonBody(_ object:[String:Any]) -> Data {
    guard let json=try? JSONSerialization.data(withJSONObject:object,options:.sortedKeys),json.count<=65535 else {fatalError("bounded JSON invariant")}
    var body=Data([1]);body.append(json);return body
}
struct Failure:Error { let code:String }
private func receiptID(_ value:Any?) -> String? {
    guard let text=value as? String,text.utf8.count==32,text.utf8.allSatisfy({(48...57).contains($0)||(97...102).contains($0)}) else{return nil};return text
}
// Service-private evidence only. Tokens identify an accepted capture, not an
// authorization grant; only the existing host-CID2 transport may query them.
final class CleanupLedger {
    struct Entry {var status:String;var basis:String?;var completed:UInt64?}
    let serviceInstance=UUID().uuidString.replacingOccurrences(of:"-",with:"").lowercased()
    private let lock=NSLock(),clock:()->UInt64,ttl:UInt64,limit:Int
    private var entries:[String:Entry]=[:],active:String?,quarantined:AnyObject?
    init(ttl:UInt64=120_000_000_000,limit:Int=32,clock:@escaping()->UInt64=now){self.ttl=ttl;self.limit=limit;self.clock=clock}
    private func prune() {
        let time=clock()
        entries=entries.filter{_,e in guard let ended=e.completed else{return true};return time>=ended && time-ended<ttl}
        let terminal=entries.filter{$0.value.completed != nil}.sorted{$0.value.completed! < $1.value.completed!}
        for (id,_) in terminal.prefix(max(0,terminal.count-limit)){entries.removeValue(forKey:id)}
    }
    func begin() throws -> String {
        lock.lock();defer{lock.unlock()};prune()
        guard quarantined==nil else{throw Failure(code:"capture_cleanup_quarantined")}
        guard active==nil else{throw Failure(code:"capture_busy")}
        var id=UUID().uuidString.replacingOccurrences(of:"-",with:"").lowercased()
        while entries[id] != nil {id=UUID().uuidString.replacingOccurrences(of:"-",with:"").lowercased()}
        entries[id]=Entry(status:"active",basis:nil,completed:nil);active=id;return id
    }
    func finish(_ id:String,cleanBasis:String?,retainOnFailure:AnyObject) {
        lock.lock();defer{lock.unlock()}
        guard active==id,entries[id]?.status=="active" else{return}
        if let basis=cleanBasis,["no_stream_created","stop_capture_completed","delegate_reported_stopped"].contains(basis) {
            entries[id]=Entry(status:"clean",basis:basis,completed:clock());active=nil
        } else {
            entries[id]=Entry(status:"failed",basis:nil,completed:clock());quarantined=retainOnFailure
            // Keep admission fenced even after this failure's short-lived
            // diagnostic record expires. Restart cannot attest old cleanup.
        }
        prune()
    }
    func query(_ request:[String:Any]) throws -> [String:Any] {
        guard Set(request.keys)==Set(["id","method","service_instance","capture_instance"]),request["method"] as? String=="cleanup_status",
              let id=request["id"] as? String,!id.isEmpty,id.utf8.count<=128,
              let service=receiptID(request["service_instance"]),let capture=receiptID(request["capture_instance"]) else {throw Failure(code:"invalid_cleanup_query")}
        lock.lock();defer{lock.unlock()};prune()
        let entry=service==serviceInstance ? entries[capture]:nil
        let status=service==serviceInstance ? (entry?.status ?? "unknown"):"service_mismatch"
        var response:[String:Any]=["event":"cleanup_status","id":id,"protocol_version":1,"service_instance":serviceInstance,
                                  "capture_instance":capture,"status":status,"cleanup_confirmed":status=="clean","retention_ms":ttl/1_000_000]
        if status=="clean",let basis=entry?.basis {response["cleanup_basis"]=basis}
        return response
    }
}
let cleanupLedger=CleanupLedger()
struct Start {
    let id:String,width:Int,height:Int,fps:Int
    let expectedConsoleIdentity:GuestConsoleIdentityV1?
    let cleanupReceipt:Bool
    init(_ request:[String:Any]) throws {
        let fields=Set(request.keys),base:Set<String>=["id","method","width","height","fps"]
        guard base.isSubset(of:fields),fields.isSubset(of:base.union(["expected_console_identity","cleanup_receipt"])) else {throw Failure(code:"invalid_start")}
        guard request["method"] as? String=="start",
              let id=request["id"] as? String,!id.isEmpty,id.utf8.count<=128 else {throw Failure(code:"invalid_start")}
        func integer(_ key:String,_ range:ClosedRange<Int>) throws -> Int {
            guard let n=request[key] as? NSNumber,CFGetTypeID(n) != CFBooleanGetTypeID(),n.doubleValue.isFinite,
                  n.doubleValue.rounded()==n.doubleValue,range.contains(n.intValue) else {throw Failure(code:"invalid_start")}
            return n.intValue
        }
        self.id=id;width=try integer("width",64...1920);height=try integer("height",64...1080);fps=try integer("fps",1...30)
        guard width%2==0,height%2==0 else {throw Failure(code:"odd_dimensions")}
        if fields.contains("expected_console_identity") {
            guard let wire=request["expected_console_identity"] as? [String:Any],let value=try? GuestConsoleIdentityV1(wire:wire) else {throw Failure(code:"invalid_expected_console_identity")}
            expectedConsoleIdentity=value
        } else {expectedConsoleIdentity=nil}
        if let value=request["cleanup_receipt"] {
            guard let number=value as? NSNumber,CFGetTypeID(number)==CFBooleanGetTypeID(),number.boolValue else{throw Failure(code:"invalid_cleanup_receipt")}
            cleanupReceipt=true
        } else {cleanupReceipt=false}
    }
}

let identityPollNS:UInt64=500_000_000,identityFreshNS:UInt64=750_000_000
func streamConsoleIdentity(_ observation:ConsoleIdentityObservation,expected:GuestConsoleIdentityV1?) throws -> GuestConsoleIdentityV1 {
    guard observation.identityEligible,let current=observation.identity else {throw Failure(code:"console_identity_unavailable")}
    if let expected,expected != current {throw Failure(code:"console_identity_mismatch")}
    return current
}
func identityFresh(verifiedAt:UInt64?,at:UInt64) -> Bool {
    guard let verifiedAt,at>=verifiedAt else{return false};return at-verifiedAt<=identityFreshNS
}
func validateStreamDisplay(_ identity:GuestConsoleIdentityV1,display:UInt32,bounds:CGRect,scale:Double) throws {
    let width=Double(bounds.width)*scale,height=Double(bounds.height)*scale
    guard scale.isFinite,scale>0,width.isFinite,height.isFinite,
          abs(width-Double(identity.physicalWidth))<0.01,abs(height-Double(identity.physicalHeight))<0.01,
          display==identity.displayID,
          let hash=try? consoleGeometryHash(displayID:display,width:identity.physicalWidth,height:identity.physicalHeight,bounds:bounds),
          hash==identity.displayGeometrySHA256 else {throw Failure(code:"console_geometry_mismatch")}
}
// Copies only the visible row bytes; padding and IOSurface ownership never
// cross the wire. Runs on the serial sample queue, outside the state lock.
func frameBody(_ pixel:CVPixelBuffer,sequence:UInt64,pts:UInt64,width:Int,height:Int) throws -> Data {
    guard CVPixelBufferGetPixelFormatType(pixel)==kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
          CVPixelBufferGetWidth(pixel)==width,CVPixelBufferGetHeight(pixel)==height,
          CVPixelBufferGetPlaneCount(pixel)==2 else {throw Failure(code:"unexpected_pixel_format_or_geometry")}
    guard CVPixelBufferLockBaseAddress(pixel,.readOnly)==kCVReturnSuccess else {throw Failure(code:"pixel_lock_failed")}
    defer {CVPixelBufferUnlockBaseAddress(pixel,.readOnly)}
    let yBytes=width*height,uvBytes=width*height/2
    var body=Data(capacity:33+yBytes+uvBytes)
    body.append(2);appendBE(sequence,to:&body);appendBE(pts,to:&body)
    appendBE(UInt32(width),to:&body);appendBE(UInt32(height),to:&body)
    appendBE(UInt32(yBytes),to:&body);appendBE(UInt32(uvBytes),to:&body)
    // The fixed header is 33 bytes: 1 + 8 + 8 + 4 + 4 + 4 + 4.
    for plane in 0..<2 {
        let rows=plane==0 ? height:height/2,stride=CVPixelBufferGetBytesPerRowOfPlane(pixel,plane)
        guard stride>=width,CVPixelBufferGetHeightOfPlane(pixel,plane)==rows,
              let base=CVPixelBufferGetBaseAddressOfPlane(pixel,plane) else {throw Failure(code:"invalid_plane_layout")}
        for row in 0..<rows {body.append(base.advanced(by:row*stride).assumingMemoryBound(to:UInt8.self),count:width)}
    }
    guard body.count<=maximumBody else {throw Failure(code:"frame_too_large")};return body
}
// Full record write has a fixed deadline, including backpressure. On failure
// the connection closes: a partially written record is never resumed/reused.
func writeRecord(_ fd:Int32,_ body:Data,timeoutNS:UInt64=2_000_000_000) -> Bool {
    guard body.count<=maximumBody else{return false}
    let deadline=now()+timeoutNS
    var prefix=Data();appendBE(UInt32(body.count),to:&prefix)
    for data in [prefix,body] {
        let okay=data.withUnsafeBytes { bytes -> Bool in
            var offset=0
            while offset<bytes.count {
                guard now()<deadline else{return false}
                var descriptor=pollfd(fd:fd,events:Int16(POLLOUT),revents:0)
                let ready=poll(&descriptor,1,20)
                if ready<0 && errno==EINTR {continue}
                if ready<0 || descriptor.revents & Int16(POLLERR|POLLHUP|POLLNVAL) != 0 {return false}
                if ready==0 {continue}
                let count=write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
                if count<0 && (errno==EAGAIN || errno==EINTR) {continue}
                if count<=0 {return false};offset+=count
            }
            return true
        }
        if !okay {return false}
    }
    return true
}
final class CaptureClient:NSObject,SCStreamOutput,SCStreamDelegate {
    let fd:Int32
    let ledger:CleanupLedger
    let condition=NSCondition(),control=DispatchQueue(label:"capture.control"),samples=DispatchQueue(label:"capture.samples")
    let finished=DispatchSemaphore(value:0)
    var stream:SCStream?,latest:Data?,events:[Data]=[]
    var accepting=false,stopping=false,stopCompleted=false,connectionDead=false,startRequested=false,starting=false
    var writerFinished=false,stopID:String?,sequence:UInt64=0,replaced:UInt64=0,sent:UInt64=0,lastPTS:UInt64?
    var settings:Start?
    var captureFailure:String?,streamCreated=false,outputInstalled=false,delegateStopped=false
    var stoppingStream:SCStream?,cleanupBasisOverride:String?,secondaryStopError:[String:Any]=[:]
    var captureIdentity:GuestConsoleIdentityV1?,identityVerifiedAt:UInt64?
    var identityTimer:DispatchSourceTimer?,workspaceObservers:[NSObjectProtocol]=[]
    var identityReads:UInt64=0,identityReadMaxNS:UInt64=0
    // One CaptureClient owns one stream lifetime; stop irrevocably advances
    // this generation. No late copy may revive it, even if PTS is newer.
    var sampleGeneration:UInt64=1
    var staleSite:String?,staleAgeUS:UInt64?
    var identityPublishLockWaitMaxNS:UInt64=0,sampleCopyMaxNS:UInt64=0,sampleLockWaitMaxNS:UInt64=0,identityTimerLatenessMaxNS:UInt64=0
    let identityObserver:()->ConsoleIdentityObservation
    let clock:()->UInt64
    func diagnosticLocked()->[String:Any] {
        func us(_ ns:UInt64)->UInt64 {min(ns/1000,9_007_199_254_740_991)}
        return ["stale_site":staleSite as Any? ?? NSNull(),"verification_age_us":staleAgeUS as Any? ?? NSNull(),
                "authority_read_max_us":us(identityReadMaxNS),"identity_publish_lock_wait_max_us":us(identityPublishLockWaitMaxNS),
                "sample_copy_max_us":us(sampleCopyMaxNS),"sample_lock_wait_max_us":us(sampleLockWaitMaxNS),
                "identity_timer_lateness_max_us":us(identityTimerLatenessMaxNS),"sample_generation":sampleGeneration]
    }
    func staleLocked(_ site:String,verifiedAt:UInt64?,at:UInt64) {
        if staleSite==nil {staleSite=site
            if let verifiedAt,at>=verifiedAt {staleAgeUS=min((at-verifiedAt)/1000,9_007_199_254_740_991)}
        }
    }
    func lockSample() {let began=clock();condition.lock();let ended=clock();if ended>=began {sampleLockWaitMaxNS=max(sampleLockWaitMaxNS,ended-began)}}
    var copyFrame:(CVPixelBuffer,UInt64,UInt64,Int,Int)throws->Data = { try frameBody($0,sequence:$1,pts:$2,width:$3,height:$4) }
    var captureInstance:String?,stopPending=false,callbacksDraining=false
    init(fd:Int32,ledger:CleanupLedger=cleanupLedger,clock:@escaping()->UInt64=now,
         identityObserver:@escaping()->ConsoleIdentityObservation={GuestConsoleIdentityReader.observe()}) {
        self.fd=fd;self.ledger=ledger;self.clock=clock;self.identityObserver=identityObserver;super.init()
    }
    func receiptFields()->[String:Any] {
        guard let captureInstance else{return [:]}
        return ["service_instance":ledger.serviceInstance,"capture_instance":captureInstance,"cleanup_protocol_version":1,"cleanup_retention_ms":120000]
    }
    func run() {
        _ = fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)|O_NONBLOCK)
        var one:Int32=1;_ = setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&one,socklen_t(MemoryLayout<Int32>.size))
        DispatchQueue.global(qos:.userInitiated).async {self.writer()}
        reader()
        finished.wait()
        close(fd)
    }
    func writer() {
        while true {
            condition.lock()
            while events.isEmpty && latest==nil && !connectionDead && !stopCompleted {condition.wait()}
            if connectionDead {condition.unlock();break}
            let event = !events.isEmpty
            let body:Data?
            if event {body=events.removeFirst()} else {body=latest;latest=nil}
            let done=stopCompleted && body==nil
            condition.unlock()
            if done {break}
            if let body {
                if !event {
                    condition.lock()
                    // Stop after dequeue is cancellation, not an identity error.
                    guard accepting && !stopping && !connectionDead else {condition.unlock();continue}
                    let at=clock(),stale = !identityFresh(verifiedAt:identityVerifiedAt,at:at)
                    if stale {staleLocked("writer_admission",verifiedAt:identityVerifiedAt,at:at)}
                    condition.unlock()
                    if stale {fail("console_identity_verification_stale");continue}
                }
                if !writeRecord(fd,body) {abortConnection();break}
                if !event {condition.lock();sent+=1;condition.unlock()}
            }
        }
        _ = shutdown(fd,SHUT_RDWR)
        // Publish completion only after the final FD operation. Otherwise the
        // owner could close/reuse this FD before this thread's shutdown call.
        condition.lock();writerFinished=true;let complete=stopCompleted;condition.unlock()
        if complete {finished.signal()}
    }
    func reader() {
        var line=Data(),bytes=[UInt8](repeating:0,count:512)
        let firstDeadline=now()+5_000_000_000
        while true {
            condition.lock();let dead=connectionDead || stopCompleted;let requested=startRequested;condition.unlock()
            if dead {return}
            if !requested && now()>firstDeadline {fail("start_deadline_exceeded");return}
            var descriptor=pollfd(fd:fd,events:Int16(POLLIN),revents:0)
            let ready=poll(&descriptor,1,100)
            if ready<0 && errno==EINTR {continue}
            if ready<0 {abortConnection();return}
            if ready==0 {continue}
            let count=read(fd,&bytes,bytes.count)
            if count<0 && (errno==EAGAIN || errno==EINTR) {continue}
            if count<=0 {abortConnection();return}
            for byte in bytes.prefix(count) {
                if byte==10 {
                    guard let request=try? JSONSerialization.jsonObject(with:line) as? [String:Any] else {fail("invalid_json");return}
                    line.removeAll(keepingCapacity:true)
                    condition.lock();let requested=startRequested;condition.unlock()
                    if !requested {
                        do {
                            if request["method"] as? String=="cleanup_status" {
                                let response=try ledger.query(request)
                                condition.lock();events.append(jsonBody(response));startRequested=true;stopCompleted=true;condition.broadcast();let done=writerFinished;condition.unlock()
                                if done {finished.signal()};return
                            }
                            let config=try Start(request)
                            let instance=try ledger.begin()
                            condition.lock();captureInstance=instance;startRequested=true;settings=config
                            if config.cleanupReceipt {
                                var registered:[String:Any]=["event":"capture_registered","id":config.id];registered.merge(receiptFields()){_,new in new};events.append(jsonBody(registered));condition.signal()
                            }
                            condition.unlock()
                            control.async {self.start(config)}
                            DispatchQueue.global().asyncAfter(deadline:.now() + .seconds(5)) { [weak self] in
                                guard let self else{return}
                                self.condition.lock();let stalled = !self.accepting && !self.stopping;self.condition.unlock()
                                if stalled {self.fail("start_capture_deadline_exceeded")}
                            }
                        } catch {fail((error as? Failure)?.code ?? "invalid_start");return}
                    } else {
                        guard Set(request.keys)==Set(["id","method"]),request["method"] as? String=="stop",
                              let id=request["id"] as? String,!id.isEmpty,id.utf8.count<=128 else {fail("expected_stop");return}
                        stop(id:id);return
                    }
                } else {line.append(byte);if line.count>2048 {fail("command_too_large");return}}
            }
        }
    }
    func enqueue(_ object:[String:Any]) {condition.lock();if !connectionDead && !stopping && events.count<2 {events.append(jsonBody(object));condition.signal()};condition.unlock()}
    func safeError(_ error:Error) -> [String:Any] {
        let error=error as NSError
        let allowed=[SCStreamErrorDomain,NSCocoaErrorDomain,NSOSStatusErrorDomain,NSPOSIXErrorDomain]
        return ["error_domain":allowed.contains(error.domain) ? error.domain:"unrecognized-domain","error_code":error.code]
    }
    func fail(_ code:String,_ error:Error?=nil,_ metadata:[String:Any]=[:],duringStop:Bool=false) {
        condition.lock()
        if (!stopping || duringStop) && !connectionDead && !stopCompleted && captureFailure==nil {
            captureFailure=code
            var event:[String:Any]=["event":"error","code":code,"console_identity_diagnostic":diagnosticLocked()]
            event.merge(receiptFields()){_,new in new}
            if let id=settings?.id {event["id"]=id}
            if let error {
                // Do not relay localized descriptions, userInfo or arbitrary
                // NSError domain text across the diagnostics boundary.
                event.merge(safeError(error)){_,new in new}
            }
            event.merge(metadata){_,new in new}
            // Opt-in registration + started + error, then at most one terminal.
            if events.count<3 {events.append(jsonBody(event));condition.signal()}
        }
        condition.unlock();stop(id:nil)
    }
    func abortConnection() {
        condition.lock();connectionDead=true;latest=nil;events=[];condition.broadcast();condition.unlock()
        _ = shutdown(fd,SHUT_RDWR);stop(id:nil)
    }
    // Runs only on control. Record measured Guest read cost for later actual
    // evidence; this does not claim that host timings predict Guest timings.
    struct AuthorityRead {let identity:GuestConsoleIdentityV1;let completed:UInt64}
    func readIdentity(expected:GuestConsoleIdentityV1?) throws -> AuthorityRead {
        let began=clock(),observation=identityObserver(),ended=clock()
        condition.lock();identityReads+=1
        if ended>=began {identityReadMaxNS=max(identityReadMaxNS,ended-began)}
        let timely=ended>=began && ended-began<=identityFreshNS
        if !timely {staleLocked("authority_read",verifiedAt:began,at:ended)}
        condition.unlock()
        guard timely else {throw Failure(code:"console_identity_verification_stale")}
        return AuthorityRead(identity:try streamConsoleIdentity(observation,expected:expected),completed:ended)
    }
    // Authority completion time is captured BEFORE any metrics/state lock wait.
    // Publishing can never renew freshness to the eventual lock acquisition time.
    func publishAuthority(_ read:AuthorityRead) throws {
        let began=clock();condition.lock();let at=clock()
        if at>=began {identityPublishLockWaitMaxNS=max(identityPublishLockWaitMaxNS,at-began)}
        defer{condition.unlock()}
        guard !stopping && !connectionDead else{return}
        guard captureIdentity==read.identity else {throw Failure(code:"console_identity_mismatch")}
        guard identityFresh(verifiedAt:read.completed,at:at) else {
            staleLocked("authority_publish",verifiedAt:read.completed,at:at)
            throw Failure(code:"console_identity_verification_stale")
        }
        identityVerifiedAt=read.completed
    }
    func start(_ config:Start) {
        condition.lock();let cancelled=stopping;condition.unlock();if cancelled{return}
        guard CGPreflightScreenCaptureAccess() else {fail("screen_recording_permission_required");return}
        // Install invalidation triggers before the first authority read. A
        // notification fences immediately; it is never used as proof of identity.
        for name in [NSWorkspace.sessionDidResignActiveNotification,NSWorkspace.screensDidSleepNotification,NSWorkspace.willSleepNotification] {
            workspaceObservers.append(NSWorkspace.shared.notificationCenter.addObserver(forName:name,object:nil,queue:nil){[weak self] _ in self?.fail("console_session_transition")})
        }
        let initial:GuestConsoleIdentityV1
        do {initial=try readIdentity(expected:config.expectedConsoleIdentity).identity}
        catch {fail((error as? Failure)?.code ?? "console_identity_unavailable");return}
        captureIdentity=initial
        SCShareableContent.getExcludingDesktopWindows(false,onScreenWindowsOnly:true) { content,error in
            self.control.async {
                self.condition.lock();let cancelled=self.stopping;self.condition.unlock();if cancelled{return}
                let main=initial.displayID
                let state:[String:Any]=["main_display_active":CGDisplayIsActive(main) != 0,"main_display_asleep":CGDisplayIsAsleep(main) != 0]
                guard error==nil else {self.fail("shareable_content_failed",error,state);return}
                guard let display=content?.displays.first(where:{$0.displayID==main}) else {self.fail("main_display_unavailable",nil,state);return}
                let filter=SCContentFilter(display:display,excludingWindows:[])
                do {
                    _=try self.readIdentity(expected:initial)
                    try validateStreamDisplay(initial,display:display.displayID,bounds:display.frame,scale:Double(filter.pointPixelScale))
                } catch {self.fail((error as? Failure)?.code ?? "console_identity_unavailable");return}
                let configuration=SCStreamConfiguration()
                configuration.width=config.width;configuration.height=config.height
                configuration.minimumFrameInterval=CMTime(value:1,timescale:Int32(config.fps))
                configuration.pixelFormat=kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                configuration.queueDepth=3;configuration.showsCursor=true;configuration.capturesAudio=false
                configuration.scalesToFit=true;configuration.preservesAspectRatio=true
                configuration.colorMatrix=CGDisplayStream.yCbCrMatrix_ITU_R_709_2
                configuration.colorSpaceName=CGColorSpace.itur_709
                if #available(macOS 15.0,*) {configuration.captureDynamicRange = .SDR}
                let stream=SCStream(filter:filter,configuration:configuration,delegate:self)
                self.stream=stream;self.streamCreated=true
                do {try stream.addStreamOutput(self,type:.screen,sampleHandlerQueue:self.samples)}
                catch {self.fail("add_stream_output_failed",error);return}
                self.outputInstalled=true
                self.starting=true
                stream.startCapture { error in self.control.async {
                    self.starting=false
                    self.condition.lock();let cancelled=self.stopping;self.condition.unlock()
                    if cancelled {self.performStop();return}
                    guard error==nil else {self.fail("start_capture_failed",error);return}
                    do {try self.publishAuthority(self.readIdentity(expected:initial))}
                    catch {self.fail((error as? Failure)?.code ?? "console_identity_unavailable");return}
                    let bounds=filter.contentRect
                    self.condition.lock()
                    if self.stopping {self.condition.unlock();self.performStop();return}
                    var started:[String:Any]=["event":"started","id":config.id,"width":config.width,"height":config.height,"fps_limit":config.fps,
                      "pixel_format":"nv12-video-range","y_stride":config.width,"uv_stride":config.width,"clock":clockName,
                      "color_matrix":"ITU-R BT.709","color_space":"ITU-R BT.709","capture_dynamic_range":"SDR",
                      "source_display_id":display.displayID,"source_width_points":bounds.width,"source_height_points":bounds.height,
                      "source_point_pixel_scale":filter.pointPixelScale,"source_origin_x_points":bounds.origin.x,"source_origin_y_points":bounds.origin.y,
                      "preserves_aspect_ratio":true,"frame_header_bytes":33,"max_pending_frames":1,"max_inflight_frames":1,
                      "console_identity":initial.wire,"console_binding_mode":config.expectedConsoleIdentity==nil ? "observe_unbound":"expected_identity_matched",
                      "frame_console_identity_scope":"this_capture_connection","console_identity_poll_ms":500,"console_identity_max_age_ms":750,
                      "console_identity_reads":self.identityReads,"console_identity_read_max_us":self.identityReadMaxNS/1000,
                      "console_transition_atomic":false,"input_authorization":"not_evaluated","lock_state":"unknown"]
                    started.merge(self.receiptFields()){_,new in new};self.events.append(jsonBody(started))
                    self.accepting=true;self.condition.signal();self.condition.unlock()
                    self.startIdentityTimer()
                }}
            }
        }
    }
    func startIdentityTimer() {
        let timer=DispatchSource.makeTimerSource(queue:control)
        timer.schedule(deadline:.now() + .milliseconds(500),repeating:.nanoseconds(Int(identityPollNS)),leeway:.milliseconds(25))
        var scheduled=clock()+identityPollNS
        timer.setEventHandler{[weak self] in
            guard let self else{return}
            let invoked=self.clock()
            self.condition.lock()
            if invoked>=scheduled {self.identityTimerLatenessMaxNS=max(self.identityTimerLatenessMaxNS,invoked-scheduled)}
            let active=self.accepting && !self.stopping,expected=self.captureIdentity
            self.condition.unlock()
            // Dispatch coalesces missed timer firings. Maintain the fixed grid,
            // without scheduling additional catch-up identity reads.
            if invoked>=scheduled {scheduled += ((invoked-scheduled)/identityPollNS+1)*identityPollNS}
            guard active,let expected else{return}
            do {try self.publishAuthority(self.readIdentity(expected:expected))}
            catch {self.fail((error as? Failure)?.code ?? "console_identity_unavailable")}
        }
        identityTimer=timer;timer.resume()
    }
    func stop(id:String?) {
        condition.lock()
        if stopping {condition.unlock();return}
        stopping=true;accepting=false;sampleGeneration+=1;latest=nil;stopID=id;condition.unlock()
        control.async {self.performStop()}
        // If the OS never confirms stop, terminate this standalone service.
        // This releases resources without falsely claiming a stopped ACK.
        DispatchQueue.global().asyncAfter(deadline:.now() + .seconds(5)) { [weak self] in
            guard let self else{return}
            self.condition.lock();let complete=self.stopCompleted;self.condition.unlock()
            if !complete {log("capture_stop_deadline_exceeded");_exit(1)}
        }
    }
    func performStop() {
        identityTimer?.cancel();identityTimer=nil
        for observer in workspaceObservers {NSWorkspace.shared.notificationCenter.removeObserver(observer)}
        workspaceObservers=[]
        if starting {return} // start completion invokes us; no start/stop overlap.
        // A second queued stop must not turn a moved/nil reference into proof.
        // The stop callback and sample-queue drain own completion once begun.
        if stopPending || stoppingStream != nil || callbacksDraining || stopCompleted {return}
        guard let stream else {
            if streamCreated {completeStop(Failure(code:"missing_stream_cleanup_proof"))}
            else {completeStop(nil)}
            return
        }
        stopPending=true;self.stream=nil
        if delegateStopped {finishAfterCallbacks(stream,basis:"delegate_reported_stopped");return}
        stoppingStream=stream
        stream.stopCapture { error in self.control.async {
            // Retain the stream until its stop callback has actually completed.
            if self.delegateStopped {
                if let error {self.secondaryStopError=self.safeError(error)}
                self.finishAfterCallbacks(stream,basis:"delegate_reported_stopped")
            } else if error==nil {self.finishAfterCallbacks(stream,basis:"stop_capture_completed")}
            else {self.completeStop(error)} // Retain unconfirmed stream in quarantine.
        }}
    }
    func finishAfterCallbacks(_ stream:SCStream,basis:String) {
        callbacksDraining=true
        // An identity-checked delegate notification proves capture stopped.
        // Detach output and drain callbacks before reporting local cleanup.
        if outputInstalled {
            do {try stream.removeStreamOutput(self,type:.screen);outputInstalled=false}
            catch {stoppingStream=stream;completeStop(error);return}
        }
        stoppingStream=nil
        samples.async {self.control.async {
            self.cleanupBasisOverride=basis;self.completeStop(nil)
        }}
    }
    func completeStop(_ error:Error?) {
        condition.lock()
        guard !stopCompleted else{condition.unlock();return}
        let basis=cleanupBasisOverride ?? (!streamCreated ? "no_stream_created":nil)
        let validProof:Bool
        if streamCreated {
            validProof=callbacksDraining && !outputInstalled && ["stop_capture_completed","delegate_reported_stopped"].contains(basis ?? "")
        } else {validProof=basis=="no_stream_created" && !outputInstalled && stream==nil && stoppingStream==nil}
        let error:Error?=error ?? (!validProof ? Failure(code:"missing_stream_cleanup_proof"):nil)
        // Persist truth before attempting delivery. A dead transport only loses
        // its receipt; it does not erase an independently completed Guest stop.
        if let captureInstance {ledger.finish(captureInstance,cleanBasis:error==nil ? basis:nil,retainOnFailure:self)}
        if !connectionDead {
            if let error {var event:[String:Any]=["event":"error","code":"stop_capture_failed"];event.merge(safeError(error)){_,new in new};event.merge(receiptFields()){_,new in new};events.append(jsonBody(event))}
            else if captureFailure != nil,let id=settings?.id {
                var terminal:[String:Any]=["event":"terminated","id":id,"reason":"capture_failed","cleanup_confirmed":true,
                                          "cleanup_basis":basis!,
                                          "console_identity_reads":identityReads,"console_identity_read_max_us":identityReadMaxNS/1000,"console_identity_diagnostic":diagnosticLocked()]
                if !secondaryStopError.isEmpty {terminal["secondary_stop_error"]=secondaryStopError}
                terminal.merge(receiptFields()){_,new in new}
                events.append(jsonBody(terminal))
            }
            else if let id=stopID {
                var event:[String:Any]=["event":"stopped","id":id,"frames_received":sequence,"frames_replaced":replaced,
                                       "console_identity_reads":identityReads,"console_identity_read_max_us":identityReadMaxNS/1000,"console_identity_diagnostic":diagnosticLocked()]
                event.merge(receiptFields()){_,new in new};events.append(jsonBody(event))
            }
        }
        stopCompleted=true;latest=nil;let writerDone=writerFinished;condition.broadcast();condition.unlock()
        if writerDone {finished.signal()}
    }
    func stream(_ stream:SCStream,didStopWithError error:Error) {
        // Public SCStream.h documents this callback as notification that the
        // stream HAS stopped. SCError.h -3815 means no capture source, not a
        // general lock-state signal. Do not issue a second stop solely because
        // this already-stopped notification contains an error.
        // https://developer.apple.com/documentation/screencapturekit/scstreamdelegate/stream(_:didstopwitherror:)
        control.async {
            // A stale callback from a released generation must never attest
            // cleanup for a different/current SCStream.
            guard self.stream === stream || self.stoppingStream === stream else{return}
            self.delegateStopped=true
            self.fail("capture_stopped_with_error",error,duringStop:true)
        }
    }
    func stream(_ stream:SCStream,didOutputSampleBuffer sample:CMSampleBuffer,of type:SCStreamOutputType) {
        guard type == .screen,CMSampleBufferIsValid(sample),
              let attachments=CMSampleBufferGetSampleAttachmentsArray(sample,createIfNecessary:false) as? [[SCStreamFrameInfo:Any]],
              let rawStatus=attachments.first?[.status] as? Int,SCFrameStatus(rawValue:rawStatus) == .complete,
              let pixel=CMSampleBufferGetImageBuffer(sample) else{return}
        let timestamp=CMTimeConvertScale(CMSampleBufferGetPresentationTimeStamp(sample),timescale:1_000_000_000,method:.default)
        guard timestamp.isValid,timestamp.isNumeric,timestamp.value>=0 else {fail("invalid_sample_timestamp");return}
        receivePixel(pixel,pts:UInt64(timestamp.value))
    }
    func receivePixel(_ pixel:CVPixelBuffer,pts:UInt64) {
        lockSample()
        guard accepting && !stopping && !connectionDead,let config=settings else {condition.unlock();return}
        let admittedAt=clock()
        guard identityFresh(verifiedAt:identityVerifiedAt,at:admittedAt) else {
            staleLocked("sample_admission",verifiedAt:identityVerifiedAt,at:admittedAt)
            condition.unlock();fail("console_identity_verification_stale");return
        }
        if let lastPTS,pts<=lastPTS {condition.unlock();return}
        if latest != nil {replaced+=1;latest=nil}
        let generation=sampleGeneration
        sequence+=1;let frameSequence=sequence
        condition.unlock()
        let began=clock()
        let copied:Result<Data,Error>
        do {copied = .success(try copyFrame(pixel,frameSequence,pts,config.width,config.height))}
        catch {copied = .failure(error)}
        let ended=clock()
        lockSample()
        if ended>=began {sampleCopyMaxNS=max(sampleCopyMaxNS,ended-began)}
        // A stopped/retired generation discards both data and copy errors;
        // neither late success nor late failure can revive/change its outcome.
        guard accepting && !stopping && !connectionDead && sampleGeneration==generation else {condition.unlock();return}
        let publishedAt=clock()
        guard identityFresh(verifiedAt:identityVerifiedAt,at:publishedAt) else {
            staleLocked("sample_publish",verifiedAt:identityVerifiedAt,at:publishedAt)
            condition.unlock();fail("console_identity_verification_stale");return
        }
        if let lastPTS,pts<=lastPTS {condition.unlock();return}
        switch copied {
        case .success(let body):latest=body;lastPTS=pts;condition.signal();condition.unlock()
        case .failure(let error):condition.unlock();fail((error as? Failure)?.code ?? "frame_copy_failed")
        }
    }

}
func machineModel() -> String {
    var size=0;guard sysctlbyname("hw.model",nil,&size,nil,0)==0,size>0,size<1024 else{return "unknown"}
    var value=[CChar](repeating:0,count:size);guard sysctlbyname("hw.model",&value,&size,nil,0)==0 else{return "unknown"};return String(cString:value)
}
func serve() throws {
    guard machineModel().hasPrefix("VirtualMac"),getuid() != 0 else {throw Failure(code:"nonroot_virtual_mac_guest_required")}
    guard CGPreflightScreenCaptureAccess() else {throw Failure(code:"screen_recording_permission_required")}
    let listener=socket(AF_VSOCK,SOCK_STREAM,0);guard listener>=0 else{throw Failure(code:"vsock_socket_failed")}
    defer{close(listener)}
    _ = fcntl(listener,F_SETFD,FD_CLOEXEC)
    var address=sockaddr_vm();address.svm_len=UInt8(MemoryLayout<sockaddr_vm>.size);address.svm_family=sa_family_t(AF_VSOCK);address.svm_port=5001;address.svm_cid=UInt32.max
    let status=withUnsafePointer(to:&address){$0.withMemoryRebound(to:sockaddr.self,capacity:1){Darwin.bind(listener,$0,socklen_t(MemoryLayout<sockaddr_vm>.size))}}
    guard status==0,listen(listener,4)==0 else{throw Failure(code:"vsock_listen_failed")}
    // Management queries remain possible during capture/stop. Ledger admits at
    // most one capture independently of this bounded connection/parser pool.
    let capacity=DispatchSemaphore(value:4);log("listening_vsock_5001_host_cid2")
    while true {
        var peer=sockaddr_vm(),length=socklen_t(MemoryLayout<sockaddr_vm>.size)
        let fd=withUnsafeMutablePointer(to:&peer){$0.withMemoryRebound(to:sockaddr.self,capacity:1){accept(listener,$0,&length)}}
        if fd<0 {if errno==EINTR{continue};throw Failure(code:"accept_failed")}
        _ = fcntl(fd,F_SETFD,FD_CLOEXEC)
        guard length==MemoryLayout<sockaddr_vm>.size,peer.svm_family==AF_VSOCK,peer.svm_cid==2,capacity.wait(timeout:.now()) == .success else {close(fd);continue}
        DispatchQueue.global(qos:.userInitiated).async {CaptureClient(fd:fd).run();capacity.signal()}
    }
}
func selfTest() throws {
    let config=try Start(["id":"test","method":"start","width":128,"height":64,"fps":30])
    let identity=try GuestConsoleIdentityV1(securitySessionID:42,uid:501,consoleSet:nil,displayID:7,physicalWidth:2560,physicalHeight:1440,
        displayGeometrySHA256:consoleGeometryHash(displayID:7,width:2560,height:1440,bounds:CGRect(x:-1280,y:90,width:1280,height:720)))
    var boundRequest:[String:Any]=["id":"bound","method":"start","width":1280,"height":720,"fps":30,"expected_console_identity":identity.wire]
    let serialized=try JSONSerialization.data(withJSONObject:boundRequest)
    precondition(serialized.count<2048)
    let bound=try Start(JSONSerialization.jsonObject(with:serialized) as! [String:Any])
    precondition(bound.expectedConsoleIdentity==identity && config.expectedConsoleIdentity==nil)
    var receiptRequest=boundRequest;receiptRequest["cleanup_receipt"]=true
    precondition(try! Start(receiptRequest).cleanupReceipt)
    for bad:Any in [false,1,"true",NSNull()] {
        receiptRequest["cleanup_receipt"]=bad
        do {_=try Start(receiptRequest);fatalError("bad cleanup receipt opt-in accepted")}catch let error as Failure{precondition(error.code=="invalid_cleanup_receipt")}
    }
    let observed=ConsoleIdentityObservation(identity:identity,rejection:nil)
    let matching=try streamConsoleIdentity(observed,expected:identity),legacy=try streamConsoleIdentity(observed,expected:nil)
    precondition(matching==identity && legacy==identity)
    for wrong:Any in [NSNull(),true,"identity",["version":1],identity.wire.merging(["unexpected":true]){_,new in new}] {
        boundRequest["expected_console_identity"]=wrong
        do {_=try Start(boundRequest);fatalError("invalid identity accepted")}catch let failure as Failure {precondition(failure.code=="invalid_expected_console_identity")}
    }
    for key in ["security_session_id","uid","display_id","physical_width","physical_height","display_geometry_sha256","console_set"] {
        var wrong=identity.wire
        switch key {case "console_set":wrong[key]=0
        case "display_geometry_sha256":wrong[key]=String(repeating:"0",count:64)
        default:wrong[key]=(wrong[key] as! NSNumber).uint32Value+1}
        do {_=try streamConsoleIdentity(observed,expected:GuestConsoleIdentityV1(wire:wrong));fatalError("identity mismatch accepted")}
        catch let error as Failure {precondition(error.code=="console_identity_mismatch")}
    }
    do {_=try streamConsoleIdentity(ConsoleIdentityObservation(identity:nil,rejection:.consoleNotActive),expected:nil);fatalError("unavailable identity accepted")}
    catch let failure as Failure {precondition(failure.code=="console_identity_unavailable")}
    try validateStreamDisplay(identity,display:7,bounds:CGRect(x:-1280,y:90,width:1280,height:720),scale:2)
    for (id,rect,scale) in [(UInt32(8),CGRect(x:-1280,y:90,width:1280,height:720),2.0),(7,CGRect(x:0,y:90,width:1280,height:720),2.0),(7,CGRect(x:-1280,y:90,width:1280,height:720),1.0)] {
        do {try validateStreamDisplay(identity,display:id,bounds:rect,scale:scale);fatalError("wrong source geometry accepted")}
        catch let failure as Failure {precondition(failure.code=="console_geometry_mismatch")}
    }
    precondition(!identityFresh(verifiedAt:nil,at:1000) && !identityFresh(verifiedAt:1001,at:1000))
    precondition(identityFresh(verifiedAt:1000,at:identityFreshNS+1000) && !identityFresh(verifiedAt:1000,at:identityFreshNS+1001))
    for invalid:[String:Any] in [ ["id":"x","method":"start","width":true,"height":64,"fps":1], ["id":"x","method":"start","width":1922,"height":64,"fps":1], ["id":"x","method":"start","width":65,"height":64,"fps":1] ] {
        do {_ = try Start(invalid);fatalError("invalid start accepted")} catch is Failure {}
    }
    var pixel:CVPixelBuffer?
    precondition(CVPixelBufferCreate(nil,config.width,config.height,kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,[kCVPixelBufferBytesPerRowAlignmentKey:256] as CFDictionary,&pixel)==kCVReturnSuccess)
    let value=pixel!;precondition(CVPixelBufferLockBaseAddress(value,[])==kCVReturnSuccess)
    for plane in 0..<2 {
        let stride=CVPixelBufferGetBytesPerRowOfPlane(value,plane),rows=CVPixelBufferGetHeightOfPlane(value,plane)
        let base=CVPixelBufferGetBaseAddressOfPlane(value,plane)!
        memset(base,0xee,stride*rows)
        for row in 0..<rows {memset(base.advanced(by:row*stride),plane==0 ? 0x10:0x80,config.width)}
    }
    CVPixelBufferUnlockBaseAddress(value,[])
    let body=try frameBody(value,sequence:7,pts:123456,width:128,height:64)
    precondition(body.count==33+128*64*3/2 && body[0]==2 && body[8]==7)
    func number(_ offset:Int,_ count:Int) -> UInt64 {body[offset..<offset+count].reduce(UInt64(0)){($0<<8)|UInt64($1)}}
    precondition(number(1,8)==7 && number(9,8)==123456 && number(17,4)==128 && number(21,4)==64)
    precondition(number(25,4)==8192 && number(29,4)==4096)
    precondition(body[33..<33+128*64].allSatisfy{$0==0x10})
    precondition(body[(33+128*64)...].allSatisfy{$0==0x80})
    var pair=[Int32](repeating:0,count:2);precondition(socketpair(AF_UNIX,SOCK_STREAM,0,&pair)==0)
    defer{close(pair[0]);close(pair[1])}
    _ = fcntl(pair[0],F_SETFL,O_NONBLOCK)
    var capacity:Int32=65536
    precondition(setsockopt(pair[0],SOL_SOCKET,SO_SNDBUF,&capacity,socklen_t(MemoryLayout<Int32>.size))==0)
    precondition(writeRecord(pair[0],body))
    var received=Data(),buffer=[UInt8](repeating:0,count:16384)
    while received.count<body.count+4 {
        let n=read(pair[1],&buffer,min(buffer.count,body.count+4-received.count));precondition(n>0);received.append(contentsOf:buffer.prefix(n))
    }
    precondition(received.prefix(4).reduce(UInt32(0)){($0<<8)|UInt32($1)}==body.count && received.dropFirst(4)==body)
    var small:Int32=4096;_ = setsockopt(pair[0],SOL_SOCKET,SO_SNDBUF,&small,socklen_t(MemoryLayout<Int32>.size))
    let began=now();precondition(!writeRecord(pair[0],Data(repeating:0,count:maximumBody),timeoutNS:100_000_000))
    precondition(now()-began<500_000_000)
    // A completed client must not be retained for the five-second watchdog.
    // No SCStream is created here; this covers pre-start teardown ownership.
    var temporary:CaptureClient?=CaptureClient(fd:pair[0])
    weak var released=temporary
    temporary?.stop(id:nil);temporary?.control.sync {};temporary=nil
    precondition(released==nil,"completed client retained by watchdog")
    // Early failure has no stream to stop: it emits a distinct terminal
    // attestation, never inventing a host-stop acknowledgement.
    let early=CaptureClient(fd:pair[0]);early.settings=config
    early.fail("main_display_unavailable")
    early.control.sync {}
    early.condition.lock();let notifications=early.events;early.condition.unlock()
    precondition(notifications.count==2)
    let terminal=try JSONSerialization.jsonObject(with:notifications[1].dropFirst()) as! [String:Any]
    precondition(terminal["event"] as? String=="terminated" && terminal["id"] as? String=="test")
    precondition(terminal["cleanup_basis"] as? String=="no_stream_created" && terminal["cleanup_confirmed"] as? Bool==true)
    let mismatch=CaptureClient(fd:pair[0]);mismatch.settings=bound
    mismatch.fail("console_identity_mismatch");mismatch.control.sync {}
    mismatch.condition.lock();let rejected=mismatch.events;mismatch.condition.unlock()
    precondition(rejected.count==2)
    let mismatchError=try JSONSerialization.jsonObject(with:rejected[0].dropFirst()) as! [String:Any]
    let mismatchTerminal=try JSONSerialization.jsonObject(with:rejected[1].dropFirst()) as! [String:Any]
    precondition(mismatchError["code"] as? String=="console_identity_mismatch" && mismatchError["id"] as? String=="bound")
    precondition(mismatchTerminal["event"] as? String=="terminated" && mismatchTerminal["cleanup_basis"] as? String=="no_stream_created")
    // A stale authority check must suppress an already-queued frame at the real
    // writer boundary. This socket has no SCK producer; it tests transport only.
    var stalePair=[Int32](repeating:0,count:2);precondition(socketpair(AF_UNIX,SOCK_STREAM,0,&stalePair)==0)
    defer{close(stalePair[0]);close(stalePair[1])}
    _=fcntl(stalePair[0],F_SETFL,O_NONBLOCK)
    let stale=CaptureClient(fd:stalePair[0]);stale.settings=bound;stale.accepting=true;stale.latest=body;stale.identityVerifiedAt=0
    DispatchQueue.global().async{stale.writer()}
    precondition(stale.finished.wait(timeout:.now() + .seconds(2)) == .success)
    var staleBytes=Data()
    while true {let n=read(stalePair[1],&buffer,buffer.count);if n==0{break};precondition(n>0);staleBytes.append(contentsOf:buffer.prefix(n))}
    var records:[[String:Any]]=[],offset=0
    while offset<staleBytes.count {
        precondition(offset+4<=staleBytes.count)
        let length=staleBytes[offset..<offset+4].reduce(0){($0<<8)|Int($1)};offset+=4
        precondition(length>1 && offset+length<=staleBytes.count && staleBytes[offset]==1,"Stale frame escaped onto wire")
        records.append(try JSONSerialization.jsonObject(with:staleBytes[(offset+1)..<(offset+length)]) as! [String:Any]);offset+=length
    }
    precondition(records.count==2 && records[0]["code"] as? String=="console_identity_verification_stale" && records[1]["event"] as? String=="terminated")
    // No SCK APIs: deterministic ledger and explicit stop-phase regression tests.
    var tick:UInt64=100
    let ledger=CleanupLedger(ttl:1000,limit:2,clock:{tick})
    func request(_ capture:String,_ service:String?=nil)->[String:Any] {["id":"query","method":"cleanup_status","service_instance":service ?? ledger.serviceInstance,"capture_instance":capture]}
    func status(_ capture:String)->String {(try! ledger.query(request(capture)))["status"] as! String}
    let a=try ledger.begin();precondition(status(a)=="active")
    do {_=try ledger.begin();fatalError("parallel capture accepted")}catch let error as Failure{precondition(error.code=="capture_busy")}
    let restarted=CleanupLedger()
    let different=try restarted.query(request(a));precondition(different["status"] as? String=="service_mismatch" && different["cleanup_confirmed"] as? Bool==false)
    for malformed in [request("bad"),request(a,String(repeating:"A",count:32)),request(a).merging(["stop":true]){_,new in new}] {
        do {_=try ledger.query(malformed);fatalError("malformed cleanup query accepted")}catch let error as Failure{precondition(error.code=="invalid_cleanup_query")}
    }
    let owner=NSObject();ledger.finish(a,cleanBasis:"no_stream_created",retainOnFailure:owner)
    let firstQuery=try ledger.query(request(a)),repeatQuery=try ledger.query(request(a))
    precondition(firstQuery["cleanup_confirmed"] as? Bool==true && NSDictionary(dictionary:firstQuery)==NSDictionary(dictionary:repeatQuery))
    tick+=1;let b=try ledger.begin();precondition(b != a && status(b)=="active");ledger.finish(b,cleanBasis:"stop_capture_completed",retainOnFailure:owner)
    tick+=1;let c=try ledger.begin();ledger.finish(c,cleanBasis:"delegate_reported_stopped",retainOnFailure:owner)
    precondition(status(a)=="unknown" && status(b)=="clean" && status(c)=="clean","bounded terminal eviction failed")
    tick+=1000;precondition(status(b)=="unknown" && status(c)=="unknown","expired proof must be unknown")
    let failed=try ledger.begin();ledger.finish(failed,cleanBasis:nil,retainOnFailure:owner)
    precondition(status(failed)=="failed")
    ledger.finish(failed,cleanBasis:"stop_capture_completed",retainOnFailure:owner)
    precondition(status(failed)=="failed","late callback must not upgrade quarantined failure")
    tick+=1000;precondition(status(failed)=="unknown")
    do {_=try ledger.begin();fatalError("TTL lifted failed-cleanup quarantine")}catch let error as Failure{precondition(error.code=="capture_cleanup_quarantined")}
    let interruptedLedger=CleanupLedger(),interrupted=CaptureClient(fd:pair[0],ledger:interruptedLedger)
    let interruptedID=try interruptedLedger.begin();interrupted.captureInstance=interruptedID;interrupted.settings=bound;interrupted.connectionDead=true;interrupted.streamCreated=true
    interrupted.starting=true;interrupted.performStop();precondition(!interrupted.stopCompleted)
    // Models the first queued performStop after start completion: stream moved
    // to asynchronous stop ownership; the duplicate queued call sees nil stream.
    interrupted.starting=false;interrupted.stopPending=true
    interrupted.performStop();interrupted.performStop();precondition(!interrupted.stopCompleted)
    let pendingQuery:[String:Any]=["id":"q","method":"cleanup_status","service_instance":interruptedLedger.serviceInstance,"capture_instance":interruptedID]
    precondition((try! interruptedLedger.query(pendingQuery))["status"] as? String=="active")
    interrupted.callbacksDraining=true;interrupted.stopPending=false;interrupted.performStop();precondition(!interrupted.stopCompleted)
    // Only the explicit post-detach/post-drain proof can complete this state.
    interrupted.cleanupBasisOverride="stop_capture_completed";interrupted.completeStop(nil)
    precondition((try! interruptedLedger.query(pendingQuery))["cleanup_confirmed"] as? Bool==true && interrupted.events.isEmpty,"disconnect must retain clean proof without inventing transport delivery")
    for missingProof in [false,true] {
        let badLedger=CleanupLedger(),bad=CaptureClient(fd:pair[0],ledger:badLedger)
        let identifier=try badLedger.begin();bad.captureInstance=identifier;bad.settings=bound;bad.streamCreated=true
        bad.completeStop(missingProof ? nil:Failure(code:"injected_stop_or_detach_error"))
        let q:[String:Any]=["id":"q","method":"cleanup_status","service_instance":badLedger.serviceInstance,"capture_instance":identifier]
        precondition((try! badLedger.query(q))["status"] as? String=="failed")
        do {_=try badLedger.begin();fatalError("unconfirmed stop released capture admission")}catch let error as Failure{precondition(error.code=="capture_cleanup_quarantined")}
    }
    for missingDrain in [false,true] {
        let badLedger=CleanupLedger(),bad=CaptureClient(fd:pair[0],ledger:badLedger)
        let identifier=try badLedger.begin();bad.captureInstance=identifier;bad.streamCreated=true
        bad.cleanupBasisOverride="stop_capture_completed";bad.callbacksDraining = !missingDrain;bad.outputInstalled = !missingDrain
        bad.completeStop(nil)
        let q:[String:Any]=["id":"q","method":"cleanup_status","service_instance":badLedger.serviceInstance,"capture_instance":identifier]
        precondition((try! badLedger.query(q))["status"] as? String=="failed","stop callback alone without detach and drain is insufficient")
    }
    // Management connection works while another capture is active and does not
    // stop it. Exercise the actual JSONL reader/framed writer/EOF, not a mock API.
    var managementPair=[Int32](repeating:0,count:2);precondition(socketpair(AF_UNIX,SOCK_STREAM,0,&managementPair)==0)
    defer{close(managementPair[1])}
    let activeLedger=CleanupLedger(),activeID=try activeLedger.begin(),managementDone=DispatchSemaphore(value:0)
    DispatchQueue.global().async{CaptureClient(fd:managementPair[0],ledger:activeLedger).run();managementDone.signal()}
    let managementRequest:[String:Any]=["id":"wire-query","method":"cleanup_status","service_instance":activeLedger.serviceInstance,"capture_instance":activeID]
    var queryData=try JSONSerialization.data(withJSONObject:managementRequest);queryData.append(10)
    precondition(queryData.withUnsafeBytes{write(managementPair[1],$0.baseAddress!,queryData.count)}==queryData.count)
    precondition(managementDone.wait(timeout:.now() + .seconds(2)) == .success)
    var queryBytes=Data()
    while true {let n=read(managementPair[1],&buffer,buffer.count);if n==0{break};precondition(n>0);queryBytes.append(contentsOf:buffer.prefix(n))}
    precondition(queryBytes.count>=5 && queryBytes[4]==1)
    let queried=try JSONSerialization.jsonObject(with:queryBytes.dropFirst(5)) as! [String:Any]
    precondition(queried["id"] as? String=="wire-query" && queried["status"] as? String=="active" && queried["cleanup_confirmed"] as? Bool==false)
    precondition((try! activeLedger.query(managementRequest))["status"] as? String=="active")
    print("PASS synthetic NV12 padded rows/header, strict legacy/bound start/identity, source geometry, freshness deadline, stale-frame real socket suppression/terminal cleanup and bounded backpressure. No capture or permission API invoked.")
    print("PASS cleanup ledger exact identities/grammar, TTL/eviction/restart, immutable failed quarantine, duplicate stop/start/drain phases, disconnected proof retention and active management JSONL/framed response/EOF. No actual SCK stop was simulated as Guest acceptance.")
}
@main struct MacOSCaptureStreamMain {
static func main() {
let args=Array(CommandLine.arguments.dropFirst())
do {
    if args==["--copy-lock-test"] {try copyLockTest();try copyPublicationTests()}
    else if args==["--self-test"] {try selfTest()}
    else if args==["--serve"] {
        DispatchQueue.global(qos:.userInitiated).async {
            do {try serve()} catch {log((error as? Failure)?.code ?? "unexpected_failure");exit(1)}
        }
        dispatchMain()
    }
    else {throw Failure(code:"usage_macos_capture_stream_serve_or_self_test")}
} catch {log((error as? Failure)?.code ?? "unexpected_failure");exit(1)}

}
}
