// Guest-only presentation instrumentation. No input, screen capture, network or TCC.
import AppKit
import MetalKit
import Darwin

private func model() -> String {
    var size=0
    guard sysctlbyname("hw.model",nil,&size,nil,0)==0,size>0,size<1024 else{return ""}
    var bytes=[CChar](repeating:0,count:size)
    guard sysctlbyname("hw.model",&bytes,&size,nil,0)==0 else{return ""}
    return String(cString:bytes)
}
private func stats(_ values:[Double])->[String:Any] {
    let sorted=values.sorted()
    guard !sorted.isEmpty else{return ["count":0]}
    func percentile(_ p:Double)->Double {sorted[max(0,Int(ceil(p*Double(sorted.count)))-1)]}
    return ["count":sorted.count,"p50":percentile(0.5),"p95":percentile(0.95),"p99":percentile(0.99),"max":sorted.last!,"over_25ms":sorted.filter{$0>25}.count,"over_50ms":sorted.filter{$0>50}.count]
}
private final class Recording {
    let lock=NSLock(),fd:Int32
    var frames:[[String:Any]]=[],submitted=0,completed=0,invalidPresented=0,noDrawable=0,backpressure=0
    var errors=Set<String>(),sealed=false
    init(_ path:String)throws {
        guard path.hasPrefix("/") else {throw NSError(domain:"absolute path required",code:1)}
        fd=open(path,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0o600)
        guard fd>=0 else{throw NSError(domain:NSPOSIXErrorDomain,code:Int(errno))}
    }
    deinit{close(fd)}
    func mutate(_ body:()->Void){lock.lock();defer{lock.unlock()};if !sealed {body()}}
    func finish(_ metadata:[String:Any])->[String:Any] {
        lock.lock();defer{lock.unlock()};sealed=true
        let valid=frames.filter{($0["presented_seconds"] as? Double ?? 0)>0}.sorted{($0["sequence"] as! Int)<($1["sequence"] as! Int)}
        var intervals:[Double]=[],nonmonotonic=0,sequenceGaps=0
        for pair in zip(valid,valid.dropFirst()) {
            let delta=(pair.1["presented_seconds"] as! Double)-(pair.0["presented_seconds"] as! Double)
            if delta<=0 {nonmonotonic+=1} else{intervals.append(delta*1000)}
            if (pair.1["sequence"] as! Int) != (pair.0["sequence"] as! Int)+1 {sequenceGaps+=1}
        }
        var report=metadata
        report["submitted"]=submitted;report["completed"]=completed;report["presented_callbacks"]=frames.count
        report["valid_presented_samples"]=valid.count;report["invalid_presented_time"]=invalidPresented
        report["missing_presented_callbacks"]=submitted-frames.count;report["no_drawable_ticks"]=noDrawable
        report["backpressure_ticks"]=backpressure;report["nonmonotonic_intervals"]=nonmonotonic;report["sequence_gaps"]=sequenceGaps
        report["interval_ms"]=stats(intervals);report["errors"]=errors.sorted()
        report["measurement_complete"]=completed==submitted && submitted==frames.count && invalidPresented==0 && errors.isEmpty && nonmonotonic==0 && valid.count>=5
        let start=metadata["measurement_start_seconds"] as? Double
        let end=metadata["measurement_end_seconds"] as? Double
        report["first_presented_seconds"]=valid.first?["presented_seconds"] ?? NSNull()
        report["last_presented_seconds"]=valid.last?["presented_seconds"] ?? NSNull()
        if let start,let end,let first=valid.first?["presented_seconds"] as? Double,let last=valid.last?["presented_seconds"] as? Double {
            report["leading_uncovered_ms"]=max(0,first-start)*1000
            report["trailing_uncovered_ms"]=max(0,end-last)*1000
        } else {report["leading_uncovered_ms"]=NSNull();report["trailing_uncovered_ms"]=NSNull()}
        report["persistence_contract"]="JSONL records measurement only; require final stdout log_write_confirmed and exit0 for durable completion"
        let rows=[metadata]+frames.sorted{($0["sequence"] as! Int)<($1["sequence"] as! Int)}+[["event":"summary","result":report]]
        var bytes=Data()
        do {
            for row in rows {let d=try JSONSerialization.data(withJSONObject:row,options:.sortedKeys);bytes.append(d);bytes.append(10)}
        } catch {report["log_write_confirmed"]=false;report["log_error"]="serialization_failed";return report}
        guard bytes.count<=4*1024*1024 else{report["log_write_confirmed"]=false;report["log_error"]="size_limit";return report}
        var offset=0,writeOK=true
        bytes.withUnsafeBytes {raw in
            while offset<raw.count {
                let n=write(fd,raw.baseAddress!.advanced(by:offset),raw.count-offset)
                if n<0 && errno==EINTR {continue}
                if n<=0 {writeOK=false;report["log_error"]="write_failed";break}
                offset+=n
            }
        }
        if fsync(fd) != 0 {writeOK=false;report["log_error"]="fsync_failed"}
        report["log_write_confirmed"]=writeOK

        return report
    }
}
private let shader="""
#include <metal_stdlib>
using namespace metal;
struct Vertex {float4 position [[position]]; float2 uv;};
vertex Vertex tri(uint i [[vertex_id]]) {
 const float2 p[3]={float2(-1,-1),float2(3,-1),float2(-1,3)};
 Vertex v;v.position=float4(p[i],0,1);v.uv=p[i]*0.5+0.5;return v;
}
fragment float4 colour(Vertex in [[stage_in]],constant float &t [[buffer(0)]]) {
 float2 q=in.uv; float v=0.5+0.5*sin(q.x*31.0+t)*cos(q.y*19.0-t);
 return float4(q.x,v,q.y,1.0);
}
"""
private final class Runner:NSObject,NSApplicationDelegate,MTKViewDelegate,NSWindowDelegate {
    let recording:Recording,seconds:Int,warmup=5.0
    var window:NSWindow!,view:MTKView!,pipeline:MTLRenderPipelineState!,queue:MTLCommandQueue!
    let inflight=DispatchSemaphore(value:2)
    var visibility:[[String:Any]]=[]
    var passed=false
    var began=0.0,sequence=0,finishing=false,metadata:[String:Any]=[:],timer:Timer?
    init(_ r:Recording,_ s:Int){recording=r;seconds=s}
    func applicationDidFinishLaunching(_ notification:Notification) {
        guard let device=MTLCreateSystemDefaultDevice(),let screen=NSScreen.main else{fputs("Missing Metal/display\n",stderr);exit(1)}
        do {
            let library=try device.makeLibrary(source:shader,options:nil),descriptor=MTLRenderPipelineDescriptor()
            descriptor.vertexFunction=library.makeFunction(name:"tri");descriptor.fragmentFunction=library.makeFunction(name:"colour");descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipeline=try device.makeRenderPipelineState(descriptor:descriptor)
        } catch {fputs("Metal pipeline failed\n",stderr);exit(1)}
        guard let q=device.makeCommandQueue() else{fputs("Metal queue failed\n",stderr);exit(1)};queue=q
        window=NSWindow(contentRect:screen.frame,styleMask:.borderless,backing:.buffered,defer:false)
        window.isReleasedWhenClosed=false;window.delegate=self
        view=MTKView(frame:CGRect(origin:.zero,size:screen.frame.size),device:device)
        view.colorPixelFormat = .bgra8Unorm;view.preferredFramesPerSecond=60;view.delegate=self
        view.autoResizeDrawable=true;view.framebufferOnly=true
        window.contentView=view;window.title="Veynode Guest Metal Presentation";window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps:true)
        began=CACurrentMediaTime()
        metadata=["event":"manifest","workload":"fullscreen-sine-v1","model":model(),"os":ProcessInfo.processInfo.operatingSystemVersionString,"device":device.name,"preferred_fps":60,"warmup_seconds":warmup,"duration_seconds":seconds,"scale":screen.backingScaleFactor,"scope":"Guest MTLDrawable presentedTime only; not physical host scanout, input latency or Parallels comparison","timestamp_basis":"Guest monotonic host time as reported by Metal","maximum_inflight_gpu_commands":2]
        metadata["measurement_start_seconds"]=began+warmup
        visibility.append(["elapsed_seconds":0,"visible":window.occlusionState.contains(.visible)])
        DispatchQueue.global().asyncAfter(deadline:.now()+Double(seconds)+warmup+10) {
            let text="presentation_probe_hard_timeout; no complete record\n"
            text.withCString{_ = write(STDERR_FILENO,$0,strlen($0))};_exit(124)
        }
        timer=Timer.scheduledTimer(withTimeInterval:Double(seconds)+warmup,repeats:false){[weak self] _ in self?.finish()}
    }
    func mtkView(_ view:MTKView,drawableSizeWillChange size:CGSize) {
        if metadata["pixel_width"] != nil && !finishing {recording.mutate{recording.errors.insert("drawable_size_changed")}}
    }
    func draw(in view:MTKView) {
        guard !finishing else{return}
        let elapsed=CACurrentMediaTime()-began,measured=elapsed>=warmup
        guard elapsed<Double(seconds)+warmup else {finish();return}
        guard inflight.wait(timeout:.now()) == .success else {if measured{recording.mutate{recording.backpressure+=1}};return}
        guard let drawable=view.currentDrawable,let pass=view.currentRenderPassDescriptor,let command=queue.makeCommandBuffer(),let encoder=command.makeRenderCommandEncoder(descriptor:pass) else {
            inflight.signal();if measured{recording.mutate{recording.noDrawable+=1}};return
        }
        if measured && metadata["pixel_width"] == nil {metadata["pixel_width"]=drawable.texture.width;metadata["pixel_height"]=drawable.texture.height}
        if measured && ((metadata["pixel_width"] as? Int) != drawable.texture.width || (metadata["pixel_height"] as? Int) != drawable.texture.height) {recording.mutate{recording.errors.insert("pixel_dimensions_changed")}}
        var t=Float(elapsed);encoder.setRenderPipelineState(pipeline);encoder.setFragmentBytes(&t,length:MemoryLayout<Float>.size,index:0)
        encoder.drawPrimitives(type:.triangle,vertexStart:0,vertexCount:3);encoder.endEncoding()
        if measured {
            sequence+=1;let id=sequence,submittedAt=CACurrentMediaTime()
            recording.mutate{recording.submitted+=1}
            drawable.addPresentedHandler{[recording] d in recording.mutate{
                let time=d.presentedTime
                if !time.isFinite || time<=0 {recording.invalidPresented+=1}
                if recording.frames.count<8000 {recording.frames.append(["event":"guest_presented","sequence":id,"submitted_seconds":submittedAt,"presented_seconds":time.isFinite ? time:0])} else {recording.errors.insert("sample_limit")}
            }}
        }
        command.addCompletedHandler{[recording,inflight] cb in
            if measured {recording.mutate{recording.completed+=1;if cb.status != .completed {recording.errors.insert("command_failed")}}}
            inflight.signal()
        }
        command.present(drawable);command.commit()
    }
    func finish() {
        guard !finishing else{return};finishing=true;timer?.invalidate();view.isPaused=true
        metadata["measurement_end_seconds"]=CACurrentMediaTime()
        metadata["visibility_observations"]=visibility
        metadata["visible_at_finish"]=window.occlusionState.contains(.visible)
        // Keep the drawable visible while bounded final callbacks drain; never invent missing frames.
        DispatchQueue.main.asyncAfter(deadline:.now()+2){[self] in
            let report=recording.finish(metadata)
            passed=report["measurement_complete"] as? Bool == true && report["log_write_confirmed"] as? Bool == true
            if let data=try? JSONSerialization.data(withJSONObject:report,options:[.sortedKeys]) {print(String(decoding:data,as:UTF8.self))}
            fflush(stdout);NSApp.terminate(nil)
        }
    }
    func windowDidChangeOcclusionState(_ notification:Notification) {
        if began>0 && visibility.count<64 {visibility.append(["elapsed_seconds":CACurrentMediaTime()-began,"visible":window.occlusionState.contains(.visible)])}
    }
    func windowWillClose(_ notification:Notification){recording.mutate{recording.errors.insert("window_closed_early")};finish()}
}
private func selfTest()throws {
    precondition((stats([1,2,3,4,100])["p95"] as? Double)==100)
    let path=NSTemporaryDirectory()+"veynode-present-"+UUID().uuidString
    defer{try? FileManager.default.removeItem(atPath:path)}
    let r=try Recording(path);r.submitted=6;r.completed=6
    for i in 0..<5 {r.frames.append(["sequence":i+1,"presented_seconds":Double(i+1)/60])}
    let v=r.finish([:]);precondition(v["measurement_complete"] as? Bool == false && v["missing_presented_callbacks"] as? Int == 1)
    let before=try Data(contentsOf:URL(fileURLWithPath:path))
    do{_=try Recording(path);preconditionFailure("overwrite")}catch{}
    let after=try Data(contentsOf:URL(fileURLWithPath:path));precondition(after==before)
    let second=path+"-gaps";defer{try? FileManager.default.removeItem(atPath:second)}
    let r2=try Recording(second);r2.submitted=5;r2.completed=5
    for i in 0..<5 {r2.frames.append(["sequence":i+1,"presented_seconds":10+Double(i)/60])}
    let gaps=r2.finish(["measurement_start_seconds":9.0,"measurement_end_seconds":70.0])
    precondition((gaps["leading_uncovered_ms"] as? Double)==1000)
    precondition((gaps["trailing_uncovered_ms"] as? Double ?? 0)>59000)
    precondition(gaps["measurement_complete"] as? Bool == true && gaps["log_write_confirmed"] as? Bool == true)
    print("PASS: percentile/missing-callback/new-only recorder; no rendering launched")
}
let args=Array(CommandLine.arguments.dropFirst())
if args==["--self-test"] {do{try selfTest();exit(0)}catch{exit(1)}}
guard args.count==4,args[0]=="--log",args[2]=="--seconds",let seconds=Int(args[3]),(10...120).contains(seconds) else{fputs("Usage: guest-metal-presentation --log NEW_ABSOLUTE_JSONL --seconds 10..120 | --self-test\n",stderr);exit(2)}
guard getuid() != 0,model().hasPrefix("VirtualMac") else{fputs("Only non-root VirtualMac Guest allowed; no host render started\n",stderr);exit(1)}
private let recording:Recording
do{recording=try Recording(args[1])}catch{fputs("Cannot create new private log\n",stderr);exit(1)}
let app=NSApplication.shared;app.setActivationPolicy(.regular)
private let runner=Runner(recording,seconds);app.delegate=runner;app.run();exit(runner.passed ? 0:1)
