import Foundation
import Darwin
import CoreGraphics
import CoreMedia
import CoreVideo
import ScreenCaptureKit

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
struct Start {
    let id:String,width:Int,height:Int,fps:Int
    init(_ request:[String:Any]) throws {
        guard Set(request.keys)==Set(["id","method","width","height","fps"]),request["method"] as? String=="start",
              let id=request["id"] as? String,!id.isEmpty,id.utf8.count<=128 else {throw Failure(code:"invalid_start")}
        func integer(_ key:String,_ range:ClosedRange<Int>) throws -> Int {
            guard let n=request[key] as? NSNumber,CFGetTypeID(n) != CFBooleanGetTypeID(),n.doubleValue.isFinite,
                  n.doubleValue.rounded()==n.doubleValue,range.contains(n.intValue) else {throw Failure(code:"invalid_start")}
            return n.intValue
        }
        self.id=id;width=try integer("width",64...1920);height=try integer("height",64...1080);fps=try integer("fps",1...30)
        guard width%2==0,height%2==0 else {throw Failure(code:"odd_dimensions")}
    }
}
// Copies only the visible row bytes; padding and IOSurface ownership never
// cross the wire. Called with the slot lock held after discarding old pending.
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
    let condition=NSCondition(),control=DispatchQueue(label:"capture.control"),samples=DispatchQueue(label:"capture.samples")
    let finished=DispatchSemaphore(value:0)
    var stream:SCStream?,latest:Data?,events:[Data]=[]
    var accepting=false,stopping=false,stopCompleted=false,connectionDead=false,startRequested=false,starting=false
    var writerFinished=false,stopID:String?,sequence:UInt64=0,replaced:UInt64=0,sent:UInt64=0,lastPTS:UInt64?
    var settings:Start?
    var captureFailure:String?,streamCreated=false,outputInstalled=false,delegateStopped=false
    var stoppingStream:SCStream?,cleanupBasisOverride:String?,secondaryStopError:[String:Any]=[:]
    init(fd:Int32) {self.fd=fd;super.init()}
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
                            let config=try Start(request)
                            condition.lock();startRequested=true;settings=config;condition.unlock()
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
                } else {line.append(byte);if line.count>1024 {fail("command_too_large");return}}
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
            var event:[String:Any]=["event":"error","code":code]
            if let id=settings?.id {event["id"]=id}
            if let error {
                // Do not relay localized descriptions, userInfo or arbitrary
                // NSError domain text across the diagnostics boundary.
                event.merge(safeError(error)){_,new in new}
            }
            event.merge(metadata){_,new in new}
            if events.count<2 {events.append(jsonBody(event));condition.signal()}
        }
        condition.unlock();stop(id:nil)
    }
    func abortConnection() {
        condition.lock();connectionDead=true;latest=nil;events=[];condition.broadcast();condition.unlock()
        _ = shutdown(fd,SHUT_RDWR);stop(id:nil)
    }
    func start(_ config:Start) {
        condition.lock();let cancelled=stopping;condition.unlock();if cancelled{return}
        guard CGPreflightScreenCaptureAccess() else {fail("screen_recording_permission_required");return}
        SCShareableContent.getExcludingDesktopWindows(false,onScreenWindowsOnly:true) { content,error in
            self.control.async {
                self.condition.lock();let cancelled=self.stopping;self.condition.unlock();if cancelled{return}
                let main=CGMainDisplayID()
                let state:[String:Any]=["main_display_active":CGDisplayIsActive(main) != 0,"main_display_asleep":CGDisplayIsAsleep(main) != 0]
                guard error==nil else {self.fail("shareable_content_failed",error,state);return}
                guard let display=content?.displays.first(where:{$0.displayID==main}) else {self.fail("main_display_unavailable",nil,state);return}
                let filter=SCContentFilter(display:display,excludingWindows:[])
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
                    let bounds=filter.contentRect
                    self.condition.lock()
                    if self.stopping {self.condition.unlock();self.performStop();return}
                    self.events.append(jsonBody(["event":"started","id":config.id,"width":config.width,"height":config.height,"fps_limit":config.fps,
                      "pixel_format":"nv12-video-range","y_stride":config.width,"uv_stride":config.width,"clock":clockName,
                      "color_matrix":"ITU-R BT.709","color_space":"ITU-R BT.709","capture_dynamic_range":"SDR",
                      "source_display_id":display.displayID,"source_width_points":bounds.width,"source_height_points":bounds.height,
                      "source_point_pixel_scale":filter.pointPixelScale,"source_origin_x_points":bounds.origin.x,"source_origin_y_points":bounds.origin.y,
                      "preserves_aspect_ratio":true,"frame_header_bytes":33,"max_pending_frames":1,"max_inflight_frames":1]))
                    self.accepting=true;self.condition.signal();self.condition.unlock()
                }}
            }
        }
    }
    func stop(id:String?) {
        condition.lock()
        if stopping {condition.unlock();return}
        stopping=true;accepting=false;latest=nil;stopID=id;condition.unlock()
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
        if starting {return} // start completion invokes us; no start/stop overlap.
        guard let stream else {completeStop(nil);return}
        self.stream=nil
        if delegateStopped {finishAfterCallbacks(stream,basis:"delegate_reported_stopped");return}
        stoppingStream=stream
        stream.stopCapture { error in self.control.async {
            // Retain the stream until its stop callback has actually completed.
            if self.delegateStopped {
                if let error {self.secondaryStopError=self.safeError(error)}
                self.finishAfterCallbacks(stream,basis:"delegate_reported_stopped")
            } else if error==nil {self.finishAfterCallbacks(stream,basis:"stop_capture_completed")}
            else {self.stoppingStream=nil;self.completeStop(error)}
        }}
    }
    func finishAfterCallbacks(_ stream:SCStream,basis:String) {
        // An identity-checked delegate notification proves capture stopped.
        // Detach output and drain callbacks before reporting local cleanup.
        if outputInstalled {
            do {try stream.removeStreamOutput(self,type:.screen);outputInstalled=false}
            catch {stoppingStream=nil;completeStop(error);return}
        }
        stoppingStream=nil
        samples.async {self.control.async {
            self.cleanupBasisOverride=basis;self.completeStop(nil)
        }}
    }
    func completeStop(_ error:Error?) {
        condition.lock()
        guard !stopCompleted else{condition.unlock();return}
        if !connectionDead {
            if let error {var event:[String:Any]=["event":"error","code":"stop_capture_failed"];event.merge(safeError(error)){_,new in new};events.append(jsonBody(event))}
            else if captureFailure != nil,let id=settings?.id {
                var terminal:[String:Any]=["event":"terminated","id":id,"reason":"capture_failed","cleanup_confirmed":true,
                                          "cleanup_basis":cleanupBasisOverride ?? (streamCreated ? "stop_capture_completed":"no_stream_created")]
                if !secondaryStopError.isEmpty {terminal["secondary_stop_error"]=secondaryStopError}
                events.append(jsonBody(terminal))
            }
            else if let id=stopID {events.append(jsonBody(["event":"stopped","id":id,"frames_received":sequence,"frames_replaced":replaced]))}
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
        condition.lock()
        guard accepting,let config=settings else {condition.unlock();return}
        let pts=UInt64(timestamp.value)
        if let lastPTS,pts<=lastPTS {condition.unlock();return}
        if latest != nil {replaced+=1;latest=nil}
        do {
            sequence+=1
            latest=try frameBody(pixel,sequence:sequence,pts:pts,width:config.width,height:config.height)
            lastPTS=pts
            condition.signal();condition.unlock()
        } catch {condition.unlock();fail((error as? Failure)?.code ?? "frame_copy_failed")}
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
    guard status==0,listen(listener,1)==0 else{throw Failure(code:"vsock_listen_failed")}
    let capacity=DispatchSemaphore(value:1);log("listening_vsock_5001_host_cid2")
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
    print("PASS synthetic NV12 padded-row packing, big-endian header, strict start bounds, bounded socket backpressure. No capture or permission API invoked.")
}
let args=Array(CommandLine.arguments.dropFirst())
do {
    if args==["--self-test"] {try selfTest()}
    else if args==["--serve"] {
        DispatchQueue.global(qos:.userInitiated).async {
            do {try serve()} catch {log((error as? Failure)?.code ?? "unexpected_failure");exit(1)}
        }
        dispatchMain()
    }
    else {throw Failure(code:"usage_macos_capture_stream_serve_or_self_test")}
} catch {log((error as? Failure)?.code ?? "unexpected_failure");exit(1)}
