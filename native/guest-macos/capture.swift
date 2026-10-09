import Foundation
import Darwin
import ScreenCaptureKit
import CoreGraphics
import ImageIO
import CryptoKit

let maximumScreenshotBytes=700*1024
private let screenshotGate=DispatchSemaphore(value:1)
struct CaptureFailure: Error {let code:String;let message:String}
func observationPolicy(_ enabled:Bool) -> [String:Any] {
    let guest=systemString("hw.model").hasPrefix("VirtualMac") && getuid() != 0
    // No host capture/TCC check: the public service is intended for Guest execution.
    let permitted=enabled && guest ? CGPreflightScreenCaptureAccess():false
    return ["enabled":enabled,"screenshot_available":permitted,"screen_recording_permission":!enabled ? "not_checked_disabled":(!guest ? "not_checked_not_guest":(permitted ? "granted":"required")),"api":"ScreenCaptureKit.SCScreenshotManager","max_image_bytes":maximumScreenshotBytes,"mime_type":"image/jpeg","jpeg_quality_ladder":[0.8,0.65,0.5,0.35],"resizing":false,"max_capture_wait_ms":8000,"input":false]
}
struct EncodedScreenshot {let data:Data;let quality:Double}
func encodeScreenshotJPEG(_ image:CGImage,limit:Int=maximumScreenshotBytes) throws -> EncodedScreenshot {
    for quality in [0.8,0.65,0.5,0.35] {
        let data=NSMutableData()
        guard let destination=CGImageDestinationCreateWithData(data,"public.jpeg" as CFString,1,nil) else {throw CaptureFailure(code:"jpeg_encoder_unavailable",message:"JPEG encoder unavailable")}
        CGImageDestinationAddImage(destination,image,[kCGImageDestinationLossyCompressionQuality:quality] as CFDictionary)
        guard CGImageDestinationFinalize(destination) else {throw CaptureFailure(code:"jpeg_encoding_failed",message:"JPEG encoding failed")}
        if data.length<=limit {return EncodedScreenshot(data:data as Data,quality:quality)}
    }
    throw CaptureFailure(code:"image_too_large",message:"Full-resolution JPEG exceeds 700 KiB at all declared quality levels 0.80/0.65/0.50/0.35; image was not resized or returned")
}
private final class CaptureResult {
    let lock=NSLock(),ready=DispatchSemaphore(value:0)
    var value:[String:Any]?
    func finish(_ reply:[String:Any]) {lock.lock();guard value==nil else{lock.unlock();return};value=reply;lock.unlock();screenshotGate.signal();ready.signal()}
    func get() -> [String:Any]? {lock.lock();defer{lock.unlock()};return value}
}
func screenshotReply(_ id:String,enabled:Bool,registerInput:Bool=false) -> [String:Any] {
    guard enabled else{return errorReply(id,"forbidden","observation disabled; launch Guest service with --allow-observe")}
    guard systemString("hw.model").hasPrefix("VirtualMac"),getuid() != 0 else{return errorReply(id,"virtual_guest_required","capture is restricted to the signed-in non-root VirtualMac Guest user")}
    guard CGPreflightScreenCaptureAccess() else{return errorReply(id,"screen_recording_permission_required","Approve Screen Recording for the responsible Guest application, then restart if required; service never requests permission automatically")}
    let console:GuestConsoleIdentityV1
    do {console=try requireConsoleIdentity(GuestConsoleIdentityReader.observe())}
    catch let error as CaptureFailure {guestInput.invalidate();return errorReply(id,error.code,error.message)}
    catch {guestInput.invalidate();return errorReply(id,"console_identity_unavailable","Console identity is unavailable")}
    guard screenshotGate.wait(timeout:.now()) == .success else{return errorReply(id,"busy","one capture is already outstanding")}
    let result=CaptureResult(),deadline=DispatchTime.now() + .seconds(8)
    let inputCaptureToken=registerInput ? guestInput.captureToken():nil
    let selected=console.displayID
    func fail(_ code:String,_ message:String){result.finish(errorReply(id,code,message))}
    SCShareableContent.getExcludingDesktopWindows(false,onScreenWindowsOnly:true) {content,error in
        guard DispatchTime.now()<deadline else{fail("capture_timeout","capture deadline expired");return}
        guard let content,error==nil else{fail("shareable_content_failed","ScreenCaptureKit could not enumerate Guest console content");return}
        guard let display=content.displays.first(where:{$0.displayID==selected}),CGDisplayIsActive(selected) != 0 else{fail("display_unavailable","main Guest console display is not active/shareable");return}
        let filter=SCContentFilter(display:display,excludingWindows:[])
        let scale=Double(filter.pointPixelScale),bounds=filter.contentRect
        let w=Double(bounds.width)*scale,h=Double(bounds.height)*scale
        guard scale.isFinite,scale>0,w.isFinite,h.isFinite,w>=1,h>=1,w<=4096,h<=4096,w*h<=8_388_608 else{fail("unsupported_geometry","capture requires <=4096 pixels per axis and <=8388608 pixels; no resizing performed");return}
        let width=Int(w.rounded()),height=Int(h.rounded())
        do {try validateCaptureConsole(console,observation:GuestConsoleIdentityReader.observe(),display:selected,width:width,height:height,bounds:display.frame)}
        catch let error as CaptureFailure {guestInput.invalidate();fail(error.code,error.message);return}
        catch {guestInput.invalidate();fail("console_identity_unavailable","Console identity is unavailable");return}
        let config=SCStreamConfiguration();config.width=width;config.height=height;config.showsCursor=true;config.scalesToFit=false
        if #available(macOS 15.0,*){config.captureDynamicRange = .SDR}
        let requested=Date(),start=DispatchTime.now().uptimeNanoseconds
        SCScreenshotManager.captureImage(contentFilter:filter,configuration:config) {image,error in
            let completed=Date(),end=DispatchTime.now().uptimeNanoseconds
            guard DispatchTime.now()<deadline else{fail("capture_timeout","capture deadline expired");return}
            guard let image,error==nil else{fail("capture_failed","ScreenCaptureKit could not capture Guest console");return}
            guard image.width==width,image.height==height else{fail("geometry_mismatch","captured image does not match requested physical pixel dimensions");return}
            do {
                let encoded=try encodeScreenshotJPEG(image),jpeg=encoded.data
                // Re-read after encoding: an image from a prior user/session or
                // geometry must never become a fresh input anchor or leave Guest.
                try validateCaptureConsole(console,observation:GuestConsoleIdentityReader.observe(),display:selected,width:image.width,height:image.height,bounds:display.frame)
                var metadata:[String:Any]=["frame_id":UUID().uuidString.lowercased(),"mime_type":"image/jpeg","data_base64":jpeg.base64EncodedString(),"byte_length":jpeg.count,"sha256":SHA256.hash(data:jpeg).map{String(format:"%02x",$0)}.joined(),"width":image.width,"height":image.height,"source_width":width,"source_height":height,"image_width":image.width,"image_height":image.height,"display_id":selected,"coordinate_space":"guest-display-physical-pixels","desktop_bounds_points":["x":display.frame.origin.x,"y":display.frame.origin.y,"width":display.frame.width,"height":display.frame.height],"content_rect_points":["x":bounds.origin.x,"y":bounds.origin.y,"width":bounds.width,"height":bounds.height],"guest_ui_scale":scale,"point_pixel_scale":scale,"capture_requested_at_us":Int64(requested.timeIntervalSince1970*1_000_000),"captured_at_us":Int64(completed.timeIntervalSince1970*1_000_000),"captured_monotonic_us":end/1000,"capture_time_basis":"Guest API completion; exact presentation timestamp unavailable","capture_duration_ms":Double(end-start)/1_000_000,"jpeg_quality":encoded.quality,"jpeg_quality_ladder":[0.8,0.65,0.5,0.35],"resized":false,"includes_cursor":true]
                metadata["console_identity"]=console.wire
                metadata["console_identity_verified_at"]="capture_pre_enumeration_and_post_encode"
                if registerInput {
                    do {metadata["source_generation"]=try guestInput.record(metadata,captureToken:inputCaptureToken);metadata["input_anchor_available"]=true}
                    catch let error as GuestInputFailure {
                        if error.code=="input_console_identity_changed" || error.code=="input_console_geometry_mismatch" || error.code=="input_geometry_changed" {
                            throw CaptureFailure(code:"console_identity_changed",message:"Console identity changed before anchor publication; image discarded")
                        }
                        metadata["input_anchor_available"]=false;metadata["input_anchor_error"]=error.code;metadata["input_anchor_error_detail"]=error.message
                    }
                    catch {metadata["input_anchor_available"]=false;metadata["input_anchor_error"]="input_registration_failed"}
                }
                result.finish(["id":id,"result":metadata])
            } catch let error as CaptureFailure {if error.code.hasPrefix("console_"){guestInput.invalidate()};fail(error.code,error.message)} catch {fail("encoding_failed","could not encode screenshot")}
        }
    }
    guard result.ready.wait(timeout:deadline) == .success,let reply=result.get() else {
        // SCScreenshotManager has no cancellation API. Keep the single capture slot
        // reserved until its callback finishes; repeated timeouts cannot accumulate work.
        return errorReply(id,"capture_timeout","capture exceeded 8 seconds; outstanding capture remains busy until its callback ends")
    }
    return reply
}
func captureSelfTest() {
    let denied=screenshotReply("denied",enabled:false)
    precondition((denied["error"] as? [String:String])?["code"]=="forbidden")
    if !systemString("hw.model").hasPrefix("VirtualMac") {
        let outside=screenshotReply("host",enabled:true)
        precondition((outside["error"] as? [String:String])?["code"]=="virtual_guest_required")
    }
    let space=CGColorSpaceCreateDeviceRGB()
    let context=CGContext(data:nil,width:32,height:16,bitsPerComponent:8,bytesPerRow:128,space:space,bitmapInfo:CGImageAlphaInfo.noneSkipLast.rawValue)!
    context.setFillColor(CGColor(red:1,green:0,blue:0,alpha:1));context.fill(CGRect(x:0,y:0,width:32,height:16))
    let image=context.makeImage()!,encoded=try! encodeScreenshotJPEG(image),bytes=encoded.data
    let source=CGImageSourceCreateWithData(bytes as CFData,nil)!,decoded=CGImageSourceCreateImageAtIndex(source,0,nil)!
    precondition(decoded.width==32 && decoded.height==16 && bytes.count<=maximumScreenshotBytes)
    // Deterministic synthetic texture forces the quality ladder without any capture.
    let noisy=CGContext(data:nil,width:256,height:256,bitsPerComponent:8,bytesPerRow:1024,space:space,bitmapInfo:CGImageAlphaInfo.noneSkipLast.rawValue)!
    let pixels=noisy.data!.assumingMemoryBound(to:UInt8.self);var state:UInt32=12345
    for i in 0..<(256*256*4){state=1664525 &* state &+ 1013904223;pixels[i]=UInt8(truncatingIfNeeded:state>>24)}
    let noiseImage=noisy.makeImage()!,high=try! encodeScreenshotJPEG(noiseImage)
    let reduced=try! encodeScreenshotJPEG(noiseImage,limit:high.data.count-1)
    precondition(reduced.quality<high.quality && reduced.data.count<high.data.count)
    let reducedSource=CGImageSourceCreateWithData(reduced.data as CFData,nil)!,reducedImage=CGImageSourceCreateImageAtIndex(reducedSource,0,nil)!
    precondition(reducedImage.width==256 && reducedImage.height==256)
    do {_=try encodeScreenshotJPEG(image,limit:1);preconditionFailure("image limit not enforced")}catch let error as CaptureFailure {precondition(error.code=="image_too_large")}catch{preconditionFailure("wrong error")}
    print("PASS: screenshot disabled/physical-host guards, synthetic JPEG dimensions/explicit quality ladder and size limit; no desktop capture performed")
}
