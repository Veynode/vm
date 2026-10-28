import Foundation
import Darwin
import ScreenCaptureKit
import CoreMedia

// Desktop sound for the browser viewer of a macOS container (--allow-audio).
// The Guest plays into the container's virtio sound card, which the host
// discards (Apple Virtualization can only play it on the Mac's own speakers);
// ScreenCaptureKit records the Guest's system audio here and the worker relays
// it into the desktop stream like the Linux agent's (desktop_audio_linux.go).
// It needs the Screen Recording permission screenshots use, and never asks for it.
//
// vsock 5002, one connection at a time, host CID 2 only:
//   host -> guest {"audio":true|false} starts or stops the capture; other lines are ignored
//   guest -> host u32 BE length, u8 kind, body:
//     4: u32 BE rate, u8 channels, s16le PCM (48 kHz stereo, 20 ms per record)
//     1: {"event":"audio_unavailable","message"} when the capture cannot start
let audioPort:UInt32=5002
let audioRate=48000,audioChunkBytes=audioRate/50*2*2

/// Float samples, planar (one array per channel) or interleaved in one array, as interleaved s16le.
func audioS16(_ channels:[[Float]],interleaved:Bool,channelCount:Int) -> [UInt8] {
    let samples:[Float]
    if interleaved {samples=channels.first ?? []}
    else {
        let frames=channels.map{$0.count}.min() ?? 0
        var mixed=[Float](repeating:0,count:frames*channelCount)
        for frame in 0..<frames {for c in 0..<channelCount {mixed[frame*channelCount+c]=channels[min(c,channels.count-1)][frame]}}
        samples=mixed
    }
    var out=[UInt8](repeating:0,count:samples.count*2)
    for (i,sample) in samples.enumerated() {
        let value=Int16(max(-32768,min(32767,(sample*32767).rounded())))
        out[2*i]=UInt8(truncatingIfNeeded:value);out[2*i+1]=UInt8(truncatingIfNeeded:value>>8)
    }
    return out
}
/// One stream record: u32 BE length, kind, body.
func audioFrame(_ kind:UInt8,_ body:[UInt8]) -> [UInt8] {
    let length=UInt32(1+body.count)
    return [UInt8(length>>24),UInt8(truncatingIfNeeded:length>>16),UInt8(truncatingIfNeeded:length>>8),UInt8(truncatingIfNeeded:length),kind]+body
}
/// A kind-4 record of stereo 48 kHz PCM.
func audioRecord(_ pcm:ArraySlice<UInt8>) -> [UInt8] {
    let rate=UInt32(audioRate)
    return audioFrame(4,[UInt8(rate>>24),UInt8(truncatingIfNeeded:rate>>16),UInt8(truncatingIfNeeded:rate>>8),UInt8(truncatingIfNeeded:rate),2]+pcm)
}

/// One viewer's connection: its writes go through a serial queue that drops audio past a short backlog.
private final class AudioConnection:NSObject,SCStreamOutput,SCStreamDelegate {
    let fd:Int32
    private let writes=DispatchQueue(label:"audio-writes"),samples=DispatchQueue(label:"audio-samples")
    private let lock=NSLock()
    private var backlog=0,closed=false,stream:SCStream?,starting=false,pending:[UInt8]=[]
    init(fd:Int32) {self.fd=fd}

    func send(_ bytes:[UInt8],droppable:Bool) {
        lock.lock()
        guard !closed,!droppable || backlog<8 else {lock.unlock();return}
        backlog+=1;lock.unlock()
        writes.async {
            let ok=bytes.withUnsafeBytes {buffer -> Bool in
                var offset=0
                while offset<buffer.count {
                    let n=Darwin.write(self.fd,buffer.baseAddress!+offset,buffer.count-offset)
                    if n<0 && errno==EINTR {continue}
                    guard n>0 else {return false}
                    offset+=n
                }
                return true
            }
            self.lock.lock();self.backlog-=1;if !ok {self.closed=true};self.lock.unlock()
            if !ok {shutdown(self.fd,SHUT_RDWR)}
        }
    }
    func unavailable(_ message:String) {
        send(audioFrame(1,[UInt8]((try? JSONSerialization.data(withJSONObject:["event":"audio_unavailable","message":message])) ?? Data())),droppable:false)
    }

