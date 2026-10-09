import Foundation
import CryptoKit
import CoreGraphics
import Darwin

// Local installation/foreground launch only. No launchd, TCP, input, capture,
// permission request, process discovery or signalling of existing services.
struct LaunchFailure:Error {let code:String}
func fail(_ code:String)throws->Never {throw LaunchFailure(code:code)}
let names=["launcher","integration","capture","manifest.json"]
let maximumBinaryBytes=64*1024*1024
func digest(_ data:Data)->String {SHA256.hash(data:data).map{String(format:"%02x",$0)}.joined()}
func emit(_ value:[String:Any]) {if let data=try? JSONSerialization.data(withJSONObject:value,options:[.sortedKeys,.withoutEscapingSlashes]) {FileHandle.standardOutput.write(data);FileHandle.standardOutput.write(Data([10]))}}
func noExtendedACL(_ fd:Int32)throws {
    guard let security=filesec_init() else{try fail("acl_read_failed")};defer{filesec_free(security)}
    var metadata=stat(),present:Int32=0
    guard fstatx_np(fd,&metadata,security)==0,filesec_query_property(security,FILESEC_ACL,&present)==0 else {try fail("acl_read_failed")}
    guard present==0 else {try fail("extended_acl_not_supported")}
}
func ownedDirectory(_ fd:Int32,privateMode:Bool)throws {
    var s=stat()
    guard fstat(fd,&s)==0,s.st_mode&S_IFMT==S_IFDIR,s.st_uid==getuid(),s.st_mode&0o022==0,
          !privateMode || s.st_mode&0o077==0 else {try fail("directory_owner_or_mode")}
    if privateMode {try noExtendedACL(fd)}
}
func openDirectory(_ path:String,privateMode:Bool=true)throws->Int32 {
    guard path.hasPrefix("/"),!path.utf8.contains(0),path.utf8.count<4096 else {try fail("absolute_path_required")}
    let components=path.split(separator:"/",omittingEmptySubsequences:true)
    guard !components.isEmpty,components.allSatisfy({$0 != "." && $0 != ".."}) else {try fail("unsafe_path")}
    var current=open("/",O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)
    guard current>=0 else {try fail("directory_open_failed")}
    do {
        for component in components {
            let next=openat(current,String(component),O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)
            guard next>=0 else {try fail("directory_symlink_or_unavailable")}
            close(current);current=next
            var s=stat()
            guard fstat(current,&s)==0,(s.st_uid==0 || s.st_uid==getuid()),s.st_mode&0o022==0 else {try fail("unsafe_parent_directory")}
        }
        try ownedDirectory(current,privateMode:privateMode)
        return current
    } catch {close(current);throw error}
}
func readFile(_ directory:Int32,_ name:String,maximum:Int)throws->Data {
    let fd=openat(directory,name,O_RDONLY|O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK)
    guard fd>=0 else {try fail("bundle_file_unavailable_or_symlink")}
    defer {close(fd)}
    try noExtendedACL(fd)
    let quarantine=fgetxattr(fd,"com.apple.quarantine",nil,0,0,0)
    if quarantine>=0 {try fail("quarantined_bundle_file_requires_separate_review")}
    guard errno==ENOATTR || errno==ENOTSUP else {try fail("bundle_file_attributes_unavailable")}
    var before=stat()
    guard fstat(fd,&before)==0,before.st_mode&S_IFMT==S_IFREG,before.st_nlink==1,before.st_uid==getuid(),
          before.st_mode&0o022==0,before.st_size>0,before.st_size<=maximum else {try fail("bundle_file_not_private_regular")}
    var output=Data(),buffer=[UInt8](repeating:0,count:65536)
    while true {
        let count=read(fd,&buffer,buffer.count)
        if count<0 && errno==EINTR {continue}
        guard count>=0 else {try fail("bundle_file_read_failed")}
        if count==0 {break}
        guard output.count+count<=maximum else {try fail("bundle_file_too_large")}
        output.append(contentsOf:buffer.prefix(count))
    }
    var after=stat()
    guard fstat(fd,&after)==0,before.st_dev==after.st_dev,before.st_ino==after.st_ino,before.st_size==after.st_size,
          before.st_mtimespec.tv_sec==after.st_mtimespec.tv_sec,before.st_mtimespec.tv_nsec==after.st_mtimespec.tv_nsec,
          output.count==Int(after.st_size) else {try fail("bundle_file_changed")}
    return output
}
struct VerifiedBundle {let files:[String:Data];let hashes:[String:String]}
func verifyBundle(_ directory:Int32)throws->VerifiedBundle {
    let manifest=try readFile(directory,"manifest.json",maximum:4096)
    guard let root=try JSONSerialization.jsonObject(with:manifest) as? [String:Any],Set(root.keys)==Set(["version","files"]),
          let version=root["version"] as? NSNumber,CFGetTypeID(version) != CFBooleanGetTypeID(),version.intValue==1,version.doubleValue==1,
          let entries=root["files"] as? [String:[String:Any]],Set(entries.keys)==Set(["launcher","integration","capture"]) else {try fail("invalid_manifest")}
    var files=["manifest.json":manifest],hashes:[String:String]=[:]
    for name in ["launcher","integration","capture"] {
        guard let entry=entries[name],Set(entry.keys)==Set(["sha256","bytes"]),let hash=entry["sha256"] as? String,
              hash.utf8.count==64,hash.utf8.allSatisfy({(48...57).contains($0)||(97...102).contains($0)}),
              let size=entry["bytes"] as? NSNumber,CFGetTypeID(size) != CFBooleanGetTypeID(),size.doubleValue.rounded()==size.doubleValue,
              size.intValue>0,size.intValue<=maximumBinaryBytes else {try fail("invalid_manifest")}
        let bytes=try readFile(directory,name,maximum:maximumBinaryBytes)
        guard bytes.count==size.intValue,digest(bytes)==hash else {try fail("binary_hash_mismatch")}
        var mode=stat()
        guard fstatat(directory,name,&mode,AT_SYMLINK_NOFOLLOW)==0,mode.st_mode&0o100 != 0 else {try fail("binary_not_executable")}
        files[name]=bytes;hashes[name]=hash
    }
    return VerifiedBundle(files:files,hashes:hashes)
}
func writeFile(_ directory:Int32,_ name:String,_ data:Data,_ mode:mode_t)throws {
    let fd=openat(directory,name,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,mode)
    guard fd>=0 else {try fail("install_file_create_failed")}
    defer{close(fd)}
    try data.withUnsafeBytes {bytes in
        var offset=0
        while offset<bytes.count {let n=write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset);if n<0 && errno==EINTR {continue};guard n>0 else {try fail("install_file_write_failed")};offset+=n}
    }
    guard fchmod(fd,mode)==0,fsync(fd)==0 else {try fail("install_file_sync_failed")}
}
func install(_ source:Int32,_ destination:String)throws->[String:Any] {
    let bundle=try verifyBundle(source)
    guard destination.hasPrefix("/"),!destination.hasSuffix("/"),!destination.utf8.contains(0) else {try fail("absolute_destination_required")}
    let rawComponents=destination.split(separator:"/",omittingEmptySubsequences:false).dropFirst()
    guard rawComponents.allSatisfy({!$0.isEmpty && $0 != "." && $0 != ".."}) else {try fail("unsafe_destination_path")}
    let path=URL(fileURLWithPath:destination),name=path.lastPathComponent
    guard !name.isEmpty,name != ".",name != "..",name.utf8.count<=200 else {try fail("unsafe_destination_name")}
    let parent=try openDirectory(path.deletingLastPathComponent().path);defer{close(parent)}
    var existing=stat()
    guard fstatat(parent,name,&existing,AT_SYMLINK_NOFOLLOW) != 0,errno==ENOENT else {try fail("destination_exists")}
    let temporary=".install-"+UUID().uuidString.lowercased()
    guard mkdirat(parent,temporary,0o700)==0 else {try fail("install_directory_failed")}
    let stage=openat(parent,temporary,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)
    guard stage>=0 else {unlinkat(parent,temporary,AT_REMOVEDIR);try fail("install_directory_failed")}
    var published=false
    defer {if !published {for item in names {unlinkat(stage,item,0)};unlinkat(parent,temporary,AT_REMOVEDIR)};close(stage)}
    for item in names {try writeFile(stage,item,bundle.files[item]!,item=="manifest.json" ? 0o600:0o700)}
    let checked=try verifyBundle(stage)
    guard checked.hashes==bundle.hashes,fsync(stage)==0 else {try fail("install_verify_or_sync_failed")}
    guard renameatx_np(parent,temporary,parent,name,UInt32(RENAME_EXCL))==0 else {try fail("install_publish_failed")}
    published=true
    // Never unlink a successfully published installation after a durability error.
    guard fsync(parent)==0 else {try fail("installation_published_durability_unconfirmed")}
    return ["installed":true,"hashes":checked.hashes,"service_started":false,"launchd_installed":false]
}
func machineModel()->String {var size=0;guard sysctlbyname("hw.model",nil,&size,nil,0)==0 else{return "unknown"};var bytes=[CChar](repeating:0,count:size);guard sysctlbyname("hw.model",&bytes,&size,nil,0)==0 else{return "unknown"};return String(cString:bytes)}
struct LaunchOptions {let role:String;let input:Bool;let filesRoot:String?}
func parseOptions(_ args:[String])throws->LaunchOptions {
    guard let role=args.first,["integration","capture"].contains(role) else {try fail("invalid_role")}
    var input=false,files:String?,index=1
    while index<args.count {
        if args[index]=="--input",role=="integration",!input {input=true;index+=1}
        else if args[index]=="--files-root",role=="integration",files==nil,index+1<args.count {files=args[index+1];index+=2}
        else {try fail("invalid_launch_options")}
    }
    if let files {let fd=try openDirectory(files);close(fd)}
    return LaunchOptions(role:role,input:input,filesRoot:files)
}
func preflight(_ options:LaunchOptions)throws->[String:Any] {
    guard getuid() != 0,machineModel().hasPrefix("VirtualMac") else {try fail("nonroot_virtual_mac_guest_required")}
    guard CGPreflightScreenCaptureAccess() else {try fail("screen_recording_permission_required")}
    if options.input && !CGPreflightPostEventAccess() {try fail("accessibility_permission_required")}
    return ["preflight_passed":true,"role":options.role,"screen_recording":"granted","accessibility":options.input ? "granted":"not_requested",
            "ready":false,"scope":"preflight only; actual service must bind its Guest vsock listener","permission_requested":false]
}
func acquireLock(_ directory:Int32,_ role:String)throws->Int32 {
    let fd=openat(directory,role+".lock",O_RDWR|O_CREAT|O_NOFOLLOW|O_NONBLOCK,0o600)
    guard fd>=0 else {try fail("service_lock_unavailable")}
    var s=stat()
    guard fstat(fd,&s)==0,s.st_mode&S_IFMT==S_IFREG,s.st_uid==getuid(),s.st_nlink==1,s.st_mode&0o077==0 else {close(fd);try fail("unsafe_service_lock")}
    do {try noExtendedACL(fd)} catch {close(fd);throw error}
    guard flock(fd,LOCK_EX|LOCK_NB)==0 else {close(fd);try fail("service_already_started_or_lock_busy")}
    // The actual exec'ed service owns the same flock description. No PID file
    // or unlocked PID-reuse inference, and no supervisor that can release early.
    guard fcntl(fd,F_SETFD,0)==0 else {close(fd);try fail("service_lock_inheritance_failed")}
    return fd
}
func lockDirectory()throws->Int32 {
    let home=try openDirectory(NSHomeDirectory(),privateMode:false);defer{close(home)}
    let name=".veynode-services"
    if mkdirat(home,name,0o700) != 0 && errno != EEXIST {try fail("service_lock_directory_failed")}
    let fd=openat(home,name,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)
    guard fd>=0 else {try fail("service_lock_directory_failed")}
    do {try ownedDirectory(fd,privateMode:true);return fd} catch {close(fd);throw error}
}
func launch(_ path:String,_ bundle:VerifiedBundle,_ options:LaunchOptions)throws->Never {
    _=try preflight(options)
    let directory=try lockDirectory();defer{close(directory)}
    let lock=try acquireLock(directory,options.role)
    var args=[path+"/"+options.role]
    if options.role=="capture" {args.append("--serve")}
    else {args.append("--allow-observe");if options.input {args.append("--allow-input")};if let files=options.filesRoot {args += ["--allow-files","--files-root",files]}}
    let argv:[UnsafeMutablePointer<CChar>?]=args.map{strdup($0)}+[nil]
    let environmentStrings:[String]=["HOME=\(NSHomeDirectory())","USER=\(NSUserName())","PATH=/usr/bin:/bin:/usr/sbin:/sbin","LANG=en_US.UTF-8"]
    let env:[UnsafeMutablePointer<CChar>?]=environmentStrings.map{strdup($0)}+[nil]
    defer{close(lock);for p in argv+env {free(p)}}
    // This event attests verified bytes/preflight and an attempted exec, not
    // vsock readiness or permission inheritance after moving the code identity.
    emit(["event":"exec_attempt","role":options.role,"sha256":bundle.hashes[options.role]!,"ready":false,"lock_inherited":true])
    _ = argv.withUnsafeBufferPointer{a in env.withUnsafeBufferPointer{e in execve(args[0],a.baseAddress,e.baseAddress)}}
    try fail("service_exec_failed")
}

