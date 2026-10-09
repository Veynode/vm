import AppKit
import Foundation
import Darwin

// Deliberately never access characters/charactersIgnoringModifiers, install an
// event tap/monitor, post an event, or change system shortcut preferences.
private let testKeys:[UInt16:String]=[8:"c",45:"n",36:"return",53:"escape",54:"right_command",55:"left_command",56:"left_shift",57:"caps_lock",58:"left_option",59:"left_control",60:"right_shift",61:"right_option",62:"right_control",63:"function"]
private func modifierWire(_ value:NSEvent.ModifierFlags)->[String:Any] {
    let flags=value.intersection(.deviceIndependentFlagsMask)
    return ["raw_device_independent":flags.rawValue,"control":flags.contains(.control),"command":flags.contains(.command),
            "shift":flags.contains(.shift),"option":flags.contains(.option),"caps_lock":flags.contains(.capsLock),"function":flags.contains(.function)]
}
private func keyMetadata(stage:String,kind:String,code:UInt16,flags:NSEvent.ModifierFlags,active:Bool,key:Bool,responder:Bool)->[String:Any]? {
    // flagsChanged can carry a nonstandard keycode in synthetic event streams.
    // Preserve only modifier state for those events, never the unknown keycode.
    guard let label=testKeys[code] else {
        guard kind == "flags_changed" else {return nil}
        return ["stage":stage,"kind":kind,"test_key":"redacted_modifier_event","modifiers":modifierWire(flags),
                "application_active":active,"window_key":key,"view_first_responder":responder]
    }
    return ["stage":stage,"kind":kind,"test_key":label,"key_code":code,"modifiers":modifierWire(flags),
            "application_active":active,"window_key":key,"view_first_responder":responder]
}
private final class Log {
    let fd:Int32,run=UUID().uuidString.lowercased()
    var count=0,bytes=0,dropped=0,failed=false,ended=false
    let limit:Int
    init(_ path:String,limit:Int=128*1024)throws {
        guard path.hasPrefix("/"),!path.utf8.contains(0) else {throw NSError(domain:"absolute_new_log_required",code:1)}
        self.limit=limit
        fd=open(path,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW_ANY|O_CLOEXEC,0o600)
        guard fd>=0 else {throw NSError(domain:"new_regular_log_required",code:Int(errno))}
    }
    deinit {close(fd)}
    func append(_ record:[String:Any],terminal:Bool=false) {
        guard !failed,!ended else{return}
        var row=record;row["run_id"]=run;row["sequence"]=count+1;row["observed_monotonic_ns"]=DispatchTime.now().uptimeNanoseconds
        guard var line=try? JSONSerialization.data(withJSONObject:row,options:[.sortedKeys,.withoutEscapingSlashes]),line.count<=2048 else{failed=true;return}
        line.append(10)
        guard bytes+line.count<=limit-(terminal ? 0:4096),terminal || count<256 else {dropped+=1;return}
        line.withUnsafeBytes {buffer in
            var offset=0
            while offset<buffer.count {let n=write(fd,buffer.baseAddress!.advanced(by:offset),buffer.count-offset);if n<0 && errno==EINTR {continue};if n<=0 {failed=true;return};offset+=n}
        }
        if !failed {bytes+=line.count;count+=1}
        if terminal {ended=true;if fsync(fd) != 0 {failed=true}}
    }
}
private final class KeyView:NSView {
    let log:Log
    var rows:[String]=[],otherCount=0
    init(_ log:Log){self.log=log;super.init(frame:NSRect(x:0,y:0,width:760,height:420))}
    required init?(coder:NSCoder){fatalError("unsupported")}
    override var acceptsFirstResponder:Bool {true}
    func receive(_ event:NSEvent,stage:String,kind:String) {
        guard let row=keyMetadata(stage:stage,kind:kind,code:event.keyCode,flags:event.modifierFlags,
            active:NSApp.isActive,key:window?.isKeyWindow==true,responder:window?.firstResponder===self) else {otherCount+=1;needsDisplay=true;return}
        log.append(row)
        rows.append("\(stage) \(kind) \(row["test_key"]!) flags=0x\(String(event.modifierFlags.intersection(.deviceIndependentFlagsMask).rawValue,radix:16))")
        if rows.count>12 {rows.removeFirst()};needsDisplay=true
    }
    func focus(_ reason:String) {
        log.append(["stage":"focus","kind":reason,"application_active":NSApp.isActive,"window_key":window?.isKeyWindow==true,"view_first_responder":window?.firstResponder===self])
        needsDisplay=true
    }
    override func keyDown(with e:NSEvent){receive(e,stage:"view",kind:"key_down")}
    override func keyUp(with e:NSEvent){receive(e,stage:"view",kind:"key_up")}
    override func flagsChanged(with e:NSEvent){receive(e,stage:"view",kind:"flags_changed")}
    override func performKeyEquivalent(with e:NSEvent)->Bool {receive(e,stage:"view",kind:"key_equivalent");return false}
    override func mouseDown(with e:NSEvent){window?.makeFirstResponder(self);focus("clicked_view")}
    override func draw(_ dirtyRect:NSRect) {
        NSColor.windowBackgroundColor.setFill();bounds.fill()
        var lines=["Veynode key event probe — this window only", "Test: plain C, Ctrl+C, plain N, Command+N, Return.",
                   "No text is recorded. Do not type passwords. Auto-exits within120s.",
                   "active=\(NSApp.isActive) key=\(window?.isKeyWindow==true) responder=\(window?.firstResponder===self) other_redacted=\(otherCount)",
                   "records=\(log.count) dropped=\(log.dropped) write_failed=\(log.failed)"]
        lines += rows
        for (i,line) in lines.enumerated() {(line as NSString).draw(at:NSPoint(x:18,y:bounds.height-30-CGFloat(i*22)),withAttributes:[.font:NSFont.monospacedSystemFont(ofSize:13,weight:.regular),.foregroundColor:NSColor.labelColor])}
    }
}
@objc(VeynodeKeyApplication)
private final class KeyApplication:NSApplication {
    weak var probe:KeyView?
    override func sendEvent(_ event:NSEvent) {
        if let probe,let window=probe.window,isActive,window.isKeyWindow,(event.window===window || event.window==nil) {
            let kind:String?
            switch event.type {case .keyDown:kind="key_down";case .keyUp:kind="key_up";case .flagsChanged:kind="flags_changed";default:kind=nil}
            if let kind {probe.receive(event,stage:"application",kind:kind)}
        }
        super.sendEvent(event)
    }
}
private final class Delegate:NSObject,NSApplicationDelegate,NSWindowDelegate {
    let log:Log,seconds:Int,host:Bool
    var window:NSWindow?,view:KeyView?,timer:Timer?,finished=false
    init(_ log:Log,_ seconds:Int,_ host:Bool){self.log=log;self.seconds=seconds;self.host=host}
    func applicationDidFinishLaunching(_ notification:Notification) {
        let view=KeyView(log),window=NSWindow(contentRect:view.bounds,styleMask:[.titled,.closable],backing:.buffered,defer:false)
        self.view=view;self.window=window;window.contentView=view;window.delegate=self;window.title=host ? "Veynode Host Key Probe":"Veynode Guest Key Probe"
        (NSApp as? KeyApplication)?.probe=view
        window.center();NSApp.activate(ignoringOtherApps:true);window.makeKeyAndOrderFront(nil);window.makeFirstResponder(view)
        log.append(["stage":"lifecycle","kind":"started","surface":host ? "host-diagnostic":"guest","deadline_seconds":seconds,"characters_recorded":false,"global_monitor":false])
        view.focus("started")
        timer=Timer.scheduledTimer(withTimeInterval:Double(seconds),repeats:false){[weak self] _ in self?.finish("deadline");NSApp.terminate(nil)}
    }
    func finish(_ reason:String) {guard !finished else{return};finished=true;timer?.invalidate();log.append(["stage":"lifecycle","kind":"ended","reason":reason,"other_event_occurrences_redacted":view?.otherCount ?? 0,"dropped":log.dropped,"write_failed_before_flush":log.failed],terminal:true)}
    func windowDidBecomeKey(_ notification:Notification){view?.focus("window_became_key")}
    func windowDidResignKey(_ notification:Notification){view?.focus("window_resigned_key")}
    func applicationDidBecomeActive(_ notification:Notification){view?.focus("application_became_active")}
    func applicationDidResignActive(_ notification:Notification){view?.focus("application_resigned_active")}
    func windowWillClose(_ notification:Notification){finish("window_closed")}
    func applicationShouldTerminateAfterLastWindowClosed(_ sender:NSApplication)->Bool {true}
    func applicationWillTerminate(_ notification:Notification){finish("application_terminated")}
}
private func model()->String {var size=0;guard sysctlbyname("hw.model",nil,&size,nil,0)==0 else{return "unknown"};var value=[CChar](repeating:0,count:size);guard sysctlbyname("hw.model",&value,&size,nil,0)==0 else{return "unknown"};return String(cString:value)}
private func selfTest()throws {
    for flags:NSEvent.ModifierFlags in [[],.control,.command,[.shift,.option]] {
        let metadata=keyMetadata(stage:"view",kind:"key_down",code:8,flags:flags,active:true,key:true,responder:true)!
        let bytes=try JSONSerialization.data(withJSONObject:metadata)
        let decoded=try JSONSerialization.jsonObject(with:bytes) as! [String:Any]
        precondition(Set(decoded.keys)==Set(["stage","kind","test_key","key_code","modifiers","application_active","window_key","view_first_responder"]))
        precondition((decoded["modifiers"] as! [String:Any])["control"] as? Bool==flags.contains(.control))
    }
    for code in UInt16.min...UInt16.max where testKeys[code] == nil {
        for kind in ["key_down","key_up","key_equivalent"] {
            precondition(keyMetadata(stage:"view",kind:kind,code:code,flags:.control,active:true,key:true,responder:true)==nil,"Unapproved text-key events must be entirely redacted")
        }
        let changed=keyMetadata(stage:"view",kind:"flags_changed",code:code,flags:.control,active:true,key:true,responder:true)!
        precondition(changed["key_code"] == nil && changed["test_key"] as? String == "redacted_modifier_event")
        precondition(Set(changed.keys)==Set(["stage","kind","test_key","modifiers","application_active","window_key","view_first_responder"]))
    }
    let changed=keyMetadata(stage:"application",kind:"flags_changed",code:0,flags:.command,active:true,key:true,responder:true)!
    let serialized=try JSONSerialization.data(withJSONObject:changed)
    let decoded=try JSONSerialization.jsonObject(with:serialized) as! [String:Any]
    precondition(decoded["key_code"] == nil && (decoded["modifiers"] as! [String:Any])["command"] as? Bool == true)
    // Foundation keeps the /var alias on this OS; O_NOFOLLOW_ANY correctly rejects it.
    guard let resolved=realpath(NSTemporaryDirectory(),nil) else {throw NSError(domain:"temporary_directory_realpath",code:Int(errno))}
    let temporary=String(cString:resolved);free(resolved)
    let parent=URL(fileURLWithPath:temporary).appendingPathComponent("key-probe-"+UUID().uuidString)
    try FileManager.default.createDirectory(at:parent,withIntermediateDirectories:false,attributes:[.posixPermissions:0o700]);defer{try? FileManager.default.removeItem(at:parent)}
    let path=parent.appendingPathComponent("events.jsonl").path
    let log=try Log(path,limit:8192)
    for _ in 0..<512 {log.append(keyMetadata(stage:"view",kind:"flags_changed",code:59,flags:.control,active:true,key:true,responder:true)!)}
    log.append(["kind":"ended"],terminal:true)
    precondition(log.dropped>0 && !log.failed && log.ended)
    let bytes=try Data(contentsOf:URL(fileURLWithPath:path));precondition(bytes.count<=8192)
    for line in bytes.split(separator:10){_=try JSONSerialization.jsonObject(with:Data(line))}
    do {_=try Log(path);preconditionFailure("must refuse existing log")}catch{}
    let link=parent.appendingPathComponent("link").path;guard symlink(path,link)==0 else{throw NSError(domain:"fixture",code:1)}
    do {_=try Log(link);preconditionFailure("must refuse symlink log")}catch{}
    var s=stat();precondition(stat(path,&s)==0 && s.st_mode&0o777==0o600)
    print("PASS: whitelisted test-key/modifier serialization, all 65536 keycodes checked for redaction with unknown flagsChanged state preserved, bounded real0600 JSONL with reserved final record, existing/symlink refusal. No AppKit window or event generated.")
}
do {
    let args=Array(CommandLine.arguments.dropFirst())
    if args==["--self-test"] {try selfTest();exit(0)}
    var path:String?,seconds=120,host=false,index=0
    while index<args.count {
        if args[index]=="--host-diagnostic",!host {host=true;index+=1}
        else if args[index]=="--log",path==nil,index+1<args.count {path=args[index+1];index+=2}
        else if args[index]=="--seconds",index+1<args.count,let value=Int(args[index+1]),(1...120).contains(value) {seconds=value;index+=2}
        else {throw NSError(domain:"invalid_arguments",code:1)}
    }
    guard getuid() != 0,host || model().hasPrefix("VirtualMac"),let path else {throw NSError(domain:"nonroot_guest_or_explicit_host_diagnostic_and_new_log_required",code:1)}
    let log=try Log(path)
    guard let application=KeyApplication.shared as? KeyApplication else {throw NSError(domain:"key_application_subclass_required",code:1)}
    let delegate=Delegate(log,seconds,host)
    application.setActivationPolicy(.regular);application.delegate=delegate
    withExtendedLifetime(delegate){application.run()}
    exit(log.failed ? 1:0)
} catch {
    if CommandLine.arguments.dropFirst().elementsEqual(["--self-test"]) {
        let e=error as NSError
        fputs("self-test failure domain=\(e.domain) code=\(e.code)\n",stderr)
    }
    fputs("key event probe refused or failed; no permission request was made\n",stderr);exit(1)
}
