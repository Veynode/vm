import Foundation
import CoreGraphics
import ScreenCaptureKit
import Darwin

let maximumWindowResultBytes = 48 * 1024
let maximumObservedWindows = 128
let maximumObservedDisplays = 16
struct WindowObservationFailure: Error { let code: String; let message: String }
private func windowFailure(_ code:String,_ message:String)->WindowObservationFailure { .init(code:code,message:message) }
func validWindowBounds(_ bounds:CGRect)->Bool {
    let values=[bounds.origin.x,bounds.origin.y,bounds.width,bounds.height].map(Double.init)
    return values.allSatisfy{$0.isFinite && abs($0)<=10_000_000} && bounds.width>0 && bounds.height>0
}
func windowBoundsWire(_ bounds:CGRect)->[String:Double] {
    ["x":Double(bounds.origin.x),"y":Double(bounds.origin.y),"width":Double(bounds.width),"height":Double(bounds.height)]
}
struct WindowDisplayObservation:Equatable {
    let id:UInt32, main:Bool, active:Bool, asleep:Bool, bounds:CGRect, pixelWidth:Int, pixelHeight:Int, rotation:Double
    var valid:Bool {id>0 && validWindowBounds(bounds) && pixelWidth>0 && pixelHeight>0 && pixelWidth<=16_384 && pixelHeight<=16_384 && rotation.isFinite && rotation>=0 && rotation<360}
    func wire(scale:Double?)->[String:Any] {
        ["display_id":id,"is_main":main,"active":active,"asleep":asleep,"bounds_points":windowBoundsWire(bounds),
         "mode_pixel_width":pixelWidth,"mode_pixel_height":pixelHeight,"rotation_degrees":rotation,
         "point_pixel_scale":scale.map{$0 as Any} ?? NSNull()]
    }
}
struct WindowCandidate {
    let id:UInt32, pid:Int32, bundle:String, application:String, bounds:CGRect
}
func windowIntersections(_ bounds:CGRect,_ displays:[WindowDisplayObservation])->[UInt32] {
    guard validWindowBounds(bounds) else{return []}
    return displays.filter{$0.active && !$0.asleep && !$0.bounds.intersection(bounds).isEmpty && !$0.bounds.intersection(bounds).isNull}.map(\.id).sorted()
}
func validateWindowObservation(expected:GuestConsoleIdentityV1,current:ConsoleIdentityObservation,
                               before:[WindowDisplayObservation],after:[WindowDisplayObservation],permission:Bool)throws {
    guard permission else {throw windowFailure("screen_recording_permission_required","Existing Guest Screen Recording permission is unavailable")}
    guard current.identityEligible,let identity=current.identity else {throw windowFailure("console_identity_unavailable","Current Guest console identity is unavailable")}
    guard identity==expected else {throw windowFailure("console_identity_changed","Guest console changed during window observation")}
    guard before==after else {throw windowFailure("display_configuration_changed","Guest display configuration changed during window observation")}
}
func windowObservationPolicy(enabled:Bool,guest:Bool,permission:Bool,identityEligible:Bool)->[String:Any] {
    ["enabled":enabled,"windows_available":enabled && guest && permission && identityEligible,
     "screen_recording_permission":!enabled ? "not_checked_disabled":(!guest ? "not_checked_not_guest":(permission ? "granted":"required")),
     "api":"ScreenCaptureKit.SCShareableContent + CoreGraphics.CGWindowListCopyWindowInfo",
     "filter":"on_screen_application_windows_layer_0","titles_included":false,"accessibility_tree":false,"input_authorization":false,
     "max_windows":maximumObservedWindows,"max_displays":maximumObservedDisplays,"max_result_bytes":maximumWindowResultBytes,
     "max_wait_ms":8000,"coordinate_space":"guest-desktop-global-points","window_ids":"snapshot-local observations; not input handles"]
}
func windowObservationPolicy(_ enabled:Bool,console:ConsoleIdentityObservation)->[String:Any] {
    let guest=getuid() != 0 && systemString("hw.model").hasPrefix("VirtualMac")
    return windowObservationPolicy(enabled:enabled,guest:guest,permission:enabled && guest ? CGPreflightScreenCaptureAccess():false,identityEligible:console.identityEligible)
}

// One outstanding OS enumeration, including callbacks that outlive the caller's
// deadline. A timeout does not free this slot or start another unbounded request.
final class WindowObservationSlot {
    private let lock=NSLock()
    private var token:UUID?
    func acquire()->UUID? {lock.lock();defer{lock.unlock()};guard token==nil else{return nil};let next=UUID();token=next;return next}
    func finish(_ finished:UUID) {lock.lock();defer{lock.unlock()};if token==finished {token=nil}}
}
private let windowObservationSlot=WindowObservationSlot()
private final class PendingWindowObservation {
    private let lock=NSLock()
    private var reply:[String:Any]?
    let ready=DispatchSemaphore(value:0)
    func finish(_ value:[String:Any]) {lock.lock();defer{lock.unlock()};guard reply==nil else{return};reply=value;ready.signal()}
    func value()->[String:Any]? {lock.lock();defer{lock.unlock()};return reply}
}

