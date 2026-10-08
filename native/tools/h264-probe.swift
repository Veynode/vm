import Foundation
import VideoToolbox
import CoreMedia
import CoreVideo

func output(_ value:[String:Any]) { let data = try! JSONSerialization.data(withJSONObject:value,options:[.sortedKeys,.prettyPrinted]);print(String(data:data,encoding:.utf8)!) }
func abort(_ message:String) -> Never { output(["success":false,"error":message]);exit(1) }
final class Results {
    let lock = NSLock()
    var sizes:[Int] = []
    var errors:[Int32] = []
    var keyframes = 0
    func record(_ status:OSStatus,_ buffer:CMSampleBuffer?) {
        lock.lock();defer{lock.unlock()}
        if status != noErr { errors.append(status);return }
        guard let buffer, CMSampleBufferDataIsReady(buffer) else { errors.append(-1);return }
        sizes.append(CMSampleBufferGetTotalSampleSize(buffer))
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(buffer,createIfNecessary:false) as? [[String:Any]], attachments.first?[kCMSampleAttachmentKey_NotSync as String] as? Bool != true { keyframes += 1 }
    }
}
let results = Results()
let callback:VTCompressionOutputCallback = { ref,_,status,_,sample in
    guard let ref else { return };Unmanaged<Results>.fromOpaque(ref).takeUnretainedValue().record(status,sample)
}
let width:Int32 = 1920,height:Int32 = 1080
var session:VTCompressionSession?
let specification = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder as String:true] as CFDictionary
let attributes = [kCVPixelBufferPixelFormatTypeKey as String:kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,kCVPixelBufferWidthKey as String:width,kCVPixelBufferHeightKey as String:height,kCVPixelBufferIOSurfacePropertiesKey as String:[:] ] as CFDictionary
let status = VTCompressionSessionCreate(allocator:kCFAllocatorDefault,width:width,height:height,codecType:kCMVideoCodecType_H264,encoderSpecification:specification,imageBufferAttributes:attributes,compressedDataAllocator:nil,outputCallback:callback,refcon:Unmanaged.passUnretained(results).toOpaque(),compressionSessionOut:&session)
guard status == noErr, let session else { abort("Hardware H.264 session creation failed OSStatus=\(status)") }
defer { VTCompressionSessionInvalidate(session) }
let settings:[(CFString,CFTypeRef)] = [(kVTCompressionPropertyKey_RealTime,kCFBooleanTrue),(kVTCompressionPropertyKey_AllowFrameReordering,kCFBooleanFalse),(kVTCompressionPropertyKey_AverageBitRate,NSNumber(value:8_000_000)),(kVTCompressionPropertyKey_ExpectedFrameRate,NSNumber(value:60)),(kVTCompressionPropertyKey_ProfileLevel,kVTProfileLevel_H264_Main_AutoLevel)]
for (key,value) in settings { let status = VTSessionSetProperty(session,key:key,value:value);guard status == noErr else { abort("Property \(key) rejected status=\(status)") } }
guard VTCompressionSessionPrepareToEncodeFrames(session) == noErr else { abort("Prepare failed") }
var hardwareReference:Unmanaged<CFTypeRef>?
let hardwareStatus = withUnsafeMutablePointer(to:&hardwareReference) { pointer in
    VTSessionCopyProperty(session,key:kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,allocator:nil,valueOut:UnsafeMutableRawPointer(pointer))
}
guard hardwareStatus == noErr, let hardware = hardwareReference?.takeRetainedValue() as? NSNumber, hardware.boolValue else { abort("Encoder did not confirm hardware acceleration") }
guard let pool = VTCompressionSessionGetPixelBufferPool(session) else { abort("No pixel buffer pool") }
let start = DispatchTime.now().uptimeNanoseconds
let frames=120
for frame in 0..<frames {
    var buffer:CVPixelBuffer?
    guard CVPixelBufferPoolCreatePixelBuffer(nil,pool,&buffer) == kCVReturnSuccess, let buffer else { abort("Pixel buffer allocation failed") }
    CVPixelBufferLockBaseAddress(buffer,[])
    for plane in 0..<CVPixelBufferGetPlaneCount(buffer) {
        if let base = CVPixelBufferGetBaseAddressOfPlane(buffer,plane) { memset(base,plane == 0 ? Int32(16+(frame%200)):128,CVPixelBufferGetBytesPerRowOfPlane(buffer,plane)*CVPixelBufferGetHeightOfPlane(buffer,plane)) }
    }
    CVPixelBufferUnlockBaseAddress(buffer,[])
    let status = VTCompressionSessionEncodeFrame(session,imageBuffer:buffer,presentationTimeStamp:CMTime(value:Int64(frame),timescale:60),duration:CMTime(value:1,timescale:60),frameProperties:nil,sourceFrameRefcon:nil,infoFlagsOut:nil)
    guard status == noErr else { abort("Encode submission failed status=\(status)") }
    // Keep the validation probe bounded to one outstanding frame. This serial
    // correctness workload deliberately does not claim streaming throughput.
    guard VTCompressionSessionCompleteFrames(session,untilPresentationTimeStamp:CMTime(value:Int64(frame),timescale:60)) == noErr else { abort("Frame completion failed") }
}
guard VTCompressionSessionCompleteFrames(session,untilPresentationTimeStamp:.invalid) == noErr else { abort("Final drain failed") }
let elapsed = Double(DispatchTime.now().uptimeNanoseconds-start)/1e6
results.lock.lock()
let count=results.sizes.count,bytes=results.sizes.reduce(0,+),errors=results.errors,keyframes=results.keyframes
results.lock.unlock()
guard errors.isEmpty,count == frames,bytes > 0 else { abort("Output mismatch: frames=\(count), errors=\(errors)") }
output(["success":true,"scope":"Host-only synthetic offline encode; not remote desktop, capture, WebRTC, latency, or Guest GPU evidence.","os":ProcessInfo.processInfo.operatingSystemVersionString,"codec":"H264","using_hardware_encoder":hardware.boolValue,"width":width,"height":height,"nominal_fps":60,"input_frames":frames,"output_frames":count,"output_bytes":bytes,"keyframes":keyframes,"total_wall_ms":elapsed,"real_time_requested":true,"frame_reordering":false])
