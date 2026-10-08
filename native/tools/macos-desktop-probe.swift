import Foundation
import Darwin
import ScreenCaptureKit
import CoreGraphics
import ImageIO
import CryptoKit

// Guest-only diagnostic. Never grant host permissions or use a host screenshot
// as Guest evidence. This executable deliberately has no physical-Mac override.
final class Report {
    let lock=NSLock()
    var done=false
    var fields:[String:Any]=["success":false,"api":"ScreenCaptureKit.SCScreenshotManager","scope":"actual process console; VirtualMac guest required"]
    func set(_ values:[String:Any]) {lock.lock();fields.merge(values){_,new in new};lock.unlock()}
    func finish(_ values:[String:Any],code:Int32) -> Never {
        lock.lock()
        if done {lock.unlock();pthread_exit(nil)}
        done=true;fields.merge(values){_,new in new};let result=fields;lock.unlock()
        let data=(try? JSONSerialization.data(withJSONObject:result,options:.sortedKeys)) ?? Data("{\"success\":false,\"error\":\"serialization_failed\"}".utf8)
        FileHandle.standardOutput.write(data);FileHandle.standardOutput.write(Data([10]));exit(code)
    }
}
let report=Report()
func fail(_ code:String,_ extra:[String:Any]=[:]) -> Never {var values=extra;values["error"]=code;values["success"]=false;report.finish(values,code:1)}
func model() -> String {
    var size=0
    guard sysctlbyname("hw.model",nil,&size,nil,0)==0,size>0,size<1024 else{return "unknown"}
    var bytes=[CChar](repeating:0,count:size)
    guard sysctlbyname("hw.model",&bytes,&size,nil,0)==0 else{return "unknown"}
    return String(cString:bytes)
}
func rect(_ value:CGRect) -> [String:Double] {["x":value.origin.x,"y":value.origin.y,"width":value.width,"height":value.height]}
func displayMetadata(_ id:CGDirectDisplayID) -> [String:Any] {
    var result:[String:Any]=["display_id":id,"desktop_bounds_points":rect(CGDisplayBounds(id)),"cg_pixels_wide":CGDisplayPixelsWide(id),"cg_pixels_high":CGDisplayPixelsHigh(id),"main":CGDisplayIsMain(id) != 0,"active":CGDisplayIsActive(id) != 0]
    if let mode=CGDisplayCopyDisplayMode(id) {result["mode_width"]=mode.width;result["mode_height"]=mode.height;result["mode_pixel_width"]=mode.pixelWidth;result["mode_pixel_height"]=mode.pixelHeight}
    return result
}
var output:String?,displayID:UInt32?,timeoutMS=8000,requestPermission=false
var arguments=Array(CommandLine.arguments.dropFirst())
if arguments==["--help"] {print("macos-desktop-probe --output ABSOLUTE_NEW_PNG [--display-id ID] [--timeout-ms 1000..25000] [--request-permission]");exit(0)}
while !arguments.isEmpty {
    let key=arguments.removeFirst()
    if key=="--request-permission" {guard !requestPermission else{fail("duplicate_argument")};requestPermission=true;continue}
    guard !arguments.isEmpty else{fail("missing_argument")};let value=arguments.removeFirst()
    switch key {
    case "--output":guard output==nil,value.hasPrefix("/"),value.utf8.count<=4096,!value.utf8.contains(0) else{fail("invalid_output")};output=value
    case "--display-id":guard displayID==nil,let id=UInt32(value) else{fail("invalid_display_id")};displayID=id
    case "--timeout-ms":guard let value=Int(value),(1000...25000).contains(value) else{fail("invalid_timeout")};timeoutMS=value
    default:fail("unknown_argument")
    }
}
guard let output else{fail("output_required")}
let machine=model()
report.set(["machine":machine,"uid":getuid(),"os_release":ProcessInfo.processInfo.operatingSystemVersionString,"timeout_ms":timeoutMS,"permission_requested":requestPermission])
guard machine.hasPrefix("VirtualMac") else{fail("virtual_mac_guest_required")}
guard getuid() != 0 else{fail("run_as_console_user_not_root")}
umask(0o077)
let timer=DispatchSource.makeTimerSource(queue:DispatchQueue.global(qos:.utility))
timer.schedule(deadline: .now() + .milliseconds(timeoutMS))
timer.setEventHandler {fail("deadline_exceeded")};timer.resume()
let before=CGPreflightScreenCaptureAccess();report.set(["screen_recording_preflight_before":before])
if !before && requestPermission {
    report.set(["screen_recording_request_result":CGRequestScreenCaptureAccess()])
}
let permitted=CGPreflightScreenCaptureAccess();report.set(["screen_recording_preflight":permitted])
var displays=[CGDirectDisplayID](repeating:0,count:16),count:UInt32=0
let displayStatus=CGGetActiveDisplayList(UInt32(displays.count),&displays,&count)
report.set(["display_query_status":displayStatus.rawValue,"displays":displays.prefix(Int(count)).map(displayMetadata)])
guard permitted else{fail("screen_recording_permission_required",["permission_action":"Approve Screen Recording for the responsible Guest app, then rerun; a process restart may be required."])}
guard displayStatus == .success,count>0 else{fail("no_active_display")}
let selected=displayID ?? CGMainDisplayID()
guard displays.prefix(Int(count)).contains(selected) else{fail("display_not_active")}
SCShareableContent.getExcludingDesktopWindows(false,onScreenWindowsOnly:true) { content,error in
    guard let content,error==nil else{let error=error as NSError?;fail("shareable_content_failed",["error_domain":error?.domain ?? "unknown","error_code":error?.code ?? 0])}
    guard let display=content.displays.first(where:{$0.displayID==selected}) else{fail("display_not_shareable")}
    let filter=SCContentFilter(display:display,excludingWindows:[])
    let scale=Double(filter.pointPixelScale),bounds=filter.contentRect
    let w=Double(bounds.width)*scale,h=Double(bounds.height)*scale
    guard scale.isFinite,scale>0,w.isFinite,h.isFinite,w>=1,h>=1,w<=4096,h<=4096,w*h<=8_388_608 else{fail("unsupported_capture_geometry")}
    let width=Int(w.rounded()),height=Int(h.rounded())
    report.set(["selected_display_id":selected,"sc_display_width_points":display.width,"sc_display_height_points":display.height,"sc_display_frame_points":rect(display.frame),"filter_content_rect_points":rect(bounds),"point_pixel_scale":scale,"requested_pixel_width":width,"requested_pixel_height":height,"coordinate_space":"display physical pixels; desktop origin in reported points"])
    let config=SCStreamConfiguration();config.width=width;config.height=height;config.showsCursor=true;config.scalesToFit=false
    if #available(macOS 15.0,*) {config.captureDynamicRange = .SDR}
    let requested=Date();let started=DispatchTime.now().uptimeNanoseconds
    SCScreenshotManager.captureImage(contentFilter:filter,configuration:config) { image,error in
        let captured=Date();let finished=DispatchTime.now().uptimeNanoseconds
        guard let image,error==nil else{let error=error as NSError?;fail("capture_failed",["error_domain":error?.domain ?? "unknown","error_code":error?.code ?? 0])}
        guard image.width==width,image.height==height else{fail("captured_size_mismatch",["actual_width":image.width,"actual_height":image.height])}
        let data=NSMutableData()
        guard let destination=CGImageDestinationCreateWithData(data,"public.png" as CFString,1,nil) else{fail("png_encoder_unavailable")}
        CGImageDestinationAddImage(destination,image,nil)
        guard CGImageDestinationFinalize(destination),data.length<=16*1024*1024 else{fail("png_failed_or_exceeds_16MiB")}
        let png=data as Data
        // O_EXCL and O_NOFOLLOW never replace an existing file/symlink. The
        // screenshot remains local to the Guest; JSON contains no image bytes.
        let fd=open(output,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0o600)
        guard fd>=0 else{fail("output_create_failed",["os_error":errno])}
        var ioError:Int32=0
        png.withUnsafeBytes { bytes in
            var offset=0
            while offset<bytes.count {
                let n=write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
                if n<0 && errno==EINTR {continue}
                if n<=0 {ioError=errno;break};offset+=n
            }
        }
        if ioError==0 && fsync(fd) != 0 {ioError=errno}
        close(fd)
        guard ioError==0 else{fail("output_write_failed",["os_error":ioError,"partial_output_may_exist":true])}
        let hash=SHA256.hash(data:png).map{String(format:"%02x",$0)}.joined()
        report.finish(["success":true,"frame_id":UUID().uuidString.lowercased(),"width":image.width,"height":image.height,"png_bytes":png.count,"sha256":hash,"output":output,"capture_requested_at_us":Int64(requested.timeIntervalSince1970*1_000_000),"capture_completed_at_us":Int64(captured.timeIntervalSince1970*1_000_000),"capture_duration_ms":Double(finished-started)/1_000_000,"capture_time_basis":"API completion; exact presentation timestamp unavailable","screen_recording_preflight_after":CGPreflightScreenCaptureAccess()],code:0)
    }
}
dispatchMain()