private func readWindowDisplays()throws->[WindowDisplayObservation] {
    var ids=[CGDirectDisplayID](repeating:0,count:maximumObservedDisplays+1),count:UInt32=0
    guard CGGetOnlineDisplayList(UInt32(ids.count),&ids,&count) == .success,count>0,Int(count)<=maximumObservedDisplays else {
        throw windowFailure("unsupported_geometry","Window observation requires one to sixteen readable online displays")
    }
    let main=CGMainDisplayID()
    return try ids.prefix(Int(count)).sorted().map { id in
        guard let mode=CGDisplayCopyDisplayMode(id) else {throw windowFailure("unsupported_geometry","Display mode is unavailable")}
        let display=WindowDisplayObservation(id:id,main:id==main,active:CGDisplayIsActive(id) != 0,asleep:CGDisplayIsAsleep(id) != 0,
            bounds:CGDisplayBounds(id),pixelWidth:mode.pixelWidth,pixelHeight:mode.pixelHeight,rotation:CGDisplayRotation(id))
        guard display.valid else {throw windowFailure("unsupported_geometry","Display geometry is not finite and bounded")}
        return display
    }
}
private func windowUInt32(_ value:Any?)->UInt32? {
    guard let n=value as? NSNumber,CFGetTypeID(n) != CFBooleanGetTypeID(),n.doubleValue.isFinite,n.doubleValue>0,
          n.doubleValue<=Double(UInt32.max),n.doubleValue.rounded()==n.doubleValue else{return nil}
    return n.uint32Value
}
private func readQuartzWindowBounds()throws->[UInt32:CGRect] {
    guard let windows=CGWindowListCopyWindowInfo([.optionOnScreenOnly,.excludeDesktopElements],kCGNullWindowID) as? [[String:Any]] else {
        throw windowFailure("shareable_content_failed","CoreGraphics could not enumerate Guest window geometry")
    }
    guard windows.count<=8192 else {throw windowFailure("shareable_content_failed","Window metadata exceeds bounded enumeration capacity")}
    var result:[UInt32:CGRect]=[:]
    for window in windows {
        guard let id=windowUInt32(window[kCGWindowNumber as String]),let wire=window[kCGWindowBounds as String] as? NSDictionary,
              let bounds=CGRect(dictionaryRepresentation:wire as CFDictionary),validWindowBounds(bounds) else{continue}
        result[id]=bounds
    }
    return result
}

func makeWindowResult(identity:GuestConsoleIdentityV1,displays:[WindowDisplayObservation],scales:[UInt32:Double],
                      candidates:[WindowCandidate],omitted:Int,requested:Int64,completed:Int64,monotonic:UInt64,
                      snapshot:String=UUID().uuidString.lowercased(),limit:Int=maximumWindowResultBytes)throws->[String:Any] {
    guard !displays.isEmpty,displays.count<=maximumObservedDisplays,displays.allSatisfy(\.valid),
          Set(displays.map(\.id)).count==displays.count,displays.filter(\.main).count==1,
          requested>=0,completed>=0,requested<=9_007_199_254_740_991,completed<=9_007_199_254_740_991,monotonic<=9_007_199_254_740_991 else {
        throw windowFailure("unsupported_geometry","Invalid bounded window observation metadata")
    }
    var invalid=omitted,eligible=0,entries:[[String:Any]]=[]
    var ids=Set<UInt32>()
    for candidate in candidates.sorted(by:{$0.id<$1.id}) {
        guard candidate.id>0,validWindowBounds(candidate.bounds),ids.insert(candidate.id).inserted else {invalid+=1;continue}
        let intersections=windowIntersections(candidate.bounds,displays)
        guard !intersections.isEmpty else {continue}
        eligible+=1
        guard candidate.pid>0,candidate.bundle.utf8.count<=256,candidate.application.utf8.count<=256,
              !candidate.bundle.utf8.contains(0),!candidate.application.utf8.contains(0) else {invalid+=1;continue}
        guard entries.count<maximumObservedWindows else{continue}
        entries.append(["window_id":candidate.id,"owner":["pid":candidate.pid,"bundle_identifier":candidate.bundle,"application_name":candidate.application],
                        "bounds_points":windowBoundsWire(candidate.bounds),"display_ids":intersections])
    }
    var result:[String:Any]=["version":1,"snapshot_id":snapshot,"console_identity":identity.wire,
        "coordinate_space":"guest-desktop-global-points","filter":"on_screen_application_windows_layer_0","titles_included":false,
        "accessibility_tree":false,"input_authorization":false,"requested_at_us":requested,"completed_at_us":completed,
        "completed_monotonic_us":monotonic,"observation_time_basis":"Guest enumeration completion; not an atomic desktop frame",
        "displays":displays.map{display in
            let scale=scales[display.id].flatMap{$0.isFinite && $0>0 ? $0:nil}
            return display.wire(scale:display.active && !display.asleep ? scale:nil)
        },"windows":entries,"eligible_window_count":eligible,"omitted_invalid_count":invalid,"truncated":invalid>0 || entries.count<eligible]
    while try JSONSerialization.data(withJSONObject:result,options:[.sortedKeys,.withoutEscapingSlashes]).count>limit {
        guard !entries.isEmpty else {throw windowFailure("unsupported_geometry","Display metadata exceeds window observation response limit")}
        entries.removeLast();result["windows"]=entries;result["truncated"]=true
    }
    return result
}