    func start() {
        lock.lock();guard stream==nil,!starting,!closed else {lock.unlock();return};starting=true;lock.unlock()
        guard systemString("hw.model").hasPrefix("VirtualMac"),getuid() != 0 else {finishStart(nil);unavailable("sound capture runs only as the signed-in user of a macOS Guest");return}
        guard CGPreflightScreenCaptureAccess() else {finishStart(nil);unavailable("the Guest service has no Screen Recording permission; approve it in the Guest's Privacy & Security settings");return}
        SCShareableContent.getExcludingDesktopWindows(false,onScreenWindowsOnly:true) {content,error in
            guard let display=content?.displays.first else {self.finishStart(nil);self.unavailable("ScreenCaptureKit found no Guest display");return}
            let config=SCStreamConfiguration()
            config.capturesAudio=true;config.sampleRate=audioRate;config.channelCount=2;config.excludesCurrentProcessAudio=true
            // Audio rides a display stream; keep its video as small and slow as allowed.
            config.width=2;config.height=2;config.minimumFrameInterval=CMTime(value:1,timescale:1);config.queueDepth=3
            let stream=SCStream(filter:SCContentFilter(display:display,excludingWindows:[]),configuration:config,delegate:self)
            do {try stream.addStreamOutput(self,type:.audio,sampleHandlerQueue:self.samples)}
            catch {self.finishStart(nil);self.unavailable("ScreenCaptureKit refused the audio output: \(error.localizedDescription)");return}
            stream.startCapture {error in
                if let error {self.finishStart(nil);self.unavailable("ScreenCaptureKit could not start: \(error.localizedDescription)");return}
                if !self.finishStart(stream) {stream.stopCapture{_ in}}
            }
        }
    }
    /// Publishes a started stream; false when the viewer stopped or left meanwhile.
    @discardableResult private func finishStart(_ started:SCStream?) -> Bool {
        lock.lock();defer {lock.unlock()}
        starting=false
        guard let started,!closed,wanted else {return started==nil}
        stream=started;return true
    }
    private var wanted=false
    func set(_ on:Bool) {
        lock.lock();wanted=on;let running=stream;if !on {stream=nil;pending=[]};lock.unlock()
        if on {start()} else {running?.stopCapture{_ in}}
    }
    func close() {
        lock.lock();closed=true;wanted=false;let running=stream;stream=nil;lock.unlock()
        running?.stopCapture{_ in}
    }

    func stream(_ stream:SCStream,didOutputSampleBuffer buffer:CMSampleBuffer,of type:SCStreamOutputType) {
        guard type == .audio,buffer.isValid,let format=buffer.formatDescription?.audioStreamBasicDescription,
              format.mFormatID==kAudioFormatLinearPCM,format.mFormatFlags & kAudioFormatFlagIsFloat != 0,format.mBitsPerChannel==32,
              Int(format.mSampleRate)==audioRate,format.mChannelsPerFrame>=1 else {return}
        let interleaved=format.mFormatFlags & kAudioFormatFlagIsNonInterleaved==0
        var planes:[[Float]]=[]
        try? buffer.withAudioBufferList {list,_ in
            for audio in list {
                guard let data=audio.mData else {continue}
                planes.append(Array(UnsafeBufferPointer(start:data.assumingMemoryBound(to:Float.self),count:Int(audio.mDataByteSize)/4)))
            }
        }
        guard !planes.isEmpty else {return}
        // Two channels whatever the source: mono is doubled, extra channels are dropped.
        let source=Int(format.mChannelsPerFrame)
        let stereo:[[Float]]
        if interleaved {
            let all=planes[0],frames=all.count/source
            stereo=[(0..<frames).map{all[$0*source]},(0..<frames).map{all[$0*source+min(1,source-1)]}]
        } else {stereo=[planes[0],planes[min(1,planes.count-1)]]}
        lock.lock();pending+=audioS16(stereo,interleaved:false,channelCount:2)
        var chunks:[ArraySlice<UInt8>]=[]
        while pending.count>=audioChunkBytes {chunks.append(pending[..<audioChunkBytes]);pending.removeFirst(audioChunkBytes)}
        lock.unlock()
        for chunk in chunks {send(audioRecord(chunk),droppable:true)}
    }
    func stream(_ stream:SCStream,didStopWithError error:Error) {
        lock.lock();let current=self.stream===stream;if current {self.stream=nil};lock.unlock()
        if current {unavailable("ScreenCaptureKit stopped: \(error.localizedDescription)")}
    }
}

