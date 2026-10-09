import Foundation
import CoreVideo

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