func windowsReply(_ id:String,enabled:Bool)->[String:Any] {
    guard enabled else {return errorReply(id,"forbidden","Window observation disabled at service launch")}
    guard getuid() != 0,systemString("hw.model").hasPrefix("VirtualMac") else {return errorReply(id,"virtual_guest_required","Window observation requires the non-root Guest console user")}
    guard CGPreflightScreenCaptureAccess() else {return errorReply(id,"screen_recording_permission_required","Existing Guest Screen Recording permission is required; service does not prompt")}
    guard let token=windowObservationSlot.acquire() else {return errorReply(id,"busy","One window enumeration is already outstanding")}
    let expected:GuestConsoleIdentityV1,before:[WindowDisplayObservation]
    do {expected=try requireConsoleIdentity(GuestConsoleIdentityReader.observe());before=try readWindowDisplays()}
    catch let failure as WindowObservationFailure {windowObservationSlot.finish(token);return errorReply(id,failure.code,failure.message)}
    catch {windowObservationSlot.finish(token);return errorReply(id,"console_identity_unavailable","Guest console identity or display geometry is unavailable")}
    let pending=PendingWindowObservation(),deadline=DispatchTime.now() + .seconds(8)
    let requested=Int64(Date().timeIntervalSince1970*1_000_000)
    SCShareableContent.getExcludingDesktopWindows(true,onScreenWindowsOnly:true) { content,error in
        defer {windowObservationSlot.finish(token)}
        func fail(_ code:String,_ message:String){pending.finish(errorReply(id,code,message))}
        guard DispatchTime.now()<deadline else{fail("windows_timeout","Window enumeration exceeded eight seconds");return}
        guard let content,error==nil else{fail("shareable_content_failed","ScreenCaptureKit could not enumerate Guest windows");return}
        guard content.windows.count<=8192 else{fail("shareable_content_failed","Window metadata exceeds bounded enumeration capacity");return}
        do {
            let quartz=try readQuartzWindowBounds()
            var scales:[UInt32:Double]=[:]
            for display in before where display.active && !display.asleep {
                guard let shareable=content.displays.first(where:{$0.displayID==display.id}),shareable.frame==display.bounds else {
                    throw windowFailure("display_configuration_changed","An active display is not consistently shareable")
                }
                let scale=Double(SCContentFilter(display:shareable,excludingWindows:[]).pointPixelScale)
                if scale.isFinite && scale>0 {scales[display.id]=scale}
            }
            var candidates:[WindowCandidate]=[],omitted=0
            for window in content.windows where window.isOnScreen && window.windowLayer==0 {
                guard let application=window.owningApplication else{continue}
                // CGWindowBounds has a documented main-display upper-left
                // screen-space origin. Do not infer AppKit's bottom-left space.
                // A window moving between the two public reads is omitted.
                guard let bounds=quartz[window.windowID],bounds==window.frame else {omitted+=1;continue}
                candidates.append(WindowCandidate(id:window.windowID,pid:application.processID,bundle:application.bundleIdentifier,
                    application:application.applicationName,bounds:bounds))
            }
            let result=try makeWindowResult(identity:expected,displays:before,scales:scales,candidates:candidates,omitted:omitted,
                requested:requested,completed:Int64(Date().timeIntervalSince1970*1_000_000),monotonic:DispatchTime.now().uptimeNanoseconds/1000)
            try validateWindowObservation(expected:expected,current:GuestConsoleIdentityReader.observe(),before:before,after:try readWindowDisplays(),permission:CGPreflightScreenCaptureAccess())
            guard DispatchTime.now()<deadline else{fail("windows_timeout","Window enumeration exceeded eight seconds");return}
            pending.finish(["id":id,"result":result])
        } catch let error as WindowObservationFailure {fail(error.code,error.message)}
        catch {fail("shareable_content_failed","Guest window observation could not be completed")}
    }
    guard pending.ready.wait(timeout:deadline) == .success,let reply=pending.value() else {
        return errorReply(id,"windows_timeout","Window enumeration exceeded eight seconds; outstanding callback remains busy")
    }
    return reply
}