func lockSelfTest(_ parentPath:String)throws->[String:Any] {
    let parent=try openDirectory(parentPath);defer{close(parent)}
    let name=".launcher-test-"+UUID().uuidString.lowercased()
    guard mkdirat(parent,name,0o700)==0 else {try fail("test_directory_failed")}
    let directory=openat(parent,name,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)
    guard directory>=0 else {unlinkat(parent,name,AT_REMOVEDIR);try fail("test_directory_failed")}
    defer {for item in ["test.lock","fifo.lock","link.lock"] {unlinkat(directory,item,0)};close(directory);unlinkat(parent,name,AT_REMOVEDIR)}
    guard mkfifoat(directory,"fifo.lock",0o600)==0,symlinkat("test.lock",directory,"link.lock")==0 else {try fail("test_special_files_failed")}
    for role in ["fifo","link"] {
        do {let unexpected=try acquireLock(directory,role);close(unexpected);try fail("test_special_lock_accepted")}
        catch let error as LaunchFailure {guard ["unsafe_service_lock","service_lock_unavailable"].contains(error.code) else {throw error}}
    }
    var lock=try acquireLock(directory,"test")
    defer {if lock>=0 {close(lock)}}
    func requireBusy()throws {
        do {let unexpected=try acquireLock(directory,"test");close(unexpected);try fail("test_expected_busy")}
        catch let e as LaunchFailure {guard e.code=="service_already_started_or_lock_busy" else {throw e}}
    }
    try requireBusy()
    var pid:pid_t=0
    let argv=[strdup("/bin/sleep"),strdup("1"),nil]
    defer {for item in argv {free(item)}}
    let environment:[UnsafeMutablePointer<CChar>?]=[nil]
    let result=argv.withUnsafeBufferPointer{a in environment.withUnsafeBufferPointer{e in posix_spawn(&pid,"/bin/sleep",nil,nil,a.baseAddress!,e.baseAddress!)}}
    guard result==0,pid>0 else {try fail("test_child_spawn_failed")}
    var reaped=false
    defer {if !reaped {kill(pid,SIGKILL);var status:Int32=0;while waitpid(pid,&status,0)<0 && errno==EINTR {}}}
    close(lock);lock = -1
    try requireBusy() // Parent no longer owns the description; exec'ed child does.
    var status:Int32=0
    var waited:pid_t
    repeat {waited=waitpid(pid,&status,0)} while waited<0 && errno==EINTR
    guard waited==pid else {try fail("test_child_reap_failed")}
    reaped=true
    guard status==0 else {try fail("test_child_failed")}
    let reclaimed=try acquireLock(directory,"test");close(reclaimed)
    return ["passed":true,"scope":"owned host temporary filesystem and /bin/sleep only; no Guest or TCC",
            "duplicate_lock_rejected":true,"exec_child_retains_lock_after_parent_close":true,"lock_available_after_exact_child_reap":true]
}

do {
    let arguments=Array(CommandLine.arguments.dropFirst())
    let executable=URL(fileURLWithPath:CommandLine.arguments[0]).standardizedFileURL.path
    let path=URL(fileURLWithPath:executable).deletingLastPathComponent().path
    let directory=try openDirectory(path);defer{close(directory)}
    if arguments.count==3,arguments[0]=="self-test",arguments[1]=="--directory" {emit(try lockSelfTest(arguments[2]))}
    else if arguments==["verify"] {let value=try verifyBundle(directory);emit(["verified":true,"hashes":value.hashes,"service_started":false,"permission_checked":false])}
    else if arguments.count==3,arguments[0]=="install",arguments[1]=="--destination" {emit(try install(directory,arguments[2]))}
    else if let action=arguments.first,["preflight","start"].contains(action) {
        let bundle=try verifyBundle(directory),options=try parseOptions(Array(arguments.dropFirst()))
        if action=="preflight" {emit(try preflight(options))}else{try launch(path,bundle,options)}
    } else {try fail("invalid_arguments")}
} catch let error as LaunchFailure {emit(["error":error.code,"ready":false]);exit(1)}
catch {emit(["error":"launcher_failed","ready":false]);exit(1)}
