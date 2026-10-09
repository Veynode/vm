// Guest-only own-window event oracle. No global monitor, event tap, event posting,
// desktop capture, AX API or permission request is used by this executable.
import AppKit
import CoreGraphics
import Foundation
import Darwin

private func guestModel() -> String {
    var size=0;guard sysctlbyname("hw.model",nil,&size,nil,0)==0,size>0,size<1024 else{return ""}
    var bytes=[CChar](repeating:0,count:size)
    guard sysctlbyname("hw.model",&bytes,&size,nil,0)==0 else{return ""};return String(cString:bytes)
}
private struct Geometry {
    let physicalWidth:Int,physicalHeight:Int,quartzBounds:CGRect,appKitFrame:CGRect
    func quartz(_ point:CGPoint)->CGPoint {
        CGPoint(x:quartzBounds.minX+(point.x-appKitFrame.minX)*quartzBounds.width/appKitFrame.width,
                y:quartzBounds.minY+(appKitFrame.maxY-point.y)*quartzBounds.height/appKitFrame.height)
    }
    func pixel(_ point:CGPoint)->CGPoint {
        CGPoint(x:(point.x-quartzBounds.minX)*Double(physicalWidth)/quartzBounds.width,
                y:(point.y-quartzBounds.minY)*Double(physicalHeight)/quartzBounds.height)
    }
    var json:[String:Any] { ["physical_width":physicalWidth,"physical_height":physicalHeight,"quartz_bounds_points":jsonRect(quartzBounds),"appkit_screen_frame_points":jsonRect(appKitFrame)] }
}
private func jsonRect(_ value:CGRect)->[String:Double] { ["x":value.minX,"y":value.minY,"width":value.width,"height":value.height] }
private func jsonPoint(_ value:CGPoint)->[String:Double] { ["x":value.x,"y":value.y] }
private func geometry()->Geometry? {
    let id=CGMainDisplayID()
    guard let mode=CGDisplayCopyDisplayMode(id),let screen=NSScreen.screens.first(where:{($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value==id}) else{return nil}
    let value=Geometry(physicalWidth:mode.pixelWidth,physicalHeight:mode.pixelHeight,quartzBounds:CGDisplayBounds(id),appKitFrame:screen.frame)
    guard value.physicalWidth>0,value.physicalHeight>0,value.quartzBounds.width>0,value.quartzBounds.height>0,value.appKitFrame.width>0,value.appKitFrame.height>0 else{return nil}
    return value
}
private final class Recorder {
    private let fd:Int32
    private let maxBytes:Int,maxEvents:Int
    private let snapshot:String?,runID:String?
    private var entries:[[String:Any]]=[]
    private(set) var count=0,bytes=0,dropped=0
    private(set) var failed=false
    init(path:String,maxBytes:Int=256*1024,maxEvents:Int=512,snapshot:String?=nil,runID:String?=nil)throws {
        guard (snapshot == nil && runID == nil) || (snapshot?.hasPrefix("/")==true && snapshot != path && runID?.utf8.count==32 && runID!.utf8.allSatisfy{(48...57).contains($0)||(97...102).contains($0)}) else{throw NSError(domain:"oracle.invalid_snapshot",code:1)}
        if let snapshot {guard access(snapshot,F_OK) != 0 && errno==ENOENT else{throw NSError(domain:"oracle.snapshot_exists",code:1)}}
        self.snapshot=snapshot;self.runID=runID
        guard path.hasPrefix("/"),maxBytes>=4096,maxEvents>0 else{throw NSError(domain:"oracle.invalid_log",code:1)}
        fd=open(path,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0o600)
        guard fd>=0 else{throw NSError(domain:NSPOSIXErrorDomain,code:Int(errno))}
        self.maxBytes=maxBytes;self.maxEvents=maxEvents
    }
    deinit {close(fd)}
    func append(_ entry:[String:Any],terminal:Bool=false) {
        guard !failed,let value=try? JSONSerialization.data(withJSONObject:entry,options:.sortedKeys),value.count<2048 else{failed=true;return}
        let line=value+Data([10])
        if (!terminal && (count>=maxEvents || bytes+line.count>maxBytes-4096)) || bytes+line.count>maxBytes {dropped+=1;return}
        var offset=0
        line.withUnsafeBytes {data in
            while offset<data.count {
                let n=write(fd,data.baseAddress!.advanced(by:offset),data.count-offset)
                if n<0 && errno==EINTR {continue}
                if n<=0 {failed=true;return};offset+=n
            }
        }
        bytes+=offset;count+=1
        entries.append(entry);publish()
    }
    private func publish() {
        guard let snapshot,let runID,!failed else{return}
        let value:[String:Any] = ["schema":1,"run_id":runID,"revision":count,"events":entries,"log_dropped":dropped,"log_write_failed":failed]
        guard let data=try? JSONSerialization.data(withJSONObject:value,options:[.sortedKeys,.withoutEscapingSlashes]),data.count<=256*1024 else{failed=true;return}
        let temporary=snapshot+".tmp-"+UUID().uuidString
        let output=open(temporary,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0o600)
        guard output>=0 else{failed=true;return}
        defer{close(output);unlink(temporary)}
        var offset=0
        data.withUnsafeBytes { raw in
            while offset<raw.count {
                let n=write(output,raw.baseAddress!.advanced(by:offset),raw.count-offset)
                if n<0 && errno==EINTR {continue}
                if n<=0 {failed=true;return};offset+=n
            }
        }
        // Atomic visibility only, not a disk durability claim. File-service reads
        // retain strict inode/mtime/nlink checks and can reject a concurrent replace.
        guard !failed,rename(temporary,snapshot)==0 else{failed=true;return}
    }
}
private final class Model {
    private(set) var counts:[String:Int]=[:],keys=Set<UInt16>(),buttons=Set<Int>()
    private(set) var observationGap=false,trackingOverflow=false,last="No input received",lastPoint=CGPoint.zero
    private(set) var modifiers:UInt=0
    func receive(_ kind:String,key:UInt16?=nil,button:Int?=nil,at:CGPoint?=nil,flags:UInt=0) {
        counts[kind,default:0]+=1;modifiers=flags
        if let key {if kind=="key_down" {if keys.count<128 || keys.contains(key) {keys.insert(key)} else {trackingOverflow=true}} else if kind=="key_up" {keys.remove(key)}}
        if let button {if kind=="mouse_down" {if buttons.count<8 || buttons.contains(button) {buttons.insert(button)} else {trackingOverflow=true}} else if kind=="mouse_up" {buttons.remove(button)}}
        if let at {lastPoint=at}
        last=kind+(key.map{" keycode=\($0)"} ?? "")+(button.map{" button=\($0)"} ?? "")
    }
    func gap(){observationGap=true}
    var summary:[String:Any] { ["counts":counts,"observed_pressed_keycodes":keys.sorted(),"observed_pressed_buttons":buttons.sorted(),"modifier_flags":modifiers,"observation_gap":observationGap,"tracking_overflow":trackingOverflow,"scope":"this oracle window only; no global state or trusted synthetic provenance"] }
}
private final class OracleView:NSView {
    let model:Model,recorder:Recorder
    private var tracking:NSTrackingArea?,lastMotionLog:TimeInterval=0
    let target=CGRect(x:40,y:235,width:720,height:250)
    init(model:Model,recorder:Recorder){self.model=model;self.recorder=recorder;super.init(frame:CGRect(x:0,y:0,width:800,height:530))}
    required init?(coder:NSCoder){fatalError("unsupported")}
    override var isFlipped:Bool {true}
    override var acceptsFirstResponder:Bool {true}
    override func acceptsFirstMouse(for event:NSEvent?)->Bool {true}
    override func updateTrackingAreas(){
        if let tracking {removeTrackingArea(tracking)}
        let next=NSTrackingArea(rect:bounds,options:[.mouseMoved,.activeInKeyWindow,.inVisibleRect],owner:self,userInfo:nil)
        addTrackingArea(next);tracking=next;super.updateTrackingAreas()
    }
    private func label(_ text:String,_ x:CGFloat,_ y:CGFloat,size:CGFloat=15,color:NSColor = .labelColor){
        (text as NSString).draw(at:CGPoint(x:x,y:y),withAttributes:[.font:NSFont.monospacedSystemFont(ofSize:size,weight:.regular),.foregroundColor:color])
    }
    override func draw(_ dirtyRect:NSRect){
        NSColor.windowBackgroundColor.setFill();bounds.fill()
        label("Veynode Guest input oracle — own window only",20,18,size:21)
        label("No capture / global key monitor / input injection / TCC request",20,52,size:12)
        label("key down \(model.counts["key_down",default:0])   up \(model.counts["key_up",default:0])   modifiers \(model.modifiers)",20,82)
        label("mouse down \(model.counts["mouse_down",default:0])   up \(model.counts["mouse_up",default:0])   drag \(model.counts["drag",default:0])   scroll \(model.counts["scroll",default:0])",20,110)
        label("Held keycodes: \(Array(model.keys.sorted().prefix(12)))  buttons: \(model.buttons.sorted())",20,138)
        label("Latest: \(model.last)",20,166)
        label(String(format:"local top-left points: %.2f, %.2f",model.lastPoint.x,model.lastPoint.y),20,192)
        NSColor(calibratedRed:0.12,green:0.22,blue:0.32,alpha:1).setFill();target.fill()
        NSColor(calibratedRed:0.1,green:0.62,blue:0.32,alpha:1).setFill();CGRect(x:target.minX,y:target.minY,width:target.width/2,height:target.height).fill()
        NSColor(calibratedRed:0.7,green:0.22,blue:0.18,alpha:1).setFill();CGRect(x:target.midX,y:target.minY,width:target.width/2,height:target.height).fill()
        for x in stride(from:Int(target.minX),through:Int(target.maxX),by:40){NSColor.white.withAlphaComponent(0.3).setStroke();let p=NSBezierPath();p.move(to:CGPoint(x:x,y:Int(target.minY)));p.line(to:CGPoint(x:x,y:Int(target.maxY)));p.stroke()}
        for y in stride(from:Int(target.minY),through:Int(target.maxY),by:40){NSColor.white.withAlphaComponent(0.3).setStroke();let p=NSBezierPath();p.move(to:CGPoint(x:Int(target.minX),y:y));p.line(to:CGPoint(x:Int(target.maxX),y:y));p.stroke()}
        label("LEFT target",target.minX+20,target.minY+20,size:20,color:.white)
        label("RIGHT target",target.midX+20,target.minY+20,size:20,color:.white)
        if target.contains(model.lastPoint) {NSColor.white.setStroke();let p=NSBezierPath();p.move(to:model.lastPoint.applying(CGAffineTransform(translationX:-10,y:0)));p.line(to:model.lastPoint.applying(CGAffineTransform(translationX:10,y:0)));p.move(to:model.lastPoint.applying(CGAffineTransform(translationX:0,y:-10)));p.line(to:model.lastPoint.applying(CGAffineTransform(translationX:0,y:10)));p.lineWidth=2;p.stroke()}
        label("Log \(recorder.count) records, dropped \(recorder.dropped); focus gap \(model.observationGap); overflow \(model.trackingOverflow); write failed \(recorder.failed)",20,503,size:11)
    }
    func targetGeometry()->[String:Any] {
        guard let window,let g=geometry() else{return ["available":false]}
        let a=g.quartz(window.convertPoint(toScreen:convert(target.origin,to:nil)))
        let b=g.quartz(window.convertPoint(toScreen:convert(CGPoint(x:target.maxX,y:target.maxY),to:nil)))
        let pa=g.pixel(a),pb=g.pixel(b)
        return ["available":true,"display":g.json,"target_quartz_points":["top_left":jsonPoint(a),"bottom_right":jsonPoint(b)],"target_main_display_pixels":["top_left":jsonPoint(pa),"bottom_right":jsonPoint(pb)],"view_points_top_left":jsonRect(target),"window_backing_scale":window.backingScaleFactor]
    }
    private func receive(_ event:NSEvent,_ kind:String,key:Bool=false,mouseButton:Bool=false){
        guard event.window === window,window?.isKeyWindow==true else{return}
        let local=convert(event.locationInWindow,from:nil)
        model.receive(kind,key:key ? event.keyCode:nil,button:mouseButton ? event.buttonNumber:nil,at:key ? nil:local,flags:event.modifierFlags.rawValue)
        needsDisplay=true
        // Motion is sampled for bounded evidence, but all own-window events are counted.
        if kind=="move" && event.timestamp-lastMotionLog<1.0/30 {return}
        if kind=="move" {lastMotionLog=event.timestamp}
        var record:[String:Any]=["event":kind,"sequence":model.counts.values.reduce(0,+),"timestamp_seconds":event.timestamp,"modifier_flags":event.modifierFlags.rawValue]
        if key {record["keycode"]=event.keyCode; if kind != "flags_changed" {record["repeat"]=event.isARepeat}} else {record["local_top_left_points"]=jsonPoint(local)}
        if mouseButton {record["button"]=event.buttonNumber}
        if kind=="scroll" {record["scrolling_delta_x"]=event.scrollingDeltaX;record["scrolling_delta_y"]=event.scrollingDeltaY;record["precise_deltas"]=event.hasPreciseScrollingDeltas}
        if let cg=event.cgEvent {
            if !key {
                record["quartz_global_points"]=jsonPoint(cg.location)
                if let g=geometry(){record["main_display_pixels"]=jsonPoint(g.pixel(cg.location))}
            }
            record["source_pid"]=cg.getIntegerValueField(.eventSourceUnixProcessID)
            record["source_state_id"]=cg.getIntegerValueField(.eventSourceStateID)
            record["source_user_data"]=cg.getIntegerValueField(.eventSourceUserData)
            record["provenance"]="untrusted CGEvent metadata; not proof of synthetic origin"
        }
        recorder.append(record)
    }
    override func mouseDown(with e:NSEvent){window?.makeFirstResponder(self);receive(e,"mouse_down",mouseButton:true)}
    override func mouseUp(with e:NSEvent){receive(e,"mouse_up",mouseButton:true)}
    override func rightMouseDown(with e:NSEvent){receive(e,"mouse_down",mouseButton:true)}
    override func rightMouseUp(with e:NSEvent){receive(e,"mouse_up",mouseButton:true)}
    override func otherMouseDown(with e:NSEvent){receive(e,"mouse_down",mouseButton:true)}
    override func otherMouseUp(with e:NSEvent){receive(e,"mouse_up",mouseButton:true)}
    override func mouseMoved(with e:NSEvent){receive(e,"move")}
    override func mouseDragged(with e:NSEvent){receive(e,"drag")}
    override func rightMouseDragged(with e:NSEvent){receive(e,"drag")}
    override func otherMouseDragged(with e:NSEvent){receive(e,"drag")}
    override func keyDown(with e:NSEvent){receive(e,"key_down",key:true)}
    override func keyUp(with e:NSEvent){receive(e,"key_up",key:true)}
    override func flagsChanged(with e:NSEvent){receive(e,"flags_changed",key:true)}
    override func scrollWheel(with e:NSEvent){receive(e,"scroll")}
}
private final class AppDelegate:NSObject,NSApplicationDelegate,NSWindowDelegate {
    let model=Model(),recorder:Recorder,seconds:Int
    var window:NSWindow!,view:OracleView!,timer:Timer?
    init(recorder:Recorder,seconds:Int){self.recorder=recorder;self.seconds=seconds}
    func applicationDidFinishLaunching(_ notification:Notification){
        view=OracleView(model:model,recorder:recorder)
        window=NSWindow(contentRect:view.frame,styleMask:[.titled,.closable,.miniaturizable],backing:.buffered,defer:false)
        window.title="Veynode Guest Input Oracle";window.delegate=self;window.contentView=view;window.isReleasedWhenClosed=false
        window.acceptsMouseMovedEvents=true;window.center();window.makeKeyAndOrderFront(nil);window.makeFirstResponder(view)
        NSApp.activate(ignoringOtherApps:true)
        recorder.append(["event":"ready","pid":getpid(),"scope":"own window only","maximum_seconds":seconds,"active_key_window":window.isKeyWindow,"geometry":view.targetGeometry()])
        timer=Timer.scheduledTimer(withTimeInterval:TimeInterval(seconds),repeats:false){_ in NSApp.terminate(nil)}
    }
    func windowDidMove(_ notification:Notification){if let view {recorder.append(["event":"geometry","geometry":view.targetGeometry()])}}
    func windowDidChangeBackingProperties(_ notification:Notification){if let view {recorder.append(["event":"geometry","geometry":view.targetGeometry()])}}
    func windowDidBecomeKey(_ notification:Notification){recorder.append(["event":"focus","active_key_window":true])}
    func windowDidResignKey(_ notification:Notification){model.gap();recorder.append(["event":"observation_gap","active_key_window":false,"reason":"window resigned key; cannot verify events delivered elsewhere"]);view?.needsDisplay=true}
    func applicationShouldTerminateAfterLastWindowClosed(_ sender:NSApplication)->Bool {true}
    func applicationWillTerminate(_ notification:Notification){timer?.invalidate();recorder.append(["event":"ended","summary":model.summary,"log_dropped":recorder.dropped,"log_write_failed":recorder.failed],terminal:true)}
}
private func selfTest() throws {
    let g=Geometry(physicalWidth:2560,physicalHeight:1440,quartzBounds:CGRect(x:-1280,y:90,width:1280,height:720),appKitFrame:CGRect(x:-1280,y:-20,width:1280,height:720))
    precondition(g.quartz(CGPoint(x:-1280,y:700))==CGPoint(x:-1280,y:90))
    precondition(g.pixel(g.quartz(CGPoint(x:-1279.75,y:699.75)))==CGPoint(x:0.5,y:0.5))
    let model=Model();model.receive("key_down",key:0);model.receive("mouse_down",button:0)
    model.gap();precondition(model.keys==[0] && model.buttons==[0],"focus loss must not fabricate releases")
    model.receive("key_up",key:0);model.receive("mouse_up",button:0)
    precondition(model.keys.isEmpty && model.buttons.isEmpty && model.observationGap)
    let url=URL(fileURLWithPath:NSTemporaryDirectory()).appendingPathComponent("veynode-oracle-test-"+UUID().uuidString)
    defer{try? FileManager.default.removeItem(at:url)}
    let recorder=try Recorder(path:url.path,maxBytes:8192,maxEvents:2)
    for i in 0..<10 {recorder.append(["event":"fixture","sequence":i])}
    recorder.append(["event":"ended"],terminal:true)
    precondition(recorder.count==3 && recorder.dropped==8 && !recorder.failed)
    let data=try Data(contentsOf:url);precondition(data.count==recorder.bytes && data.count<=8192)
    let attributes=try FileManager.default.attributesOfItem(atPath:url.path)
    precondition((attributes[.posixPermissions] as? NSNumber)?.intValue==0o600)
    let link=url.appendingPathExtension("link");defer{try? FileManager.default.removeItem(at:link)}
    try FileManager.default.createSymbolicLink(at:link,withDestinationURL:url)
    do {_=try Recorder(path:link.path);preconditionFailure("symlink must be rejected")}catch{}
    let unchanged=try Data(contentsOf:url);precondition(unchanged==data)
    let overflow=Model();for key in 0..<130 {overflow.receive("key_down",key:UInt16(key))}
    precondition(overflow.trackingOverflow && overflow.keys.count==128)
    precondition(String(decoding:data,as:UTF8.self).split(separator:"\n").count==3)
    do {_=try Recorder(path:url.path);preconditionFailure("existing log must not be overwritten")}catch{}
    let snap=url.appendingPathExtension("json"),snapLog=url.appendingPathExtension("jsonl")
    defer{try? FileManager.default.removeItem(at:snap);try? FileManager.default.removeItem(at:snapLog)}
    let r=try Recorder(path:snapLog.path,snapshot:snap.path,runID:String(repeating:"a",count:32))
    r.append(["event":"ready","active_key_window":true])
    let first=try Data(contentsOf:snap)
    let held=open(snap.path,O_RDONLY|O_NOFOLLOW);precondition(held>=0);defer{close(held)}
    r.append(["event":"ended"])
    let second=try Data(contentsOf:snap)
    let decoded=try JSONSerialization.jsonObject(with:second) as! [String:Any]
    precondition(first != second && decoded["revision"] as? Int==2 && decoded["run_id"] as? String==String(repeating:"a",count:32))
    var old=stat();precondition(fstat(held,&old)==0 && old.st_nlink==0,"concurrent old reader must detect retired inode")
    precondition(!r.failed && second.count<=256*1024)
    print("PASS: atomic nonce-bound bounded snapshot, retired inode remains rejectable; own-window oracle model, explicit observation gap, Retina/global origin mapping, bounded new-only log. No app/window/input capture launched.")
}
let args=Array(CommandLine.arguments.dropFirst())
if args==["--self-test"] {do{try selfTest();exit(0)}catch{fputs("oracle self-test failed\n",stderr);exit(1)}}
var seconds=300,log:String?,snapshot:String?,runID:String?
var index=0,valid=true
while index<args.count {
    if args[index]=="--seconds",index+1<args.count,let n=Int(args[index+1]),(30...900).contains(n){seconds=n;index+=2}
    else if args[index]=="--log",index+1<args.count,log==nil {log=args[index+1];index+=2}
    else if args[index]=="--snapshot",index+1<args.count,snapshot==nil {snapshot=args[index+1];index+=2}
    else if args[index]=="--run-id",index+1<args.count,runID==nil {runID=args[index+1];index+=2}
    else{valid=false;break}
}
guard valid,let log else {fputs("Usage: macos-input-oracle --log NEW_ABSOLUTE_FILE [--seconds 30..900] [--snapshot NEW_ABSOLUTE_FILE --run-id 32LOWERHEX] | --self-test\n",stderr);exit(2)}
guard getuid() != 0,guestModel().hasPrefix("VirtualMac") else {fputs("Oracle runs only in a non-root VirtualMac Guest; no host observation was started\n",stderr);exit(1)}
private let recorder:Recorder
do{recorder=try Recorder(path:log,snapshot:snapshot,runID:runID)}catch{fputs("Could not create new private oracle log\n",stderr);exit(1)}
let application=NSApplication.shared
application.setActivationPolicy(.regular)
private let delegate=AppDelegate(recorder:recorder,seconds:seconds)
application.delegate=delegate
application.run()
