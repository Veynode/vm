import Foundation
import Darwin
import Metal

let maxRequest = 65_536
let maxOutput = 65_536
struct AgentError: LocalizedError { let message:String; var errorDescription:String? {message} }
func errorReply(_ id:String,_ code:String,_ message:String) -> [String:Any] { ["id":id,"error":["code":code,"message":message]] }
func systemString(_ name:String) -> String {
    var size=0
    guard sysctlbyname(name,nil,&size,nil,0)==0,size>0 else {return "unknown"}
    var buffer=[CChar](repeating:0,count:size)
    guard sysctlbyname(name,&buffer,&size,nil,0)==0 else {return "unknown"}
    return String(cString:buffer)
}
// Reserve the child PID/PGID until group cleanup finishes. No waitpid call is
// permitted before the last group signal, including the ordinary-exit path.
func childExitedWithoutReaping(_ pid:pid_t) throws -> Bool {
    var information=siginfo_t()
    while waitid(P_PID,id_t(pid),&information,WEXITED|WNOHANG|WNOWAIT) != 0 {
        if errno==EINTR {continue}
        throw AgentError(message:"waitid failed: \(String(cString:strerror(errno)))")
    }
    return information.si_pid==pid
}
func signalOwnedCommandGroup(_ pid:pid_t) throws {
    if kill(-pid,SIGKILL)==0 {return}
    let failure=errno
    if failure==ESRCH {return}
    // Darwin reports EPERM for a group containing only its unreaped zombie.
    // Confirm both its waitable identity and exact group membership; a live or
    // foreign-credential member must not be mistaken for successful cleanup.
    if try failure==EPERM && childExitedWithoutReaping(pid) {
        var members=[pid_t](repeating:0,count:1024)
        let bytes=members.withUnsafeMutableBytes { buffer in
            proc_listpids(UInt32(PROC_PGRP_ONLY),UInt32(pid),buffer.baseAddress,Int32(buffer.count))
        }
        if bytes==MemoryLayout<pid_t>.size && members[0]==pid {return}
    }
    throw AgentError(message:"process group cleanup failed: \(String(cString:strerror(failure)))")
}
func reapOwnedCommand(_ pid:pid_t,_ status:inout Int32) throws {
    while true {
        let result=waitpid(pid,&status,0)
        if result==pid {return}
        if result<0 && errno==EINTR {continue}
        throw AgentError(message:"command reap failed")
    }
}
func command(_ argv:[String],timeoutMS:Int) throws -> [String:Any] {
    var outPipe=[Int32](repeating:0,count:2),errPipe=[Int32](repeating:0,count:2)
    guard pipe(&outPipe)==0 else {throw AgentError(message:"stdout pipe failed")}
    defer { close(outPipe[0]);close(outPipe[1]) }
    guard pipe(&errPipe)==0 else {throw AgentError(message:"stderr pipe failed")}
    defer { close(errPipe[0]);close(errPipe[1]) }
    for fd in outPipe+errPipe { _ = fcntl(fd,F_SETFD,FD_CLOEXEC) }
    var actions:posix_spawn_file_actions_t?
    guard posix_spawn_file_actions_init(&actions)==0 else {throw AgentError(message:"spawn actions failed")}
    defer {posix_spawn_file_actions_destroy(&actions)}
    _ = posix_spawn_file_actions_addopen(&actions,STDIN_FILENO,"/dev/null",O_RDONLY,0)
    _ = posix_spawn_file_actions_adddup2(&actions,outPipe[1],STDOUT_FILENO)
    _ = posix_spawn_file_actions_adddup2(&actions,errPipe[1],STDERR_FILENO)
    for fd in outPipe+errPipe { _ = posix_spawn_file_actions_addclose(&actions,fd) }
    var attributes:posix_spawnattr_t?
    guard posix_spawnattr_init(&attributes)==0 else {throw AgentError(message:"spawn attributes failed")}
    defer {posix_spawnattr_destroy(&attributes)}
    guard posix_spawnattr_setpgroup(&attributes,0)==0,
          posix_spawnattr_setflags(&attributes,Int16(POSIX_SPAWN_SETPGROUP))==0 else {throw AgentError(message:"private process group setup failed")}
    let argvPointers:[UnsafeMutablePointer<CChar>?]=argv.map{strdup($0)}+[nil]
    let home=NSHomeDirectory()
    let envStrings:[String]=["PATH=/usr/bin:/bin:/usr/sbin:/sbin","LANG=en_US.UTF-8","HOME=\(home)","USER=\(NSUserName())"]
    let envPointers:[UnsafeMutablePointer<CChar>?]=envStrings.map{strdup($0)}+[nil]
    defer {for pointer in argvPointers+envPointers {free(pointer)}}
    var pid:pid_t=0
    let start=DispatchTime.now().uptimeNanoseconds
    let spawnStatus=argvPointers.withUnsafeBufferPointer{arg in envPointers.withUnsafeBufferPointer{env in
        posix_spawnp(&pid,argv[0],&actions,&attributes,arg.baseAddress!,env.baseAddress!)
    }}
    guard spawnStatus==0 else {throw AgentError(message:"Could not execute command: \(String(cString:strerror(spawnStatus)))")}
    close(outPipe[1]);outPipe[1] = -1
    close(errPipe[1]);errPipe[1] = -1
    for fd in [outPipe[0],errPipe[0]] { _ = fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)|O_NONBLOCK) }
    var outputs=[Data(),Data()],truncated=[false,false],openStreams=[true,true]
    var processStatus:Int32=0,exited=false,timedOut=false
    var drainDeadline:UInt64?
    let deadline=start+UInt64(timeoutMS)*1_000_000
    var buffer=[UInt8](repeating:0,count:8192)
    while true {
        let now=DispatchTime.now().uptimeNanoseconds
        if !exited {
            let waitable=try childExitedWithoutReaping(pid)
            if waitable || now >= deadline {
                timedOut = !waitable
                do {try signalOwnedCommandGroup(pid)}
                catch {
                    // A waitable leader can be reaped safely even if a remaining
                    // member could not be signalled; report failure, not success.
                    if waitable {try reapOwnedCommand(pid,&processStatus)}
                    throw error
                }
                try reapOwnedCommand(pid,&processStatus)
                exited=true;drainDeadline=now+250_000_000
            }
        }
        if exited && (!openStreams.contains(true) || now >= (drainDeadline ?? now)) {break}
        var descriptors=[pollfd(fd:openStreams[0] ? outPipe[0]:-1,events:Int16(POLLIN),revents:0),pollfd(fd:openStreams[1] ? errPipe[0]:-1,events:Int16(POLLIN),revents:0)]
        _ = poll(&descriptors,2,10)
        for i in 0..<2 where openStreams[i] {
            if descriptors[i].revents == 0 {continue}
            while true {
                let count=read(descriptors[i].fd,&buffer,buffer.count)
                if count>0 {
                    let remaining=maxOutput-outputs[i].count
                    outputs[i].append(contentsOf:buffer.prefix(min(count,remaining)))
                    if count>remaining {truncated[i]=true}
                    // Bound per-iteration work even if a writer floods the pipe.
                    break
                }
                if count==0 {openStreams[i]=false;break}
                if errno==EINTR {continue}
                if errno != EAGAIN {openStreams[i]=false}
                break
            }
        }
    }
    for i in 0..<2 where openStreams[i] {truncated[i]=true}
    let exitCode=(processStatus & 0x7f)==0 ? Int((processStatus>>8)&0xff):-1
    return ["exit_code":exitCode,"stdout":String(decoding:outputs[0],as:UTF8.self),"stderr":String(decoding:outputs[1],as:UTF8.self),"stdout_truncated":truncated[0],"stderr_truncated":truncated[1],"timed_out":timedOut,"duration_ms":Int((DispatchTime.now().uptimeNanoseconds-start)/1_000_000)]
}
func handle(_ request:[String:Any],allowExec:Bool,allowObserve:Bool=false,allowInput:Bool=false,
            consoleRead:()->ConsoleIdentityObservation={GuestConsoleIdentityReader.observe()},
            graphicsRead:()->[String:Any]={readGuestGraphicsFields()}) -> [String:Any] {
    let id=request["id"] as? String ?? ""
    guard !id.isEmpty,id.utf8.count<=128,let method=request["method"] as? String else {return errorReply(id,"invalid_request","id must be 1..128 bytes and method must be string")}
    if guestFileMethods.contains(method) {
        guard let files=guestFiles else{return errorReply(id,"unsupported","file integration disabled at service launch")}
        return files.call(request)
    }
    guard Set(request.keys).isSubset(of:["id","method","argv","timeout_ms","arguments"]) else {return errorReply(id,"invalid_request","unknown request fields")}
    switch method {
    case "info":
        let console=consoleRead()
        if !console.identityEligible {guestInput.invalidate()}
        let fields=consoleInfoFields(console,observation:observationPolicy(allowObserve),input:inputPolicy(allowInput))
        let observation=fields["observation"] as! [String:Any],input=fields["input_policy"] as! [String:Any]
        var info:[String:Any]=["protocol_version":1,"os":"darwin","arch":"arm64","os_release":ProcessInfo.processInfo.operatingSystemVersionString,"kernel":systemString("kern.osrelease"),"machine":systemString("hw.model"),"uid":getuid(),"username":NSUserName(),"transport":"vsock-host-only","capabilities":["exec":allowExec,"async_exec":false,"shutdown":false,"desktop":input["available"] as! Bool,"input":input["available"] as! Bool,"screenshot":observation["screenshot_available"] as! Bool,"files":false],"max_output_bytes_per_stream":maxOutput,"max_timeout_ms":30000]
        info.merge(graphicsRead()){_,new in new}
        let windows=windowObservationPolicy(allowObserve,console:console)
        info["window_policy"]=windows
        var observationCapabilities=info["capabilities"] as! [String:Bool]
        observationCapabilities["windows"]=windows["windows_available"] as? Bool ?? false
        info["capabilities"]=observationCapabilities
        if let files=guestFiles {
            let policy=files.call(["id":"files-info","method":"file_worker_info"])
            info["file_policy"]=policy["result"] ?? ["error":policy["error"] ?? [:]]
            var capabilities=info["capabilities"] as! [String:Bool];capabilities["files"]=policy["result"] != nil;info["capabilities"]=capabilities
        }
        info.merge(fields){_,new in new}
        return ["id":id,"result":info]
    case "screenshot":
        guard Set(request.keys)==Set(["id","method"]) else{return errorReply(id,"invalid_request","screenshot accepts only id and method")}
        return screenshotReply(id,enabled:allowObserve,registerInput:allowInput)
    case "windows":
        guard Set(request.keys)==Set(["id","method"]) else{return errorReply(id,"invalid_request","windows accepts only id and method")}
        return windowsReply(id,enabled:allowObserve)
    case "input_step","input_cleanup","input_revoke":
        guard Set(request.keys)==Set(["id","method","arguments"]),let args=request["arguments"] as? [String:Any] else {return errorReply(id,"invalid_request","input requires id, method, arguments")}
        return inputReply(id,method:method,args:args,enabled:allowInput)
    case "exec":
        guard allowExec else {return errorReply(id,"forbidden","command execution disabled at guest service launch")}
        guard let argv=request["argv"] as? [String],!argv.isEmpty,argv.count<=128,!argv[0].isEmpty,
              let timeout=request["timeout_ms"] as? NSNumber,CFGetTypeID(timeout) != CFBooleanGetTypeID(),timeout.doubleValue.rounded()==timeout.doubleValue,timeout.intValue>=1,timeout.intValue<=30000,
              !argv.contains(where:{$0.utf8.contains(0)}) else {return errorReply(id,"invalid_request","argv required without NUL; timeout_ms must be integer 1..30000")}
        do {return ["id":id,"result":try command(argv,timeoutMS:timeout.intValue)]}
        catch {return errorReply(id,"exec_failed",error.localizedDescription)}
    default:return errorReply(id,"unsupported","unsupported guest method")
    }
}
func serve(_ fd:Int32,allowExec:Bool,allowObserve:Bool,allowInput:Bool) {
    defer {close(fd)}
    _ = fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)|O_NONBLOCK)
    var noSignal:Int32=1;_ = setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&noSignal,socklen_t(MemoryLayout<Int32>.size))
    var data=Data(),complete=false
    let readDeadline=DispatchTime.now().uptimeNanoseconds+5_000_000_000
    var buffer=[UInt8](repeating:0,count:4096)
    while data.count<maxRequest && DispatchTime.now().uptimeNanoseconds<readDeadline && !complete {
        var descriptor=pollfd(fd:fd,events:Int16(POLLIN),revents:0)
        if poll(&descriptor,1,100)<=0 {continue}
        let count=read(fd,&buffer,buffer.count)
        if count<0 && (errno==EINTR || errno==EAGAIN) {continue}
        if count<=0 {return}
        for byte in buffer.prefix(count) {
            if byte==10 {complete=true;break}
            data.append(byte)
            if data.count==maxRequest {break}
        }
    }
    let reply:[String:Any]
    var failedWriteCleanup:[String:Any]?
    if complete,let value=try? JSONSerialization.jsonObject(with:data) as? [String:Any] {
        reply=handle(value,allowExec:allowExec,allowObserve:allowObserve,allowInput:allowInput)
        if value["method"] as? String=="input_step" {failedWriteCleanup=value["arguments"] as? [String:Any]}
    }
    else {reply=errorReply("","invalid_request","expected newline JSON within 64 KiB")}
    var written=false
    defer {if !written,let args=failedWriteCleanup {_ = try? guestInput.cleanup(args)}}
    guard var output=try? JSONSerialization.data(withJSONObject:reply,options:[.sortedKeys,.withoutEscapingSlashes]) else {return}
    if output.count>=1024*1024 {output=try! JSONSerialization.data(withJSONObject:errorReply(reply["id"] as? String ?? "","response_too_large","response exceeds 1 MiB limit"))}
    output.append(10)
    let writeDeadline=DispatchTime.now().uptimeNanoseconds+2_000_000_000
    output.withUnsafeBytes { bytes in
        var offset=0
        while offset<bytes.count && DispatchTime.now().uptimeNanoseconds<writeDeadline {
            var descriptor=pollfd(fd:fd,events:Int16(POLLOUT),revents:0)
            if poll(&descriptor,1,100)<=0 {continue}
            let count=write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
            if count<0 && (errno==EINTR || errno==EAGAIN) {continue}
            if count<=0 {break}
            offset+=count
        }
        written = offset==bytes.count
    }
}
func run(allowExec:Bool,allowObserve:Bool,allowInput:Bool) throws {
    guard getuid() != 0 else {throw AgentError(message:"Run this service as the intended Guest user, not root")}
    let listener=socket(AF_VSOCK,SOCK_STREAM,0)
    guard listener>=0 else {throw AgentError(message:"AF_VSOCK unavailable: \(String(cString:strerror(errno)))")}
    defer{close(listener)}
    _ = fcntl(listener,F_SETFD,FD_CLOEXEC)
    var address=sockaddr_vm();address.svm_len=UInt8(MemoryLayout<sockaddr_vm>.size);address.svm_family=sa_family_t(AF_VSOCK);address.svm_port=5000;address.svm_cid=UInt32.max
    let status=withUnsafePointer(to:&address){$0.withMemoryRebound(to:sockaddr.self,capacity:1){Darwin.bind(listener,$0,socklen_t(MemoryLayout<sockaddr_vm>.size))}}
    guard status==0,listen(listener,16)==0 else {throw AgentError(message:"VSOCK listen failed: \(String(cString:strerror(errno)))")}
    FileHandle.standardError.write(Data("veynode macOS guest agent protocol=1 port=5000 hostCID=2 uid=\(getuid()) exec=\(allowExec) observe=\(allowObserve)\n".utf8))
    let capacity=DispatchSemaphore(value:16)
    while true {
        var peer=sockaddr_vm(),length=socklen_t(MemoryLayout<sockaddr_vm>.size)
        let fd=withUnsafeMutablePointer(to:&peer){$0.withMemoryRebound(to:sockaddr.self,capacity:1){accept(listener,$0,&length)}}
        if fd<0 {if errno==EINTR{continue};throw AgentError(message:"VSOCK accept failed")}
        _ = fcntl(fd,F_SETFD,FD_CLOEXEC)
        guard length==MemoryLayout<sockaddr_vm>.size,peer.svm_family==AF_VSOCK,peer.svm_cid==2,capacity.wait(timeout:.now()) == .success else {close(fd);continue}
        DispatchQueue.global(qos:.userInitiated).async {serve(fd,allowExec:allowExec,allowObserve:allowObserve,allowInput:allowInput);capacity.signal()}
    }
}
func selfTest() {
    func result(_ value:[String:Any]) -> [String:Any] {guard let result=value["result"] as? [String:Any] else {fatalError("Expected result: \(value)")};return result}
    let forbidden=handle(["id":"1","method":"exec","argv":["/usr/bin/true"],"timeout_ms":1000],allowExec:false)
    precondition((forbidden["error"] as? [String:String])?["code"]=="forbidden")
    let text=result(handle(["id":"2","method":"exec","argv":["/bin/sh","-c","printf '你好'; printf error >&2; exit 7"],"timeout_ms":1000],allowExec:true))
    precondition(text["stdout"] as? String=="你好" && text["stderr"] as? String=="error" && text["exit_code"] as? Int==7)
    let timeout=result(handle(["id":"3","method":"exec","argv":["/bin/sh","-c","sleep 10 & wait"],"timeout_ms":100],allowExec:true))
    precondition(timeout["timed_out"] as? Bool==true && (timeout["duration_ms"] as! Int)<1000)
    let bounded=result(handle(["id":"4","method":"exec","argv":["/bin/sh","-c","/usr/bin/yes x | /usr/bin/head -c 100000"],"timeout_ms":1000],allowExec:true))
    precondition((bounded["stdout"] as! String).utf8.count==maxOutput && bounded["stdout_truncated"] as? Bool==true)
    let bad=handle(["id":"5","method":"exec","argv":["/usr/bin/true"],"timeout_ms":true],allowExec:true)
    precondition((bad["error"] as? [String:String])?["code"]=="invalid_request")
    let marker=URL(fileURLWithPath:NSTemporaryDirectory()).appendingPathComponent("veynode-child-"+UUID().uuidString)
    defer {try? FileManager.default.removeItem(at:marker)}
    let descendants=result(handle(["id":"6","method":"exec","argv":["/bin/sh","-c","(sleep 0.4; printf escaped > \"$1\") & wait","test",marker.path],"timeout_ms":50],allowExec:true))
    precondition(descendants["timed_out"] as? Bool==true)
    usleep(500_000)
    precondition(!FileManager.default.fileExists(atPath:marker.path),"Timed-out descendant survived process group kill")
    let natural=result(handle(["id":"natural","method":"exec","argv":["/bin/sh","-c","(sleep 0.4; printf escaped > \"$1\") & exit 0","test",marker.path],"timeout_ms":1000],allowExec:true))
    precondition(natural["timed_out"] as? Bool==false && natural["exit_code"] as? Int==0)
    usleep(500_000)
    precondition(!FileManager.default.fileExists(atPath:marker.path),"Natural-exit descendant survived pinned group cleanup")
    // Repeated short-lived groups exercise Darwin's zombie-only EPERM path.
    for _ in 0..<32 {
        let short=try! command(["/usr/bin/true"],timeoutMS:1000)
        precondition(short["exit_code"] as? Int==0 && short["timed_out"] as? Bool==false)
    }
    let unknown=handle(["id":"7","method":"info","unexpected":true],allowExec:true)
    precondition((unknown["error"] as? [String:String])?["code"]=="invalid_request")
    print("PASS: macOS Guest handler forbidden, Unicode/nonzero exit, pinned process-group timeout/natural-exit cleanup,32 short-lived groups, bounded output, strict timeout type. Host-only handler tests; vsock Guest acceptance separate.")
}
var rawArguments=Array(CommandLine.arguments.dropFirst())
if rawArguments.count==3,rawArguments[0]=="--file-worker",rawArguments[1]=="--files-root" {exit(runGuestFileWorker(rawArguments[2]))}
var filesRoot:String?
if let index=rawArguments.firstIndex(of:"--files-root"),index+1<rawArguments.count {filesRoot=rawArguments[index+1];rawArguments.removeSubrange(index...index+1)}
let arguments=Set(rawArguments)
if arguments==["--graphics-self-test"] {graphicsSelfTest();exit(0)}
if arguments==["--windows-self-test"] {windowsSelfTest();exit(0)}
if arguments==["--files-self-test"] {filesSelfTest();exit(0)}
if arguments==["--self-test"] {selfTest();captureSelfTest();inputSelfTest();consoleIntegrationSelfTest();graphicsSelfTest();windowsSelfTest();filesSelfTest();exit(0)}
guard rawArguments.count==arguments.count, arguments.contains("--allow-files")==((filesRoot) != nil),arguments.isSubset(of:["--allow-execute","--allow-observe","--allow-input","--allow-files"]), !arguments.contains("--allow-input") || arguments.contains("--allow-observe") else {fputs("Usage: macos-guest-agent [--allow-execute] [--allow-observe [--allow-input]] [--allow-files --files-root ABSOLUTE_PRIVATE_DIRECTORY] | --self-test\n",stderr);exit(2)}
signal(SIGPIPE,SIG_IGN)
if let root=filesRoot {
    do {guestFiles=try GuestFileBridge(binary:URL(fileURLWithPath:CommandLine.arguments[0]).standardizedFileURL.path,root:root)}
    catch {fputs("File integration startup failed; verify non-root private directory and worker availability\n",stderr);exit(1)}
}
if arguments.contains("--allow-input") && !registerGuestInputDisplayNotifications() {fputs("Input requires a non-root VirtualMac Guest and display reconfiguration notifications\n",stderr);exit(1)}
let inputTimer=DispatchSource.makeTimerSource(queue:.global(qos:.userInitiated))
if arguments.contains("--allow-input") {inputTimer.schedule(deadline:.now(),repeating:.milliseconds(100));inputTimer.setEventHandler{guestInput.tick()};inputTimer.resume()}
var terminationSources:[DispatchSourceSignal]=[]
for number in [SIGINT,SIGTERM] {signal(number,SIG_IGN);let source=DispatchSource.makeSignalSource(signal:number,queue:.global(qos:.userInitiated));source.setEventHandler{let inputClean=guestInput.shutdown();let filesClean=guestFiles?.stop() ?? true;exit(inputClean && filesClean ? 0:1)};source.resume();terminationSources.append(source)}
DispatchQueue.global(qos:.userInitiated).async {
    do {try run(allowExec:arguments.contains("--allow-execute"),allowObserve:arguments.contains("--allow-observe"),allowInput:arguments.contains("--allow-input"))}
    catch {_ = guestInput.shutdown();_ = guestFiles?.stop();fputs(error.localizedDescription+"\n",stderr);exit(1)}
}
dispatchMain()
