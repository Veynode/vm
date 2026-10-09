import Foundation
import Darwin
import CryptoKit
func filesCheck(_ value:Bool,_ message:String="files assertion failed"){precondition(value,message)}
private final class FileFramingResult: @unchecked Sendable {
    var error: Error?
}
func filesFramingSelfTest() {
    // Use the production reader/writer over real nonblocking pipes, not just
    // Foundation dictionaries. A concurrent reader is required at these sizes.
    func exchange(_ value: [String: Any], raw: Data? = nil) throws -> [String: Any]? {
        var fds: [Int32] = [-1, -1]
        filesCheck(pipe(&fds) == 0)
        for fd in fds { filesCheck(fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)|O_NONBLOCK) == 0) }
        let readFD=fds[0],writeFD=fds[1],done=DispatchSemaphore(value:0),result=FileFramingResult()
        let deadline=DispatchTime.now().uptimeNanoseconds+2_000_000_000
        DispatchQueue.global().async {
            defer { close(writeFD);done.signal() }
            do {
                if let raw=raw {
                    try raw.withUnsafeBytes { bytes in
                        var offset=0
                        while offset<bytes.count {
                            guard DispatchTime.now().uptimeNanoseconds<deadline else {throw fileFailure("timeout","test write")}
                            let n=write(writeFD,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
                            if n<0 && (errno==EAGAIN || errno==EINTR) {var p=pollfd(fd:writeFD,events:Int16(POLLOUT),revents:0);_ = poll(&p,1,10);continue}
                            guard n>0 else {throw fileFailure("unavailable","test pipe closed")};offset+=n
                        }
                    }
                } else { try fileWireWrite(writeFD,value,deadline:deadline) }
            } catch { result.error=error }
        }
        defer {close(readFD)}
        let readResult=Result {try fileWireRead(readFD,deadline:deadline)}
        filesCheck(done.wait(timeout:.now()+3) == .success,"framing writer did not finish")
        if let error=result.error {throw error}
        return try readResult.get()
    }
    do {
        let body=Data(repeating:0xff,count:guestFileChunk),base64=body.base64EncodedString()
        let request:[String:Any]=["id":"full","method":"file_upload_write","owner":String(repeating:"a",count:64),"token":String(repeating:"b",count:32),"offset":8355840,"data":base64]
        let response:[String:Any]=["id":"full","result":["data":base64,"offset":8388608,"eof":true,"sha256":String(repeating:"c",count:64)]]
        for value in [request,response] {
            let decoded=try exchange(value)!
            filesCheck(NSDictionary(dictionary:decoded).isEqual(to:value),"real pipe changed payload")
        }
        for size in [65534,65535,65536] {
            let value:[String:Any]=["data":String(repeating:"x",count:size-11)]
            var raw=try JSONSerialization.data(withJSONObject:value);filesCheck(raw.count==size);raw.append(10)
            for suppliedRaw in [false,true] {
                do {
                    let decoded=try exchange(value,raw:suppliedRaw ? raw:nil)
                    filesCheck(size<65536 && decoded?["data"] as? String==value["data"] as? String,"oversize pipe frame accepted")
                } catch {
                    filesCheck(size==65536 && (error as? GuestFileError)?.code=="invalid_request","unexpected boundary error")
                }
            }
        }
    } catch {fatalError("real pipe framing failed: \(error)")}
    print("PASS: real pipe upload/download envelopes preserve full 32KiB all-FF; 65535-byte JSON plus newline accepted, 65536-byte JSON rejected by both writer and reader")
}
func filesSelfTest(){
    signal(SIGPIPE,SIG_IGN)
    filesFramingSelfTest()
    let resolved=realpath(NSTemporaryDirectory(),nil)!;defer{free(resolved)}
    let base=URL(fileURLWithPath:String(cString:resolved)).appendingPathComponent("veynode-files-tests-"+UUID().uuidString)
    try! FileManager.default.createDirectory(at:base,withIntermediateDirectories:false,attributes:[.posixPermissions:0o700])
    defer{try? FileManager.default.removeItem(at:base)}
    var index=0
    func directory()->String{index+=1;let path=base.appendingPathComponent("root-\(index)").path;filesCheck(mkdir(path,0o700)==0);return path}
    let owner=String(repeating:"a",count:64),other=String(repeating:"b",count:64)
    func digest(_ data:Data)->String{SHA256.hash(data:data).map{String(format:"%02x",$0)}.joined()}
    func good(_ reply:[String:Any])->[String:Any]{guard let result=reply["result"] as? [String:Any] else{fatalError("Unexpected file error \(reply)")};return result}
    func bad(_ reply:[String:Any],_ code:String?=nil){guard let error=reply["error"] as? [String:Any] else{fatalError("Expected error \(reply)")};if let code=code {filesCheck(error["code"] as? String==code,"Unexpected code \(error)")}}
    func call(_ m:GuestFiles,_ method:String,_ args:[String:Any]=[:],_ who:String?=nil)->[String:Any]{var req=args;req["id"]="test";req["method"]=method;if method != "file_worker_info"{req["owner"]=who ?? owner};return m.handle(req)}
    func upload(_ m:GuestFiles,_ name:String,_ body:Data)->[String:Any]{
        let started=good(call(m,"file_upload_begin",["path":name,"size":body.count,"sha256":digest(body)]));let token=started["token"] as! String
        var offset=0;while offset<body.count {let part=body.subdata(in:offset..<min(offset+guestFileChunk,body.count));_ = good(call(m,"file_upload_write",["token":token,"offset":offset,"data":part.base64EncodedString()]));offset+=part.count}
        return good(call(m,"file_upload_commit",["token":token]))
    }
    do {
        let path=directory(),m=try GuestFiles(path)
        _ = good(call(m,"file_worker_info"))
        do{_ = try GuestFiles(path);fatalError("duplicate root admitted")}catch{}
        for body in [Data(),Data("你好 / hello\n".utf8),Data(repeating:0xa5,count:guestFileMaximum)] {
            let name="sample-\(body.count)";filesCheck(upload(m,name,body)["published"] as? Bool==true)
            let begun=good(call(m,"file_download_begin",["path":name])),token=begun["token"] as! String
            var result=Data(),offset=0
            repeat{let part=good(call(m,"file_download_read",["token":token,"offset":offset]));let bytes=Data(base64Encoded:part["data"] as! String)!;result.append(bytes);offset=part["offset"] as! Int;if part["eof"] as! Bool{break}}while true
            filesCheck(result==body && begun["sha256"] as? String==digest(body))
            bad(call(m,"file_upload_begin",["path":name,"size":0,"sha256":digest(Data())]),"already_exists")
        }
        for name in ["../escape","a/b","a\\b",".","..",".veynode-staging","","a\0b","../\u{0301}escaped","..\\\u{0301}escaped"] {bad(call(m,"file_download_begin",["path":name]),"invalid_request")}
        for name in ["../\u{0301}escaped","..\\\u{0301}escaped"] {bad(call(m,"file_upload_begin",["path":name,"size":0,"sha256":digest(Data())]),"invalid_request")}
        filesCheck(!FileManager.default.fileExists(atPath:base.appendingPathComponent("\u{0301}escaped").path))
        bad(call(m,"file_upload_begin",["path":"oversize","size":guestFileMaximum+1,"sha256":digest(Data())]),"invalid_request")
        bad(call(m,"file_upload_begin",["path":"bool","size":true,"sha256":digest(Data())]),"invalid_request")
        let token=good(call(m,"file_upload_begin",["path":"corrupt","size":1,"sha256":digest(Data([0]))]))["token"] as! String
        bad(call(m,"file_upload_write",["token":token,"offset":0,"data":"AQ=="],other),"not_found")
        bad(call(m,"file_upload_write",["token":token,"offset":1,"data":"AQ=="]),"invalid_request")
        bad(call(m,"file_upload_write",["token":token,"offset":0,"data":"AQ==\n"]),"invalid_request")
        _ = good(call(m,"file_upload_write",["token":token,"offset":0,"data":"AQ=="]))
        bad(call(m,"file_upload_commit",["token":token]),"integrity_error");filesCheck(try m.stagingNames().isEmpty)
        let race=good(call(m,"file_upload_begin",["path":"race","size":0,"sha256":digest(Data())]))["token"] as! String
        try Data("keep".utf8).write(to:URL(fileURLWithPath:path+"/race"));bad(call(m,"file_upload_commit",["token":race]),"already_exists");filesCheck(try Data(contentsOf:URL(fileURLWithPath:path+"/race"))==Data("keep".utf8))
        filesCheck(symlink("/etc/passwd",path+"/symlink")==0);bad(call(m,"file_download_begin",["path":"symlink"]))
        filesCheck(link(path+"/sample-0",path+"/hardlink")==0);bad(call(m,"file_download_begin",["path":"hardlink"]),"invalid_request")
        filesCheck(mkfifo(path+"/fifo",0o600)==0);bad(call(m,"file_download_begin",["path":"fifo"]),"invalid_request")
        let changing=Data(repeating:7,count:40000);_ = upload(m,"changing",changing)
        let download=good(call(m,"file_download_begin",["path":"changing"]))["token"] as! String
        _ = good(call(m,"file_download_read",["token":download,"offset":0]))
        let fd=open(path+"/changing",O_WRONLY);filesCheck(fd>=0);var changed:UInt8=1;filesCheck(write(fd,&changed,1)==1);close(fd)
        bad(call(m,"file_download_read",["token":download,"offset":guestFileChunk]),"changed")
        let pending=good(call(m,"file_upload_begin",["path":"pending","size":0,"sha256":digest(Data())]))["token"] as! String
        filesCheck(good(call(m,"file_session_end"))["cancelled_transfers"] as? Int==1)
        bad(call(m,"file_upload_commit",["token":pending]),"forbidden");bad(call(m,"file_upload_begin",["path":"late","size":0,"sha256":digest(Data())]),"forbidden")
        _ = good(call(m,"file_session_end",[:],other));bad(call(m,"file_download_begin",["path":"sample-0"],other),"forbidden")
        filesCheck(try m.stagingNames().isEmpty);try m.stop()
    }catch{fatalError("filesystem tests failed: \(error)")}
    do {
        var clock:UInt64=1;let m=try GuestFiles(directory(),now:{clock})
        var tokens:[String]=[]
        for i in 0..<16{tokens.append(good(call(m,"file_upload_begin",["path":"pending\(i)","size":0,"sha256":digest(Data())]))["token"] as! String)}
        bad(call(m,"file_upload_begin",["path":"overflow","size":0,"sha256":digest(Data())]),"busy")
        clock+=60_000_000_000;bad(call(m,"file_upload_commit",["token":tokens[0]]),"not_found");filesCheck(try m.stagingNames().isEmpty)
        _ = good(call(m,"file_session_end"));clock+=299_000_000_000;bad(call(m,"file_upload_begin",["path":"closed","size":0,"sha256":digest(Data())]),"forbidden")
        clock+=1_000_000_000;_ = good(call(m,"file_upload_begin",["path":"expired","size":0,"sha256":digest(Data())]));try m.stop()
    }catch{fatalError("bounds tests failed: \(error)")}
    do {
        let path=directory();let m=try GuestFiles(path);let moved=path+"-moved";filesCheck(rename(path,moved)==0 && mkdir(path,0o700)==0)
        bad(call(m,"file_upload_begin",["path":"wrong-root","size":0,"sha256":digest(Data())]),"unavailable");try m.stop()
        let symlinkPath=base.appendingPathComponent("link-root").path;filesCheck(symlink(path,symlinkPath)==0);do{_ = try GuestFiles(symlinkPath);fatalError("symlink root accepted")}catch{}
        let unsafe=directory();filesCheck(chmod(unsafe,0o755)==0);do{_ = try GuestFiles(unsafe);fatalError("public root accepted")}catch{}
        let aclRoot=directory();let chmodProcess=Process();chmodProcess.executableURL=URL(fileURLWithPath:"/bin/chmod");chmodProcess.arguments=["+a","everyone allow read",aclRoot];try chmodProcess.run();chmodProcess.waitUntilExit();filesCheck(chmodProcess.terminationStatus==0);do{_ = try GuestFiles(aclRoot);fatalError("ACL root accepted")}catch{}
    }catch{fatalError("root identity tests failed: \(error)")}
    do {
        let path=directory()
        var m:GuestFiles?=try GuestFiles(path)
        let linked=path+"/.veynode-staging/.veynode-upload-"+String(repeating:"c",count:32)+".part"
        filesCheck(symlink("/etc/passwd",linked)==0);m=nil
        do{_ = try GuestFiles(path);fatalError("unsafe stale symlink accepted")}catch{}
        filesCheck(try FileManager.default.destinationOfSymbolicLink(atPath:linked)=="/etc/passwd")
        try FileManager.default.removeItem(atPath:linked)
        m=try GuestFiles(path)
        filesCheck(rename(path+"/.veynode-lock",path+"/.veynode-lock-old")==0)
        let replacement=open(path+"/.veynode-lock",O_CREAT|O_EXCL|O_RDWR,0o600);filesCheck(replacement>=0);close(replacement)
        bad(call(m!,"file_worker_info"),"unavailable");m=nil
        var time:UInt64=0;let lifetime=try GuestFiles(directory(),now:{time})
        let token=good(call(lifetime,"file_upload_begin",["path":"lifetime","size":6,"sha256":digest(Data(repeating:0,count:6))]))["token"] as! String
        for i in 0..<5 {time+=50_000_000_000;_ = good(call(lifetime,"file_upload_write",["token":token,"offset":i,"data":"AA=="]))}
        time=300_000_000_000;bad(call(lifetime,"file_upload_write",["token":token,"offset":5,"data":"AA=="]),"not_found")
        filesCheck(try lifetime.stagingNames().isEmpty)
    }catch{fatalError("staging/lock/absolute-lifetime tests failed: \(error)")}
    print("PASS: files real filesystem roundtrip 0/Unicode/8MiB, size/hash/offset/owner bounds, no-replace race, symlink/hardlink/FIFO/ACL refusal, source mutation, owner retirement, TTL/capacity and root replacement")
    do {
        let path=directory(),binary=URL(fileURLWithPath:CommandLine.arguments[0]).standardizedFileURL.path
        let unrelated=Process();unrelated.executableURL=URL(fileURLWithPath:"/bin/sleep");unrelated.arguments=["0.3"];try unrelated.run()
        do{_ = try GuestFileBridge(binary:"/veynode-missing-executable",root:path);fatalError("invalid worker executable started")}catch{}
        filesCheck(unrelated.isRunning && kill(unrelated.processIdentifier,0)==0,"failed initialization touched unrelated child")
        unrelated.waitUntilExit();filesCheck(unrelated.terminationStatus==0)
        let worker=try GuestFileBridge(binary:binary,root:path)
        _ = good(worker.call(["id":"i","method":"file_worker_info"]))
        let full=Data(repeating:0xff,count:guestFileChunk)
        let fullStart=good(worker.call(["id":"full-begin","method":"file_upload_begin","owner":owner,"path":"all-ff","size":full.count,"sha256":digest(full)]))
        let fullToken=fullStart["token"] as! String
        _ = good(worker.call(["id":"full-write","method":"file_upload_write","owner":owner,"token":fullToken,"offset":0,"data":full.base64EncodedString()]))
        filesCheck(good(worker.call(["id":"full-commit","method":"file_upload_commit","owner":owner,"token":fullToken]))["published"] as? Bool==true)
        let fullDownload=good(worker.call(["id":"full-read-begin","method":"file_download_begin","owner":owner,"path":"all-ff"]))["token"] as! String
        let fullRead=good(worker.call(["id":"full-read","method":"file_download_read","owner":owner,"token":fullDownload,"offset":0]))
        filesCheck(Data(base64Encoded:fullRead["data"] as! String)==full && fullRead["eof"] as? Bool==true && fullRead["sha256"] as? String==digest(full),"full binary subprocess roundtrip failed")
        let begin:[String:Any]=["id":"b","method":"file_upload_begin","owner":owner,"path":"partial","size":1,"sha256":digest(Data([1]))]
        _ = good(worker.call(begin));filesCheck(worker.stop(),"worker EOF cleanup failed")
        do{let check=try GuestFiles(path);filesCheck(try check.stagingNames().isEmpty)}
        let killed=try GuestFileBridge(binary:binary,root:path);_ = good(killed.call(begin));filesCheck(kill(killed.pid,SIGKILL)==0)
        bad(killed.call(["id":"dead","method":"file_worker_info"]));bad(killed.call(begin),"unavailable")
        let recovered=try GuestFileBridge(binary:binary,root:path);filesCheck(good(recovered.call(["id":"recover","method":"file_worker_info"]))["staged_upload_files"] as? Int==0);filesCheck(recovered.stop())
        let stalled=try GuestFileBridge(binary:binary,root:path);filesCheck(kill(stalled.pid,SIGSTOP)==0)
        let start=DispatchTime.now().uptimeNanoseconds;bad(stalled.call(["id":"stall","method":"file_worker_info"]),"timeout");filesCheck(DispatchTime.now().uptimeNanoseconds-start<7_000_000_000);bad(stalled.call(begin),"unavailable")
        var status:Int32=0;filesCheck(waitpid(stalled.pid,&status,WNOHANG) == -1 && errno==ECHILD,"owned stalled child not reaped")
    }catch{fatalError("worker subprocess tests failed: \(error)")}
    print("PASS: files real subprocess private RPC, graceful EOF removes staging, killed worker fences, fresh worker recovers exact staging, SIGSTOP timeout kills/reaps within7s; no Guest acceptance or sandbox claim")
}
