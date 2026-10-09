import Foundation
import CoreVideo
import CoreGraphics
import Darwin

// Controlled copy barrier through the actual production admission/copy path.
// No SCStream, console query, capture permission or Guest is used.
func copyLockTest() throws {
    let client=CaptureClient(fd:-1)
    client.settings=try Start(["id":"test","method":"start","width":128,"height":64,"fps":30])
    client.accepting=true;client.identityVerifiedAt=now()
    var pixel:CVPixelBuffer?
    precondition(CVPixelBufferCreate(nil,128,64,kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,nil,&pixel)==kCVReturnSuccess)
    let entered=DispatchSemaphore(value:0),release=DispatchSemaphore(value:0),copied=DispatchSemaphore(value:0),observer=DispatchSemaphore(value:0)
    client.copyFrame={p,s,t,w,h in
        entered.signal();precondition(release.wait(timeout:.now()+2) == .success)
        return try frameBody(p,sequence:s,pts:t,width:w,height:h)
    }
    let value=pixel!
    client.samples.async {client.receivePixel(value,pts:1);copied.signal()}
    precondition(entered.wait(timeout:.now()+2) == .success)
    client.control.async {client.condition.lock();client.condition.unlock();observer.signal()}
    let available = observer.wait(timeout:.now()+0.2) == .success
    release.signal();precondition(copied.wait(timeout:.now()+2) == .success)
    client.control.sync {}
    guard available else {throw Failure(code:"copy_blocks_identity_condition")}
    print("PASS authority condition remains available while actual frame copy is blocked; no SCK capture invoked")
}

