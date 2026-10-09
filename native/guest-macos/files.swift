import Foundation
import Darwin
import CryptoKit

// This is a fixed file protocol, not an arbitrary command worker or a sandbox.
// It runs as the same non-root Guest user. All path access is relative to pinned
// private directories; same-user applications remain inside the trust boundary.
let guestFileMethods:Set<String> = ["file_upload_begin","file_upload_write","file_upload_commit","file_download_begin","file_download_read","file_cancel","file_session_end"]
let guestFileChunk=32768, guestFileMaximum=8*1024*1024
struct GuestFileError:Error {let code:String;let message:String}
func fileFailure(_ code:String,_ message:String)->GuestFileError {GuestFileError(code:code,message:message)}
func fileSystemFailure(_ operation:String)->GuestFileError {
    let code=errno==EEXIST ? "already_exists":errno==ENOENT ? "not_found":errno==EACCES || errno==EPERM ? "forbidden":"file_error"
    return fileFailure(code,"\(operation) failed (errno \(errno))")
}
func fileHex(_ value:String,_ count:Int)->Bool {value.utf8.count==count && value.utf8.allSatisfy{(48...57).contains($0)||(97...102).contains($0)}}
func fileInteger(_ value:Any?,max:Int)->Int? {
    guard let n=value as? NSNumber,CFGetTypeID(n) != CFBooleanGetTypeID(),n.doubleValue.isFinite,n.doubleValue.rounded()==n.doubleValue,n.doubleValue>=0,n.doubleValue<=Double(max) else{return nil};return n.intValue
}
func fileNameValid(_ name:String)->Bool {!name.isEmpty && name.utf8.count<=255 && name != "." && name != ".." && !name.hasPrefix(".veynode-") && !name.utf8.contains(where:{$0==47 || $0==92 || $0==0})}
func fileStat(_ fd:Int32)throws->stat {var s=stat();guard fstat(fd,&s)==0 else{throw fileSystemFailure("stat")};return s}
func fileSame(_ a:stat,_ b:stat)->Bool {a.st_dev==b.st_dev && a.st_ino==b.st_ino}
func fileUnchanged(_ a:stat,_ b:stat)->Bool {fileSame(a,b) && a.st_size==b.st_size && a.st_mtimespec.tv_sec==b.st_mtimespec.tv_sec && a.st_mtimespec.tv_nsec==b.st_mtimespec.tv_nsec && a.st_ctimespec.tv_sec==b.st_ctimespec.tv_sec && a.st_ctimespec.tv_nsec==b.st_ctimespec.tv_nsec && b.st_nlink==1 && b.st_uid==getuid()}
func fileNoACL(_ fd:Int32)throws {
    // acl_get_fd_np alone ambiguously returns nil/ENOENT when FILESEC_ACL is
    // absent. Query an fstatx_np result explicitly; errors never mean no ACL.
    guard let security=filesec_init() else{throw fileSystemFailure("ACL allocation")};defer{filesec_free(security)}
    var metadata=stat(),present:Int32=0
    guard fstatx_np(fd,&metadata,security)==0,filesec_query_property(security,FILESEC_ACL,&present)==0 else{throw fileSystemFailure("ACL read")}
    guard present==0 else{throw fileFailure("forbidden","extended ACL is unsupported for private files")}
}
func filePrivateDirectory(_ fd:Int32)throws->stat {let s=try fileStat(fd);guard s.st_mode&S_IFMT==S_IFDIR,s.st_uid==getuid(),s.st_mode&0o077==0 else{throw fileFailure("forbidden","files directory must be private and owned by current user")};try fileNoACL(fd);return s}
func fileWriteAll(_ fd:Int32,_ data:Data)throws {
    try data.withUnsafeBytes{bytes in var offset=0;while offset<bytes.count {let n=write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset);if n<0 && errno==EINTR{continue};guard n>0 else{throw fileSystemFailure("write")};offset+=n}}
}
func fileRead(_ fd:Int32,_ count:Int)throws->Data {
    var data=Data(count:count);var offset=0
    try data.withUnsafeMutableBytes{bytes in while offset<count {let n=read(fd,bytes.baseAddress!.advanced(by:offset),count-offset);if n<0 && errno==EINTR{continue};guard n>0 else{throw fileFailure("changed","file ended or read failed")};offset+=n}}
    return data
}
final class GuestTransfer {
    let owner:String,kind:String,path:String,temporary:String,size:Int,digest:String,created:UInt64,initial:stat
    let fd:Int32
    var offset=0,touched:UInt64,hash=SHA256()
    init(owner:String,kind:String,path:String,temporary:String,size:Int,digest:String,fd:Int32,now:UInt64,initial:stat){self.owner=owner;self.kind=kind;self.path=path;self.temporary=temporary;self.size=size;self.digest=digest;self.fd=fd;self.created=now;self.touched=now;self.initial=initial}
    deinit{close(fd)}
}
final class GuestFiles {
    let rootPath:String,root:Int32,staging:Int32,lockFD:Int32,rootIdentity:stat,stagingIdentity:stat
    let now:()->UInt64
    var transfers:[String:GuestTransfer]=[:],closed:[String:UInt64]=[:]
    var fault=false
    init(_ path:String,now:@escaping()->UInt64={DispatchTime.now().uptimeNanoseconds})throws {
        guard getuid() != 0,path.hasPrefix("/"),path.utf8.count<=4096,!path.utf8.contains(0) else{throw fileFailure("forbidden","files require non-root user and an absolute root")}
        let root=open(path,O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW_ANY);guard root>=0 else{throw fileSystemFailure("open files root")}
        var lockFD:Int32 = -1,stage:Int32 = -1,success=false
        defer{if !success {if stage>=0{close(stage)};if lockFD>=0{close(lockFD)};close(root)}}
        let rootIdentity=try filePrivateDirectory(root)
        var volume=statfs();guard fstatfs(root,&volume)==0,volume.f_flags&UInt32(MNT_LOCAL) != 0 else{throw fileFailure("forbidden","files root must be on a local filesystem")}
        lockFD=openat(root,".veynode-lock",O_CREAT|O_RDWR|O_CLOEXEC|O_NOFOLLOW,0o600);guard lockFD>=0 else{throw fileSystemFailure("open files lock")}
        let lockStat=try fileStat(lockFD);guard lockStat.st_mode&S_IFMT==S_IFREG,lockStat.st_uid==getuid(),lockStat.st_nlink==1,lockStat.st_mode&0o077==0 else{throw fileFailure("forbidden","unsafe files lock")};try fileNoACL(lockFD)
        guard flock(lockFD,LOCK_EX|LOCK_NB)==0 else{throw fileFailure("busy","files root already owned by a worker")}
        guard mkdirat(root,".veynode-staging",0o700)==0 || errno==EEXIST else{throw fileSystemFailure("create staging")}
        stage=openat(root,".veynode-staging",O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW);guard stage>=0 else{throw fileSystemFailure("open staging")}
        self.rootPath=path;self.root=root;self.staging=stage;self.lockFD=lockFD;self.rootIdentity=rootIdentity;self.stagingIdentity=try filePrivateDirectory(stage);self.now=now
        success=true // All descriptors are now owned by self, including throwing recovery.
        // Only our exact reserved temporary names and safe inodes can be removed.
        // Any unexpected directory entry fails startup instead of broad deletion.
        try validateRoots()
        for name in try stagingNames() {try removeStale(name)}
        success=true
    }
    deinit {transfers.removeAll();close(staging);close(lockFD);close(root)}
    func validateRoots()throws {
        let current=open(rootPath,O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW_ANY);guard current>=0 else{throw fileFailure("unavailable","files root path changed")};defer{close(current)}
        guard fileSame(try filePrivateDirectory(current),rootIdentity),fileSame(try filePrivateDirectory(root),rootIdentity) else{throw fileFailure("unavailable","files root identity changed")}
        var linkedLock=stat();guard fstatat(root,".veynode-lock",&linkedLock,AT_SYMLINK_NOFOLLOW)==0,fileSame(linkedLock,try fileStat(lockFD)),linkedLock.st_nlink==1 else{throw fileFailure("unavailable","files lock identity changed")}
        let currentStage=openat(root,".veynode-staging",O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW);guard currentStage>=0 else{throw fileFailure("unavailable","staging path changed")};defer{close(currentStage)}
        guard fileSame(try filePrivateDirectory(currentStage),stagingIdentity),fileSame(try filePrivateDirectory(staging),stagingIdentity) else{throw fileFailure("unavailable","staging identity changed")}
    }
    func stagingNames()throws->[String] {
        let copy=openat(staging,".",O_RDONLY|O_DIRECTORY|O_CLOEXEC);guard copy>=0 else{throw fileSystemFailure("read staging")}
        guard let dir=fdopendir(copy) else{close(copy);throw fileSystemFailure("read staging")};defer{closedir(dir)}
        var names:[String]=[]
        while true {errno=0;guard let entry=readdir(dir) else{if errno != 0{throw fileSystemFailure("read staging")};break};let name=withUnsafePointer(to:entry.pointee.d_name){$0.withMemoryRebound(to:CChar.self,capacity:1024){String(cString:$0)}};if name=="." || name==".."{continue};guard names.count<16 else{throw fileFailure("unavailable","unexpected staging capacity")};names.append(name)}
        return names
    }
    func removeStale(_ name:String)throws {
        guard name.hasPrefix(".veynode-upload-"),name.hasSuffix(".part"),fileHex(String(name.dropFirst(16).dropLast(5)),32) else{throw fileFailure("unavailable","unexpected staging entry")}
        let fd=openat(staging,name,O_RDONLY|O_CLOEXEC|O_NOFOLLOW|O_NONBLOCK);guard fd>=0 else{throw fileSystemFailure("open stale staging")};defer{close(fd)}
        let s=try fileStat(fd);guard s.st_mode&S_IFMT==S_IFREG,s.st_uid==getuid(),s.st_nlink==1,s.st_mode&0o077==0 else{throw fileFailure("forbidden","unsafe stale staging entry")};try fileNoACL(fd)
        var linked=stat();guard fstatat(staging,name,&linked,AT_SYMLINK_NOFOLLOW)==0,fileSame(s,linked),unlinkat(staging,name,0)==0 else{throw fileFailure("cleanup_failed","stale staging cleanup failed")}
    }
    func remove(_ token:String)throws {
        guard let t=transfers[token] else{return}
        if !t.temporary.isEmpty {var s=stat();if fstatat(staging,t.temporary,&s,AT_SYMLINK_NOFOLLOW)==0 {guard fileSame(try fileStat(t.fd),s),s.st_nlink==1,unlinkat(staging,t.temporary,0)==0 else{throw fileFailure("cleanup_failed","staging inode changed or unlink failed")}} else if errno != ENOENT {throw fileSystemFailure("staging unlink")}}
        transfers.removeValue(forKey:token)
    }
    func prune()throws {let time=now();closed=closed.filter{$0.value>time};for (token,t) in transfers where time>=t.touched+60_000_000_000 || time>=t.created+300_000_000_000 {try remove(token)}}
    func stop()throws {for token in Array(transfers.keys){try remove(token)}}
    func handle(_ req:[String:Any])->[String:Any] {
        let id=req["id"] as? String ?? ""
        do{return ["id":id,"result":try operation(req)]}catch let e as GuestFileError {if ["cleanup_failed","unavailable"].contains(e.code){fault=true};return errorReply(id,e.code,e.message)}catch{fault=true;return errorReply(id,"file_error","unexpected file operation failure")}
    }
    func operation(_ req:[String:Any])throws->[String:Any] {
        guard let id=req["id"] as? String,!id.isEmpty,id.utf8.count<=128,let method=req["method"] as? String else{throw fileFailure("invalid_request","invalid id or method")}
        let fields:[String:Set<String>] = ["file_worker_info":[],"file_upload_begin":["owner","path","size","sha256"],"file_upload_write":["owner","token","offset","data"],"file_upload_commit":["owner","token"],"file_download_begin":["owner","path"],"file_download_read":["owner","token","offset"],"file_cancel":["owner","token"],"file_session_end":["owner"]]
        guard let required=fields[method],Set(req.keys)==required.union(["id","method"]) else{throw fileFailure("invalid_request","unexpected or missing file fields")}
        guard !fault else{throw fileFailure("unavailable","file worker fenced; service restart required")}
        try validateRoots();try prune()
        if method=="file_worker_info" {return ["uid":getuid(),"gid":getgid(),"max_file_bytes":guestFileMaximum,"chunk_bytes":guestFileChunk,"idle_timeout_seconds":60,"absolute_timeout_seconds":300,"path_scope":"single filename within configured private files root","root":rootPath,"ownership":"same non-root Guest user; not isolation from other programs running as that user","max_transfers":16,"active_transfers":transfers.count,"staged_upload_files":try stagingNames().count]}
        guard let owner=req["owner"] as? String,fileHex(owner,64) else{throw fileFailure("invalid_request","invalid session owner")}
        if method=="file_session_end" {if closed[owner]==nil && closed.count>=256{fault=true;throw fileFailure("unavailable","closed owner capacity exhausted")};closed[owner]=now()+300_000_000_000;var count=0;for (token,t) in transfers where t.owner==owner {try remove(token);count+=1};return ["cancelled_transfers":count]}
        guard closed[owner]==nil else{throw fileFailure("forbidden","file session closed")}
        if method=="file_upload_begin" || method=="file_download_begin" {
            guard closed.count<256,transfers.count<16 else{throw fileFailure("busy","file transfer capacity reached")}
            guard let path=req["path"] as? String,fileNameValid(path) else{throw fileFailure("invalid_request","path must be a single non-reserved filename")}
            let token=UUID().uuidString.replacingOccurrences(of:"-",with:"").lowercased()
            var fd:Int32 = -1;var temporary="",size=0,digest="",kind="download",initial=stat()
            do {
                if method=="file_upload_begin" {
                    guard let declared=fileInteger(req["size"],max:guestFileMaximum),let hash=req["sha256"] as? String,fileHex(hash,64) else{throw fileFailure("invalid_request","invalid size or SHA256")};size=declared;digest=hash;kind="upload"
                    var existing=stat();if fstatat(root,path,&existing,AT_SYMLINK_NOFOLLOW)==0{throw fileFailure("already_exists","overwrite forbidden")};guard errno==ENOENT else{throw fileSystemFailure("destination check")}
                    temporary=".veynode-upload-"+token+".part";fd=openat(staging,temporary,O_CREAT|O_EXCL|O_RDWR|O_NOFOLLOW|O_CLOEXEC,0o600);guard fd>=0 else{throw fileSystemFailure("create upload")};initial=try fileStat(fd);try fileNoACL(fd)
                } else {
                    fd=openat(root,path,O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC);guard fd>=0 else{throw fileSystemFailure("open download")};initial=try fileStat(fd)
                    guard initial.st_mode&S_IFMT==S_IFREG,initial.st_uid==getuid(),initial.st_nlink==1,initial.st_size>=0,initial.st_size<=guestFileMaximum,initial.st_mode&0o077==0 else{throw fileFailure("invalid_request","download requires private owned single-link regular file of at most 8MiB")};try fileNoACL(fd);size=Int(initial.st_size)
                    var hash=SHA256(),remaining=size;while remaining>0{let data=try fileRead(fd,min(guestFileChunk,remaining));hash.update(data:data);remaining-=data.count};digest=hash.finalize().map{String(format:"%02x",$0)}.joined()
                    guard fileUnchanged(initial,try fileStat(fd)),lseek(fd,0,SEEK_SET)==0 else{throw fileFailure("changed","download changed during hashing")}
                }
            }catch{if fd>=0{close(fd)};if !temporary.isEmpty && unlinkat(staging,temporary,0) != 0 && errno != ENOENT{fault=true};throw error}
            transfers[token]=GuestTransfer(owner:owner,kind:kind,path:path,temporary:temporary,size:size,digest:digest,fd:fd,now:now(),initial:initial)
            return ["token":token,"size":size,"sha256":digest,"chunk_bytes":guestFileChunk,"idle_timeout_seconds":60]
        }
        guard let token=req["token"] as? String,fileHex(token,32) else{throw fileFailure("invalid_request","invalid transfer token")}
        guard let t=transfers[token],t.owner==owner else{throw fileFailure("not_found","transfer unknown, expired or not owned by session")};t.touched=now()
        if method=="file_cancel" {try remove(token);return ["cancelled":true]}
        if method=="file_upload_write",t.kind=="upload" {
            guard let encoded=req["data"] as? String,encoded.utf8.count<=43692,let data=Data(base64Encoded:encoded),data.base64EncodedString()==encoded,!data.isEmpty,data.count<=guestFileChunk,fileInteger(req["offset"],max:guestFileMaximum)==t.offset,data.count<=t.size-t.offset else{throw fileFailure("invalid_request","invalid base64 chunk, size or sequential offset")}
            do{try fileWriteAll(t.fd,data)}catch{try remove(token);throw error};t.hash.update(data:data);t.offset+=data.count;return ["offset":t.offset]
        }
        if method=="file_upload_commit",t.kind=="upload" {
            guard t.offset==t.size,t.hash.finalize().map({String(format:"%02x",$0)}).joined()==t.digest else{try remove(token);throw fileFailure("integrity_error","upload length or SHA256 differs; staging removed")}
            var linked=stat();guard fstatat(staging,t.temporary,&linked,AT_SYMLINK_NOFOLLOW)==0,fileSame(linked,try fileStat(t.fd)),linked.st_nlink==1 else{throw fileFailure("cleanup_failed","upload staging identity changed")}
            guard fsync(t.fd)==0 else{let e=fileSystemFailure("sync upload");try remove(token);throw e}
            guard renameatx_np(staging,t.temporary,root,t.path,UInt32(RENAME_EXCL))==0 else{let e=fileSystemFailure("publish without replacement");try remove(token);throw e}
            transfers.removeValue(forKey:token)
            guard fsync(root)==0,fsync(staging)==0 else{throw fileFailure("durability_unknown","file published but directory sync failed; inspect before retrying")}
            return ["published":true,"size":t.size,"sha256":t.digest]
        }
        if method=="file_download_read",t.kind=="download" {
            guard fileInteger(req["offset"],max:guestFileMaximum)==t.offset else{throw fileFailure("invalid_request","download offset must be sequential")}
            guard fileUnchanged(t.initial,try fileStat(t.fd)) else{try remove(token);throw fileFailure("changed","source changed during transfer")}
            let data:Data;do{data=try fileRead(t.fd,min(guestFileChunk,t.size-t.offset))}catch{try remove(token);throw error};t.offset+=data.count;t.hash.update(data:data);let eof=t.offset==t.size
            if eof {guard fileUnchanged(t.initial,try fileStat(t.fd)),t.hash.finalize().map({String(format:"%02x",$0)}).joined()==t.digest else{try remove(token);throw fileFailure("changed","download changed; discard partial data")};try remove(token)}
            return ["data":data.base64EncodedString(),"offset":t.offset,"eof":eof,"sha256":t.digest]
        }
        throw fileFailure("invalid_request","method does not match transfer kind")
    }
}