/// Reads the host's lines until it leaves; the capture stops with it.
func serveAudio(_ fd:Int32) {
    defer {close(fd)}
    var noSignal:Int32=1;_ = setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&noSignal,socklen_t(MemoryLayout<Int32>.size))
    let connection=AudioConnection(fd:fd)
    defer {connection.close()}
    var pending:[UInt8]=[],buffer=[UInt8](repeating:0,count:4096)
    while true {
        let n=read(fd,&buffer,buffer.count)
        if n<0 && errno==EINTR {continue}
        guard n>0 else {return}
        pending+=buffer[..<n]
        while let newline=pending.firstIndex(of:10) {
            let line=Data(pending[..<newline]);pending.removeSubrange(...newline)
            if let on=((try? JSONSerialization.jsonObject(with:line)) as? [String:Any])?["audio"] as? Bool {connection.set(on)}
        }
        // Microphone lines (up to 2.6 KiB) are not used; anything longer is not a line we know.
        guard pending.count<=8192 else {return}
    }
}

/// The audio listener: one host connection at a time.
func runAudio() throws {
    let listener=socket(AF_VSOCK,SOCK_STREAM,0)
    guard listener>=0 else {throw AgentError(message:"AF_VSOCK unavailable: \(String(cString:strerror(errno)))")}
    _ = fcntl(listener,F_SETFD,FD_CLOEXEC)
    var address=sockaddr_vm();address.svm_len=UInt8(MemoryLayout<sockaddr_vm>.size);address.svm_family=sa_family_t(AF_VSOCK);address.svm_port=audioPort;address.svm_cid=UInt32.max
    let status=withUnsafePointer(to:&address){$0.withMemoryRebound(to:sockaddr.self,capacity:1){Darwin.bind(listener,$0,socklen_t(MemoryLayout<sockaddr_vm>.size))}}
    guard status==0,listen(listener,1)==0 else {throw AgentError(message:"VSOCK audio listen failed: \(String(cString:strerror(errno)))")}
    while true {
        var peer=sockaddr_vm(),length=socklen_t(MemoryLayout<sockaddr_vm>.size)
        let fd=withUnsafeMutablePointer(to:&peer){$0.withMemoryRebound(to:sockaddr.self,capacity:1){accept(listener,$0,&length)}}
        if fd<0 {if errno==EINTR{continue};throw AgentError(message:"VSOCK audio accept failed")}
        _ = fcntl(fd,F_SETFD,FD_CLOEXEC)
        guard length==MemoryLayout<sockaddr_vm>.size,peer.svm_family==AF_VSOCK,peer.svm_cid==2 else {close(fd);continue}
        // A new viewer replaces the old one: serve it here, on the accepting thread.
        serveAudio(fd)
    }
}

func audioSelfTest() {
    // Planar float to interleaved s16le, clamped.
    precondition(audioS16([[0,1,-1.5],[0.5,-1,2]],interleaved:false,channelCount:2)==[0,0,0,0x40,0xff,0x7f,0x01,0x80,0x00,0x80,0xff,0x7f])
    precondition(audioS16([[0.25,-0.25]],interleaved:true,channelCount:2)==[0x00,0x20,0x00,0xe0])
    let record=audioRecord([1,2,3,4][...])
    precondition(record==[0,0,0,10,4,0,0,0xbb,0x80,2,1,2,3,4])
    precondition(audioChunkBytes==3840)
    if !systemString("hw.model").hasPrefix("VirtualMac") {
        var pair:[Int32]=[-1,-1];precondition(socketpair(AF_UNIX,SOCK_STREAM,0,&pair)==0)
        let connection=AudioConnection(fd:pair[1]);connection.set(true)
        var reply=[UInt8](repeating:0,count:512);var got=0
        var descriptor=pollfd(fd:pair[0],events:Int16(POLLIN),revents:0)
        if poll(&descriptor,1,2000)>0 {got=read(pair[0],&reply,reply.count)}
        precondition(got>5 && reply[4]==1 && String(decoding:reply[5..<got],as:UTF8.self).contains("audio_unavailable"),"host refusal not reported")
        connection.close();close(pair[0]);close(pair[1])
    }
    print("PASS: audio PCM conversion, record framing, 20 ms chunk size, physical-host refusal; no capture performed")
}