private final class CaptureTestClock {
    let lock=NSLock()
    var tick:UInt64=1_000_000
    func read()->UInt64 {lock.lock();defer{lock.unlock()};return tick}
    func set(_ value:UInt64) {lock.lock();tick=value;lock.unlock()}
}
func copyPublicationTests() throws {
    let identity=try GuestConsoleIdentityV1(securitySessionID:42,uid:501,consoleSet:nil,displayID:7,physicalWidth:2560,physicalHeight:1440,
        displayGeometrySHA256:consoleGeometryHash(displayID:7,width:2560,height:1440,bounds:CGRect(x:-1280,y:90,width:1280,height:720)))
    let config=try Start(["id":"test","method":"start","width":128,"height":64,"fps":30])
    var pixel:CVPixelBuffer?
    precondition(CVPixelBufferCreate(nil,128,64,kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,nil,&pixel)==kCVReturnSuccess)
    let value=pixel!
    for mode in ["renew","stop","generation","stale","pts","late_error"] {
        let time=CaptureTestClock()
        let client=CaptureClient(fd:-1,clock:time.read,identityObserver:{ConsoleIdentityObservation(identity:identity,rejection:nil)})
        client.settings=config;client.captureIdentity=identity;client.identityVerifiedAt=time.read();client.accepting=true
        let entered=DispatchSemaphore(value:0),release=DispatchSemaphore(value:0),done=DispatchSemaphore(value:0)
        client.copyFrame={p,s,t,w,h in
            entered.signal();precondition(release.wait(timeout:.now()+2) == .success)
            if mode=="late_error" {throw Failure(code:"synthetic_copy_error")}
            return try frameBody(p,sequence:s,pts:t,width:w,height:h)
        }
        client.samples.async {client.receivePixel(value,pts:10);done.signal()}
        precondition(entered.wait(timeout:.now()+2) == .success)
        switch mode {
        case "renew":
            time.set(600_000_000)
            try client.control.sync {try client.publishAuthority(client.readIdentity(expected:identity))}
            time.set(900_000_000) // original verification stale; real fresh read permits publication
        case "stop","late_error":
            client.stop(id:"stop-test");client.control.sync {}
            precondition(client.sampleGeneration==2 && !client.accepting)
        case "generation":client.condition.lock();client.sampleGeneration+=1;client.condition.unlock()
        case "stale":time.set(time.read()+identityFreshNS+1)
        case "pts":client.condition.lock();client.lastPTS=11;client.condition.unlock()
        default:fatalError()
        }
        release.signal();precondition(done.wait(timeout:.now()+2) == .success);client.control.sync {}
        client.condition.lock()
        if mode=="renew" {precondition(client.latest != nil && client.lastPTS==10 && client.identityVerifiedAt==600_000_000)}
        else {precondition(client.latest==nil,"late copy published after invalidation")}
        if mode=="stale" {precondition(client.staleSite=="sample_publish" && client.captureFailure=="console_identity_verification_stale")}
        if mode=="late_error" || mode=="stop" {precondition(client.captureFailure==nil,"cancelled copy changed terminal outcome")}
        client.condition.unlock()
    }
    // A completed authority read cannot acquire a newer timestamp just because
    // writing its metrics waited for the condition. The two clock reads below
    // enclose only the authority call, and deliberately precede lock acquisition.
    let clockLock=NSLock(),ended=DispatchSemaphore(value:0),finished=DispatchSemaphore(value:0)
    var calls=0,current:UInt64=100
    let clock:()->UInt64={clockLock.lock();calls+=1;let n=calls;let value=n==1 ? 100:n==2 ? 200:current;clockLock.unlock();if n==2 {ended.signal()};return value}
    let delayed=CaptureClient(fd:-1,clock:clock,identityObserver:{ConsoleIdentityObservation(identity:identity,rejection:nil)})
    delayed.captureIdentity=identity
    delayed.condition.lock()
    var captured:CaptureClient.AuthorityRead?
    delayed.control.async {captured=try! delayed.readIdentity(expected:identity);finished.signal()}
    precondition(ended.wait(timeout:.now()+2) == .success)
    clockLock.lock();current=identityFreshNS+201;clockLock.unlock();delayed.condition.unlock()
    precondition(finished.wait(timeout:.now()+2) == .success);delayed.control.sync {}
    precondition(captured!.completed==200)
    do {try delayed.publishAuthority(captured!);fatalError("metrics lock renewed authority time")}
    catch let error as Failure {precondition(error.code=="console_identity_verification_stale")}
    precondition(delayed.identityVerifiedAt==nil && delayed.staleSite=="authority_publish")

    // Real framed socket: freshness expiring DURING copy must emit only error
    // and confirmed no-stream terminal; no copied NV12 record may escape.
    var pair=[Int32](repeating:0,count:2);precondition(socketpair(AF_UNIX,SOCK_STREAM,0,&pair)==0)
    defer{close(pair[0]);close(pair[1])}
    let time=CaptureTestClock(),wire=CaptureClient(fd:pair[0],clock:time.read)
    wire.settings=config;wire.accepting=true;wire.identityVerifiedAt=time.read()
    wire.copyFrame={p,s,t,w,h in time.set(time.read()+identityFreshNS+1);return try frameBody(p,sequence:s,pts:t,width:w,height:h)}
    wire.receivePixel(value,pts:12);wire.control.sync {}
    DispatchQueue.global().async {wire.writer()}
    precondition(wire.finished.wait(timeout:.now()+2) == .success)
    var bytes=Data(),buffer=[UInt8](repeating:0,count:4096)
    while true {let n=read(pair[1],&buffer,buffer.count);if n==0{break};precondition(n>0);bytes.append(contentsOf:buffer.prefix(n))}
    var offset=0,events:[[String:Any]]=[]
    while offset<bytes.count {
        precondition(offset+4<=bytes.count);let size=bytes[offset..<offset+4].reduce(0){($0<<8)|Int($1)};offset+=4
        precondition(size>1 && offset+size<=bytes.count && bytes[offset]==1)
        events.append(try JSONSerialization.jsonObject(with:bytes[(offset+1)..<(offset+size)]) as! [String:Any]);offset+=size
    }
    precondition(events.count==2 && events[0]["code"] as? String=="console_identity_verification_stale" && events[1]["cleanup_basis"] as? String=="no_stream_created")
    let diagnostic=events[0]["console_identity_diagnostic"] as! [String:Any]
    precondition(diagnostic["stale_site"] as? String=="sample_publish")
    precondition(Set(diagnostic.keys)==Set(["stale_site","verification_age_us","authority_read_max_us","identity_publish_lock_wait_max_us","sample_copy_max_us","sample_lock_wait_max_us","identity_timer_lateness_max_us","sample_generation"]))
    print("PASS blocked-copy renewal/stop/generation/PTS/late-error; authority completion is not renewed by metrics lock; expired publication emits zero frames on real socket. Synthetic, not SCK acceptance.")
}
