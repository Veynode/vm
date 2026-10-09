import Foundation
import CoreGraphics
import Darwin

struct GuestInputFailure: Error { let code: String; let message: String }
private func inputFailure(_ code: String, _ message: String) -> GuestInputFailure { GuestInputFailure(code: code, message: message) }
private func unsignedInput(_ value: Any?) -> UInt64? {
    guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID(), n.doubleValue.isFinite,
          n.doubleValue >= 0, n.doubleValue <= 9_007_199_254_740_991, n.doubleValue.rounded() == n.doubleValue else { return nil }
    return n.uint64Value
}
private func inputHex(_ text:String, count:Int) -> Bool { text.utf8.count==count && text.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) } }
private func inputDouble(_ value:Any?) -> Double? {
    // ScreenCaptureKit CGRect metadata is CGFloat boxed as Any. `as? Double`
    // rejects that real representation even though JSON would serialize it.
    guard let number=value as? NSNumber,CFGetTypeID(number) != CFBooleanGetTypeID(),number.doubleValue.isFinite else{return nil}
    return number.doubleValue
}
private func inputBool(_ value: Any?) -> Bool? {
    guard let n = value as? NSNumber, CFGetTypeID(n) == CFBooleanGetTypeID() else { return nil }; return n.boolValue
}
struct GuestDisplayGeometry: Equatable {
    let display: UInt32, width: Int, height: Int
    let x: Double, y: Double, pointsWidth: Double, pointsHeight: Double
    var valid: Bool { width > 0 && height > 0 && width <= 4096 && height <= 4096 &&
        [x,y,pointsWidth,pointsHeight].allSatisfy(\.isFinite) && pointsWidth > 0 && pointsHeight > 0 }
    func point(_ x: UInt64, _ y: UInt64) -> CGPoint {
        // Existing host plan normalizes pixel CENTRES into0..32767. Recover the
        // pixel index, then map using the actual captured global display bounds.
        let px = floor(Double(x) * Double(width) / 32768)
        let py = floor(Double(y) * Double(height) / 32768)
        return CGPoint(x: self.x + (px + 0.5) * pointsWidth / Double(width),
                       y: self.y + (py + 0.5) * pointsHeight / Double(height))
    }
}
enum GuestInputEvent {
    case move(CGPoint, String?)
    case button(String, Bool, CGPoint)
    case key(String, Bool, CGEventFlags)
    case scroll(String, Int32, CGPoint)
}
protocol GuestInputSink {
    func geometry() -> GuestDisplayGeometry?
    func consoleIdentity() -> GuestConsoleIdentityV1?
    func available() -> Bool
    func post(_ event: GuestInputEvent) throws
}
let physicalMacKeys: [String: CGKeyCode] = [
    "a":0,"s":1,"d":2,"f":3,"h":4,"g":5,"z":6,"x":7,"c":8,"v":9,"b":11,
    "q":12,"w":13,"e":14,"r":15,"y":16,"t":17,"1":18,"2":19,"3":20,"4":21,"6":22,"5":23,
    "9":25,"7":26,"8":28,"0":29,"o":31,"u":32,"i":34,"p":35,"ret":36,"l":37,"j":38,"k":40,
    "n":45,"m":46,"tab":48,"spc":49,"backspace":51,"esc":53,"meta_r":54,"meta_l":55,
    "shift":56,"alt":58,"ctrl":59,"shift_r":60,"alt_r":61,"ctrl_r":62,
    "f1":122,"f2":120,"f3":99,"f4":118,"f5":96,"f6":97,"f7":98,"f8":100,"f9":101,"f10":109,"f11":103,"f12":111,
    "home":115,"pgup":116,"delete":117,"end":119,"pgdn":121,"left":123,"right":124,"down":125,"up":126
]
private final class QuartzGuestInputSink: GuestInputSink {
    private let source = CGEventSource(stateID: .privateState)
    func available() -> Bool {
        guard getuid() != 0, systemString("hw.model").hasPrefix("VirtualMac") else { return false }
        return CGPreflightPostEventAccess()
    }
    func consoleIdentity() -> GuestConsoleIdentityV1? {
        guard getuid() != 0,systemString("hw.model").hasPrefix("VirtualMac") else {return nil}
        let observation=GuestConsoleIdentityReader.observe()
        return observation.identityEligible ? observation.identity:nil
    }
    func geometry() -> GuestDisplayGeometry? {
        guard getuid() != 0, systemString("hw.model").hasPrefix("VirtualMac") else { return nil }
        let id = CGMainDisplayID(), bounds = CGDisplayBounds(id)
        guard CGDisplayIsActive(id) != 0, CGDisplayIsAsleep(id) == 0 else { return nil }
        guard let mode=CGDisplayCopyDisplayMode(id) else {return nil}
        let value = GuestDisplayGeometry(display:id,width:mode.pixelWidth,height:mode.pixelHeight,
            x:bounds.origin.x,y:bounds.origin.y,pointsWidth:bounds.width,pointsHeight:bounds.height)
        return value.valid ? value : nil
    }
    func post(_ input: GuestInputEvent) throws {
        guard available(), let source else { throw inputFailure("accessibility_permission_required", "Guest Accessibility permission is unavailable; no input was posted") }
        // This includes cleanup posts. Never intentionally release into another
        // user's active console; a refused cleanup remains truthfully unconfirmed.
        guard consoleIdentity() != nil else {throw inputFailure("console_identity_unavailable","Current public console ownership cannot be verified; no event was posted")}
        let event: CGEvent?
        switch input {
        case .move(let point, let held):
            let type: CGEventType = held == "left" ? .leftMouseDragged : (held == "right" ? .rightMouseDragged : (held == "middle" ? .otherMouseDragged : .mouseMoved))
            event = CGEvent(mouseEventSource:source,mouseType:type,mouseCursorPosition:point,mouseButton:held == "right" ? .right : (held == "middle" ? .center : .left))
        case .button(let name, let down, let point):
            let button: CGMouseButton = name == "left" ? .left : (name == "right" ? .right : .center)
            let type: CGEventType = name == "left" ? (down ? .leftMouseDown : .leftMouseUp) : (name == "right" ? (down ? .rightMouseDown : .rightMouseUp) : (down ? .otherMouseDown : .otherMouseUp))
            event = CGEvent(mouseEventSource:source,mouseType:type,mouseCursorPosition:point,mouseButton:button)
            event?.setIntegerValueField(.mouseEventClickState,value:1)
        case .key(let name, let down, let flags):
            guard let code = physicalMacKeys[name] else { throw inputFailure("unsupported_key", "Physical key is not supported by this macOS adapter") }
            event = CGEvent(keyboardEventSource:source,virtualKey:code,keyDown:down); event?.flags = flags
        case .scroll(let direction, let ticks, let point):
            let vertical: Int32 = direction == "wheel-up" ? ticks : (direction == "wheel-down" ? -ticks : 0)
            let horizontal: Int32 = direction == "wheel-left" ? ticks : (direction == "wheel-right" ? -ticks : 0)
            event = CGEvent(scrollWheelEvent2Source:source,units:.line,wheelCount:2,wheel1:vertical,wheel2:horizontal,wheel3:0)
            event?.location = point
        }
        guard let event else { throw inputFailure("input_event_failed", "Could not create a Guest input event") }
        event.post(tap:.cghidEventTap) // Public API returns void; never application-delivery confirmation.
    }
}
private struct GuestInputFrame { let id:String; let geometry:GuestDisplayGeometry; let identity:GuestConsoleIdentityV1; let generation:UInt64; let captured:UInt64 }
final class GuestInputCoordinator {
    private let lock = NSLock(), sink: GuestInputSink, now: () -> UInt64
    private var frames: [GuestInputFrame] = [], geometry: GuestDisplayGeometry?, generation: UInt64 = 0, configurationRevision:UInt64 = 0
    private var operation: String?, operationOwner: String?, epoch: UInt64 = 0, revokedThrough: UInt64 = 0
    private var operationIdentity:GuestConsoleIdentityV1?, lastIdentityPoll:UInt64=0
    private var deadline: UInt64 = 0, lastStep: UInt64 = 0
    private var keys = Set<String>(), buttons = Set<String>(), location = CGPoint.zero
    private var fault = false, positioned = false
    init(sink: GuestInputSink, now: @escaping () -> UInt64 = { DispatchTime.now().uptimeNanoseconds }) { self.sink=sink; self.now=now }
    private func flags(_ held: Set<String>) -> CGEventFlags {
        var result = CGEventFlags()
        if !held.isDisjoint(with:["shift","shift_r"]) { result.insert(.maskShift) }
        if !held.isDisjoint(with:["ctrl","ctrl_r"]) { result.insert(.maskControl) }
        if !held.isDisjoint(with:["alt","alt_r"]) { result.insert(.maskAlternate) }
        if !held.isDisjoint(with:["meta_l","meta_r"]) { result.insert(.maskCommand) }
        return result
    }
    private func release() throws {
        // Only release states this service itself posted. Do not synthesize an
        // all-key reset or attempt to release unrelated physical user keys.
        func requireOriginalConsole() throws {
            guard let previous=operationIdentity,let current=sink.consoleIdentity(),
                  previous.securitySessionID==current.securitySessionID,previous.uid==current.uid,
                  previous.consoleSet==current.consoleSet else {
                fault=true
                throw inputFailure("input_release_console_unconfirmed","Synthetic state retained because its original login console cannot be verified; no release was posted")
            }
        }
        var failure:Error?
        for key in keys.sorted().reversed() {
            var remaining = keys; remaining.remove(key)
            do { try requireOriginalConsole();try sink.post(.key(key,false,flags(remaining))); keys.remove(key) }
            catch { fault=true; if failure==nil {failure=error} }
        }
        for button in buttons.sorted() {
            do { try requireOriginalConsole();try sink.post(.button(button,false,location)); buttons.remove(button) }
            catch { fault=true; if failure==nil {failure=error} }
        }
        if let failure {throw failure}
        positioned=false; operation=nil; operationOwner=nil; operationIdentity=nil; deadline=0; lastStep=0
    }
    func blockedReason() -> String? {lock.lock();defer{lock.unlock()};return fault ? "synthetic_release_unconfirmed":nil}
    func captureToken() -> UInt64 { lock.lock();defer{lock.unlock()};return configurationRevision }
    func invalidate() {
        lock.lock();defer{lock.unlock()}
        frames.removeAll();geometry=nil
        if configurationRevision<UInt64.max {configurationRevision+=1} else {fault=true}
        do {try release()} catch {fault=true}
    }
    func record(_ metadata: [String:Any], captureToken:UInt64?=nil) throws -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        guard !fault else {throw inputFailure("input_cleanup_unconfirmed","Earlier synthetic release failed; input anchors remain disabled")}
        guard captureToken==nil || captureToken==configurationRevision else {throw inputFailure("input_geometry_changed","Display reconfigured while screenshot was captured")}
        guard let id=metadata["frame_id"] as? String, let display=unsignedInput(metadata["display_id"]), display<=UInt32.max,
              let width=unsignedInput(metadata["width"]),let height=unsignedInput(metadata["height"]),
              let capturedUS=unsignedInput(metadata["captured_monotonic_us"]),
              let bounds=metadata["desktop_bounds_points"] as? [String:Any],
              let x=inputDouble(bounds["x"]),let y=inputDouble(bounds["y"]),
              let pw=inputDouble(bounds["width"]),let ph=inputDouble(bounds["height"]) else { throw inputFailure("input_geometry_invalid", "Screenshot lacks input coordinate metadata") }
        let next=GuestDisplayGeometry(display:UInt32(display),width:Int(width),height:Int(height),x:x,y:y,pointsWidth:pw,pointsHeight:ph)
        guard next.valid, sink.geometry()==next else { throw inputFailure("input_geometry_changed", "Screenshot display geometry changed before publication") }
        guard let wire=metadata["console_identity"] as? [String:Any],let identity=try? GuestConsoleIdentityV1(wire:wire),
              identity==sink.consoleIdentity() else {throw inputFailure("input_console_identity_changed","Screenshot public console identity is unavailable or changed")}
        do {try validateCaptureConsole(identity,observation:ConsoleIdentityObservation(identity:identity,rejection:nil),display:next.display,width:next.width,height:next.height,bounds:CGRect(x:x,y:y,width:pw,height:ph))}
        catch {throw inputFailure("input_console_geometry_mismatch","Screenshot geometry differs from its public console identity")}
        if geometry != next || frames.last?.identity != identity {
            try release(); frames.removeAll(); guard generation<9_007_199_254_740_991 else {throw inputFailure("input_generation_exhausted","Restart Guest service to restore input")}; generation += 1; geometry=next
        }
        frames.append(GuestInputFrame(id:id,geometry:next,identity:identity,generation:generation,captured:capturedUS*1000))
        if frames.count>16 {frames.removeFirst(frames.count-16)}
        return generation
    }
    func tick() {
        lock.lock(); defer { lock.unlock() }
        let current=sink.geometry()
        let time=now()
        frames.removeAll{time<$0.captured || time-$0.captured>5_000_000_000}
        var identityInvalid=false
        // Authority reads are on-demand per input action. The existing100ms
        // release watchdog samples identity at most2Hz, only while anchors or
        // an operation exist; idle service does not continuously query sessions.
        if (!frames.isEmpty || operation != nil) && (lastIdentityPoll==0 || time-lastIdentityPoll>=500_000_000) {
            lastIdentityPoll=time
            let expected=operationIdentity ?? frames.last?.identity
            identityInvalid=sink.consoleIdentity() != expected
            if identityInvalid {frames.removeAll();geometry=nil;if configurationRevision<UInt64.max {configurationRevision+=1} else {fault=true}}
        }
        if current != geometry && geometry != nil { frames.removeAll();geometry=nil;if configurationRevision<UInt64.max {configurationRevision+=1} else {fault=true} }
        if operation != nil && (identityInvalid || current == nil || current != frames.last?.geometry || now()>=deadline || now()-lastStep>=1_000_000_000 || !sink.available()) {
            do {try release()} catch {fault=true}
        }
    }
    func shutdown() -> Bool {
        lock.lock();defer{lock.unlock()};frames.removeAll();revokedThrough=9_007_199_254_740_991
        do {try release();return true} catch {return false}
    }
    func cleanup(_ args:[String:Any]) throws -> [String:Any] {
        lock.lock();defer{lock.unlock()}
        guard let op=args["operation_id"] as? String,op.count==32 else {throw inputFailure("invalid_request","operation_id required for cleanup")}
        guard operation==op else {return ["released":false,"reason":"operation is no longer active"]}
        try release();return ["released":true,"synthetic_release_dispatched":true,"guest_delivery_confirmed":false]
    }
    func revoke(_ args:[String:Any]) throws -> [String:Any] {
        lock.lock();defer{lock.unlock()}
        guard let through=unsignedInput(args["through_epoch"]) else {throw inputFailure("invalid_request","through_epoch must be a safe unsigned integer")}
        revokedThrough=max(revokedThrough,through) // Commit the fence BEFORE attempting cleanup.
        if operation != nil && epoch<=revokedThrough {try release()}
        return ["released":true,"through_epoch":through,"synthetic_release_dispatched":true,"guest_delivery_confirmed":false]
    }
    func step(_ args:[String:Any]) throws -> [String:Any] {
        lock.lock();defer{lock.unlock()}
        guard !fault else {throw inputFailure("input_cleanup_unconfirmed","Earlier synthetic release failed; restart service after restoring permission")}
        guard sink.available() else {throw inputFailure("accessibility_permission_required","Existing Guest Accessibility permission is required; service does not prompt")}
        guard let op=args["operation_id"] as? String,inputHex(op,count:32),
              let requestedEpoch=unsignedInput(args["epoch"]),requestedEpoch>revokedThrough,
              let owner=args["owner"] as? String,inputHex(owner,count:64),
              let begin=inputBool(args["begin"]),let id=args["frame_id"] as? String,
              let requestedGeneration=unsignedInput(args["source_generation"]),
              let width=unsignedInput(args["width"]),let height=unsignedInput(args["height"]),
              let events=args["events"] as? [[String:Any]],!events.isEmpty,events.count<=16 else {throw inputFailure("invalid_request","Invalid bounded physical input request or revoked epoch")}
        guard let frame=frames.first(where:{$0.id==id}), now()>=frame.captured,now()-frame.captured<=5_000_000_000,
              frame.generation==requestedGeneration,UInt64(frame.geometry.width)==width,UInt64(frame.geometry.height)==height,
              sink.geometry()==frame.geometry else {throw inputFailure("stale_frame","Capture a fresh screenshot after display or geometry changes")}
        guard sink.consoleIdentity()==frame.identity else {
            frames.removeAll();geometry=nil
            do {try release()} catch {fault=true;throw error}
            throw inputFailure("input_console_identity_changed","Captured login console no longer matches; acquire a fresh screenshot")
        }
        guard begin ? operation==nil : (operation==op && operationOwner==owner && epoch==requestedEpoch && now()<deadline) else {throw inputFailure("input_busy_or_expired","Input operation is active, replaced, or expired")}
        // Validate the ENTIRE batch before posting any element.
        var plans:[GuestInputEvent]=[], nextKeys=keys, nextButtons=buttons, point=location
        let hasPoint = !begin && positioned
        if events.contains(where:{$0["type"] as? String=="abs"}) {
            guard events.count==2,events.allSatisfy({$0["type"] as? String=="abs"}) else {throw inputFailure("invalid_request","Absolute x/y coordinates must be paired")}
            var coordinates:[String:UInt64]=[:]
            for event in events {
                guard let d=event["data"] as? [String:Any],let axis=d["axis"] as? String,["x","y"].contains(axis),coordinates[axis]==nil,
                      let v=unsignedInput(d["value"]),v<32768 else {throw inputFailure("invalid_request","Invalid normalized absolute coordinate")}
                coordinates[axis]=v
            }
            guard let x=coordinates["x"],let y=coordinates["y"] else {throw inputFailure("invalid_request","Both axes required")}
            point=frame.geometry.point(x,y);plans.append(.move(point,nextButtons.sorted().first))
        } else {
            for event in events {
                guard let type=event["type"] as? String,let d=event["data"] as? [String:Any],let down=inputBool(d["down"]) else {throw inputFailure("invalid_request","Invalid physical input event")}
                if type=="key" {
                    guard let key=d["key"] as? [String:Any],key["type"] as? String=="qcode",let name=key["data"] as? String,physicalMacKeys[name] != nil else {throw inputFailure("unsupported_key","Only explicitly supported physical macOS keys are accepted; no Unicode/layout translation")}
                    if down {guard !nextKeys.contains(name) else {throw inputFailure("invalid_request","Key already pressed")};nextKeys.insert(name)} else {guard nextKeys.remove(name) != nil else {throw inputFailure("invalid_request","Key is not held by this operation")}}
                    plans.append(.key(name,down,flags(nextKeys)))
                } else if type=="btn",let name=d["button"] as? String {
                    guard hasPoint else {throw inputFailure("invalid_request","Mouse action requires absolute position in this operation")}
                    if ["wheel-up","wheel-down","wheel-left","wheel-right"].contains(name) {if down {plans.append(.scroll(name,1,point))}}
                    else {
                        guard ["left","right","middle"].contains(name) else {throw inputFailure("invalid_request","Unsupported mouse button")}
                        if down {guard !nextButtons.contains(name) else {throw inputFailure("invalid_request","Button already pressed")};nextButtons.insert(name)} else {guard nextButtons.remove(name) != nil else {throw inputFailure("invalid_request","Button is not held by this operation")}}
                        plans.append(.button(name,down,point))
                    }
                } else {throw inputFailure("invalid_request","Unsupported input event")}
            }
        }
        if begin {positioned=false;operation=op;operationOwner=owner;operationIdentity=frame.identity;epoch=requestedEpoch;deadline=now()+4_000_000_000}
        lastStep=now()
        do {
            for plan in plans {
                guard sink.consoleIdentity()==frame.identity else {throw inputFailure("input_console_identity_changed","Console changed between input batch elements; remaining events were not posted")}
                // post() can throw only before CGEventPost; record states only
                // after an event was actually submitted, under this same lock.
                try sink.post(plan)
                switch plan {
                case .key(let name,true,_):keys.insert(name)
                case .button(let name,true,_):buttons.insert(name)
                case .key(let name,false,_):keys.remove(name)
                case .button(let name,false,_):buttons.remove(name)
                case .move(let p,_):location=p;positioned=true
                default:break
                }
            }
        } catch let postingError {
            do {try release()} catch {fault=true;throw inputFailure("input_cleanup_unconfirmed","Input was interrupted and original-console synthetic release could not be confirmed")}
            throw postingError
        }
        return ["dispatched":true,"source_generation":frame.generation,"guest_delivery_confirmed":false,"coordinate_space":"captured-display-pixels-to-Quartz-points"]
    }
}
let guestInput = GuestInputCoordinator(sink:QuartzGuestInputSink())
func inputPolicy(_ enabled:Bool) -> [String:Any] {
    let guest=getuid() != 0 && systemString("hw.model").hasPrefix("VirtualMac")
    let permitted=enabled && guest ? CGPreflightPostEventAccess():false
    let blocked=guestInput.blockedReason()
    return ["enabled":enabled,"available":permitted && blocked==nil,"input_blocked_reason":blocked.map{$0 as Any} ?? NSNull(),"permission":!enabled ? "not_checked_disabled":(!guest ? "not_checked_not_guest":(permitted ? "granted":"required")),"api":"CoreGraphics.CGEventPost","max_step_events":16,"max_operation_ms":4000,"inter_step_timeout_ms":1000,"physical_keys":physicalMacKeys.keys.sorted(),"text_input":false,"delivery_confirmation":false]
}
func inputReply(_ id:String,method:String,args:[String:Any],enabled:Bool) -> [String:Any] {
    guard enabled else {return errorReply(id,"forbidden","desktop input disabled; launch Guest service with --allow-observe --allow-input")}
    guard getuid() != 0,systemString("hw.model").hasPrefix("VirtualMac") else{return errorReply(id,"virtual_guest_required","input is restricted to the signed-in non-root VirtualMac Guest user")}
    do {
        let result:[String:Any]
        switch method {case "input_step":result=try guestInput.step(args);case "input_cleanup":result=try guestInput.cleanup(args);case "input_revoke":result=try guestInput.revoke(args);default:throw inputFailure("unsupported","unknown input method")}
        return ["id":id,"result":result]
    } catch {
        // A failed continuation must not leave this operation's synthetic keys held.
        // A different operation token cannot clean up the currently active owner.
        if method=="input_step",args["operation_id"] != nil { _ = try? guestInput.cleanup(args) }
        if let error=error as? GuestInputFailure {return errorReply(id,error.code,error.message)}
        return errorReply(id,"input_failed","Guest input operation failed")
    }
}

func registerGuestInputDisplayNotifications() -> Bool {
    guard getuid() != 0,systemString("hw.model").hasPrefix("VirtualMac") else {return false}
    return CGDisplayRegisterReconfigurationCallback({ _,_,_ in guestInput.invalidate() },nil) == .success
}