func fileWireWrite(_ fd:Int32,_ value:[String:Any],deadline:UInt64)throws {
    var data=try JSONSerialization.data(withJSONObject:value,options:[.sortedKeys,.withoutEscapingSlashes])
    guard data.count<65536 else{throw fileFailure("invalid_request","file protocol line exceeds 64KiB")};data.append(10)
    try data.withUnsafeBytes{bytes in var offset=0;while offset<bytes.count {
        guard DispatchTime.now().uptimeNanoseconds<deadline else{throw fileFailure("timeout","file worker write deadline")}
        var p=pollfd(fd:fd,events:Int16(POLLOUT),revents:0);let ready=poll(&p,1,20);if ready<0 && errno==EINTR{continue};guard ready>=0 else{throw fileSystemFailure("poll file worker")};if ready==0{continue}
        let n=write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset);if n<0 && (errno==EINTR || errno==EAGAIN){continue};guard n>0 else{throw fileFailure("unavailable","file worker pipe closed")};offset+=n
    }}
}
func fileWireRead(_ fd:Int32,deadline:UInt64?,tick:(()->Void)?=nil)throws->[String:Any]? {
    var data=Data(),buffer=[UInt8](repeating:0,count:4096),effectiveDeadline=deadline
    while effectiveDeadline == nil || DispatchTime.now().uptimeNanoseconds<effectiveDeadline! {
        tick?();var p=pollfd(fd:fd,events:Int16(POLLIN),revents:0);let ready=poll(&p,1,100);if ready<0 && errno==EINTR{continue};guard ready>=0 else{throw fileSystemFailure("poll file worker")};if ready==0{continue}
        let n=read(fd,&buffer,buffer.count);if n<0 && (errno==EINTR || errno==EAGAIN){continue};if n==0 && data.isEmpty{return nil};guard n>0 else{throw fileFailure("unavailable","file worker response incomplete")}
        if effectiveDeadline==nil{effectiveDeadline=DispatchTime.now().uptimeNanoseconds+5_000_000_000}
        if let newline=buffer.prefix(n).firstIndex(of:10) {guard newline==n-1,data.count+newline<65536 else{throw fileFailure("invalid_request","oversized or pipelined file protocol")};data.append(contentsOf:buffer[..<newline]);guard let value=try JSONSerialization.jsonObject(with:data) as? [String:Any] else{throw fileFailure("invalid_request","expected file JSON object")};return value}
        data.append(contentsOf:buffer.prefix(n));guard data.count<65536 else{throw fileFailure("invalid_request","file protocol line too large")}
    }
    throw fileFailure("timeout","file worker response deadline")
}
func runGuestFileWorker(_ root:String)->Int32 {
    // Internal fixed implementation: no arbitrary argv, no network listener, no TCC.
    signal(SIGPIPE,SIG_IGN)
    do {
        let manager=try GuestFiles(root)
        for fd in [STDIN_FILENO,STDOUT_FILENO]{guard fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)|O_NONBLOCK)==0 else{throw fileSystemFailure("worker pipe flags")}}
        while true {
            let next=try fileWireRead(STDIN_FILENO,deadline:nil,tick:{do{try manager.prune()}catch{manager.fault=true}})
            guard let request=next else{try manager.stop();return manager.fault ? 1:0}
            try fileWireWrite(STDOUT_FILENO,manager.handle(request),deadline:DispatchTime.now().uptimeNanoseconds+2_000_000_000)
        }
    }catch{fputs("file worker failed; no file content logged\n",stderr);return 1}
}
final class GuestFileBridge {
    private let gate=NSLock()
    private(set) var pid:pid_t=0
    private var input:Int32 = -1,output:Int32 = -1
    private var dead=false,reaped=false,lostOwnership=false,cleanupConfirmed=false,ownedChild=false
    init(binary:String,root:String)throws {
        var toChild=[Int32](repeating:-1,count:2),fromChild=[Int32](repeating:-1,count:2)
        guard pipe(&toChild)==0 else{throw fileSystemFailure("worker pipe")}
        guard pipe(&fromChild)==0 else{close(toChild[0]);close(toChild[1]);throw fileSystemFailure("worker pipe")}
        defer{for fd in toChild+fromChild where fd>=0{close(fd)}}
        for fd in toChild+fromChild {guard fcntl(fd,F_SETFD,FD_CLOEXEC)==0 else{throw fileSystemFailure("worker CLOEXEC")}}
        var actions:posix_spawn_file_actions_t?,attributes:posix_spawnattr_t?
        guard posix_spawn_file_actions_init(&actions)==0 else{throw fileFailure("unavailable","worker spawn actions")};defer{posix_spawn_file_actions_destroy(&actions)}
        guard posix_spawnattr_init(&attributes)==0 else{throw fileFailure("unavailable","worker spawn attributes")};defer{posix_spawnattr_destroy(&attributes)}
        var defaults=sigset_t(),mask=sigset_t();sigemptyset(&defaults);sigemptyset(&mask);for number in [SIGTERM,SIGINT,SIGPIPE]{sigaddset(&defaults,number)}
        guard posix_spawnattr_setsigdefault(&attributes,&defaults)==0,posix_spawnattr_setsigmask(&attributes,&mask)==0,posix_spawnattr_setflags(&attributes,Int16(POSIX_SPAWN_CLOEXEC_DEFAULT|POSIX_SPAWN_SETSIGDEF|POSIX_SPAWN_SETSIGMASK))==0,
              posix_spawn_file_actions_adddup2(&actions,toChild[0],STDIN_FILENO)==0,posix_spawn_file_actions_adddup2(&actions,fromChild[1],STDOUT_FILENO)==0,posix_spawn_file_actions_addopen(&actions,STDERR_FILENO,"/dev/null",O_WRONLY,0)==0 else{throw fileFailure("unavailable","worker spawn descriptor setup")}
        let argvStrings:[String]=[binary,"--file-worker","--files-root",root]
        let argv:[UnsafeMutablePointer<CChar>?]=argvStrings.map{strdup($0)}+[nil]
        let envStrings:[String]=["PATH=/usr/bin:/bin","LANG=en_US.UTF-8","HOME=\(NSHomeDirectory())"]
        let env:[UnsafeMutablePointer<CChar>?]=envStrings.map{strdup($0)}+[nil]
        defer{for p in argv+env{free(p)}}
        let code=argv.withUnsafeBufferPointer{a in env.withUnsafeBufferPointer{e in posix_spawn(&pid,binary,&actions,&attributes,a.baseAddress!,e.baseAddress!)}}
        guard code==0,pid>0 else{throw fileFailure("unavailable","worker spawn failed (\(code))")}
        ownedChild=true
        input=toChild[1];toChild[1] = -1;output=fromChild[0];fromChild[0] = -1
        // Close the parent's copies before handshake; otherwise EOF is masked.
        close(toChild[0]);toChild[0] = -1;close(fromChild[1]);fromChild[1] = -1
        for fd in [input,output]{let flags=fcntl(fd,F_GETFL);guard flags>=0,fcntl(fd,F_SETFL,flags|O_NONBLOCK)==0 else{_ = terminateLocked();throw fileFailure("unavailable","worker pipe nonblocking setup failed")}}
        let reply=call(["id":"files-startup","method":"file_worker_info"])
        guard reply["result"] != nil else{_ = stop();throw fileFailure("unavailable","file worker startup refused private root")}
    }
    deinit{_ = stop()}
    private func reapUntil(_ deadline:UInt64)->Int32? {
        guard ownedChild,pid>0,!lostOwnership else{return nil}
        while DispatchTime.now().uptimeNanoseconds<deadline {var status:Int32=0;let result=waitpid(pid,&status,WNOHANG);if result==pid{reaped=true;return status};if result<0 && errno != EINTR{dead=true;lostOwnership=true;return nil};usleep(1000)};return nil
    }
    private func terminateLocked()->Bool {
        guard ownedChild,pid>0 else{return false}
        if input>=0{close(input);input = -1};if output>=0{close(output);output = -1}
        if reaped{return true}
        // This trusted worker cannot spawn commands. The unreaped child PID is
        // retained until the last signal, so no stale PID is ever signalled.
        if reapUntil(DispatchTime.now().uptimeNanoseconds+500_000_000) != nil{return true}
        guard !lostOwnership else{return false}
        _ = kill(pid,SIGKILL)
        return reapUntil(DispatchTime.now().uptimeNanoseconds+500_000_000) != nil
    }
    func stop()->Bool {
        guard ownedChild,pid>0 else{return true}
        gate.lock();defer{gate.unlock()};let wasDead=dead;dead=true
        if reaped{return cleanupConfirmed}
        if input>=0{close(input);input = -1}
        if let status=reapUntil(DispatchTime.now().uptimeNanoseconds+500_000_000){if output>=0{close(output);output = -1};cleanupConfirmed = status==0 && !wasDead;return cleanupConfirmed}
        _ = terminateLocked();return false // forced exit is not staging cleanup proof
    }
    func call(_ request:[String:Any])->[String:Any] {
        let id=request["id"] as? String ?? ""
        guard gate.try() else{return errorReply(id,"busy","file worker request already active")};defer{gate.unlock()}
        guard !dead else{return errorReply(id,"unavailable","file worker fenced; restart service")}
        do {let deadline=DispatchTime.now().uptimeNanoseconds+5_000_000_000;try fileWireWrite(input,request,deadline:deadline);guard let reply=try fileWireRead(output,deadline:deadline),reply["id"] as? String==id,Set(reply.keys)==Set(["id",reply["result"] != nil ? "result":"error"]) else{throw fileFailure("unavailable","file worker invalid response")};return reply}
        catch {dead=true;_ = terminateLocked();return errorReply(id,(error as? GuestFileError)?.code ?? "unavailable","file worker failed or timed out; fenced until service restart")}
    }
}
var guestFiles:GuestFileBridge?
