// Linux-only Apple saved state support. A capability query is not a restore acceptance.
// All paths below are private daemon-owned artifacts; they are never public MCP arguments.
import Foundation
import AppKit
import Virtualization
import CryptoKit
import CoreFoundation
import Darwin

struct LinuxSnapshotRestore {
    let path: String
    let sha256: String
    let configurationSHA256: String
    let captureID: String
}
func linuxSnapshotHex(_ value: String, bytes: Int) -> Bool {
    value.utf8.count == bytes * 2 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
}
private func snapshotError(_ value: String) -> NativeError { NativeError(message:value) }
private func snapshotDigest(_ bytes: Data) -> String { SHA256.hash(data:bytes).map { String(format:"%02x",$0) }.joined() }
private func snapshotJSON(_ object: [String:Any]) throws -> Data {
    try JSONSerialization.data(withJSONObject:object,options:[.sortedKeys,.withoutEscapingSlashes])
}
private func snapshotStat(_ fd: Int32) throws -> stat {
    var value = stat()
    guard fstat(fd,&value) == 0 else { throw snapshotError("Cannot inspect owned snapshot descriptor") }
    return value
}
private func snapshotSame(_ a: stat, _ b: stat) -> Bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino && a.st_size == b.st_size &&
    a.st_mtimespec.tv_sec == b.st_mtimespec.tv_sec && a.st_mtimespec.tv_nsec == b.st_mtimespec.tv_nsec &&
    a.st_ctimespec.tv_sec == b.st_ctimespec.tv_sec && a.st_ctimespec.tv_nsec == b.st_ctimespec.tv_nsec
}
private func snapshotNoExtendedACL(_ descriptor: Int32) throws {
    errno = 0
    if let acl = acl_get_fd_np(descriptor,ACL_TYPE_EXTENDED) {
        defer { acl_free(UnsafeMutableRawPointer(acl)) }
        throw snapshotError("Snapshot privacy forbids extended ACLs")
    }
    guard errno == ENOENT else { throw snapshotError("Cannot verify snapshot descriptor ACL privacy") }
}
private func snapshotPrivate(_ value: stat, directory: Bool, descriptor: Int32) throws {
    guard value.st_uid == geteuid(), value.st_mode & 0o077 == 0,
          value.st_mode & mode_t(S_IFMT) == mode_t(directory ? S_IFDIR : S_IFREG),
          directory || value.st_nlink == 1 else { throw snapshotError("Snapshot artifact must be private, owned and free of aliases") }
    try snapshotNoExtendedACL(descriptor)
}

/// Each traversal rejects links, dot components and non-directory edges. Retain
/// the final FD and re-open the complete path before every pathname-only VZ call.
final class LinuxSnapshotDirectory {
    let path: String
    let descriptor: Int32
    init(_ path: String, createLeaf: Bool = false) throws {
        guard path.hasPrefix("/"), !path.utf8.contains(0), path.count <= 4096 else { throw snapshotError("Invalid absolute snapshot directory") }
        let parts = path.split(separator:"/",omittingEmptySubsequences:false)
        guard parts.first == "", parts.count > 1,
              parts.dropFirst().allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else { throw snapshotError("Snapshot directory must be normalized") }
        var current = open("/",O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard current >= 0 else { throw snapshotError("Cannot open snapshot ancestry") }
        do {
            for (offset, component) in parts.dropFirst().enumerated() {
                let leaf = String(component)
                if createLeaf && offset == parts.count - 2 {
                    if mkdirat(current,leaf,0o700) != 0 && errno != EEXIST { throw snapshotError("Cannot create private machine identity directory") }
                }
                let next = openat(current,leaf,O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                guard next >= 0 else { throw snapshotError("Snapshot ancestry missing, changed or symbolic link") }
                close(current); current = next
            }
            try snapshotPrivate(snapshotStat(current),directory:true,descriptor:current)
        } catch { close(current); throw error }
        self.path = path; descriptor = current
    }
    deinit { close(descriptor) }
    func verify() throws {
        let current = try LinuxSnapshotDirectory(path)
        let a = try snapshotStat(descriptor), b = try snapshotStat(current.descriptor)
        try snapshotPrivate(a,directory:true,descriptor:descriptor)
        guard a.st_dev == b.st_dev, a.st_ino == b.st_ino else { throw snapshotError("Snapshot directory path was replaced") }
    }
    func absent(_ leaf: String) throws {
        try verify()
        var value = stat()
        guard fstatat(descriptor,leaf,&value,AT_SYMLINK_NOFOLLOW) != 0, errno == ENOENT else { throw snapshotError("Refusing existing snapshot artifact") }
    }
    func read(_ leaf: String, maximum: Int) throws -> Data {
        let file = try LinuxSnapshotFile(directory:self,leaf:leaf)
        let metadata = try snapshotStat(file.descriptor)
        guard metadata.st_size >= 0, metadata.st_size <= maximum else { throw snapshotError("Snapshot metadata exceeds bound") }
        var data = Data(count:Int(metadata.st_size))
        let count = data.withUnsafeMutableBytes { raw in pread(file.descriptor,raw.baseAddress,raw.count,0) }
        guard count == data.count else { throw snapshotError("Cannot read complete snapshot metadata") }
        try file.verify(); return data
    }
    func create(_ leaf: String, data: Data) throws {
        try absent(leaf)
        let fd = openat(descriptor,leaf,O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,0o600)
        guard fd >= 0 else { throw snapshotError("Cannot exclusively create machine identity") }
        defer { close(fd) }
        try snapshotPrivate(snapshotStat(fd),directory:false,descriptor:fd)
        try data.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let n = write(fd,raw.baseAddress!.advanced(by:offset),raw.count-offset)
                if n < 0 && errno == EINTR { continue }
                guard n > 0 else { throw snapshotError("Cannot complete machine identity write; incomplete file retained") }
                offset += n
            }
        }
        guard fsync(fd) == 0, fsync(descriptor) == 0 else { throw snapshotError("Machine identity durability unconfirmed; file retained") }
        try verify()
    }
    func freeSpace(required: UInt64) throws {
        var value = statvfs()
        guard fstatvfs(descriptor,&value) == 0 else { throw snapshotError("Cannot query snapshot free space") }
        let free = UInt64(value.f_bavail).multipliedReportingOverflow(by:UInt64(value.f_frsize))
        guard !free.overflow, free.partialValue >= required else { throw snapshotError("Insufficient snapshot disk space") }
    }
}
final class LinuxSnapshotFile {
    let directory: LinuxSnapshotDirectory
    let leaf: String
    let descriptor: Int32
    private let initial: stat
    init(directory: LinuxSnapshotDirectory, leaf: String) throws {
        guard !leaf.isEmpty, leaf != ".", leaf != "..", !leaf.contains("/"), !leaf.utf8.contains(0) else { throw snapshotError("Invalid snapshot leaf") }
        try directory.verify()
        let fd = openat(directory.descriptor,leaf,O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard fd >= 0 else { throw snapshotError("Cannot open snapshot artifact") }
        do { initial = try snapshotStat(fd); try snapshotPrivate(initial,directory:false,descriptor:fd) }
        catch { close(fd); throw error }
        descriptor = fd; self.directory = directory; self.leaf = leaf
    }
    func verify() throws {
        try directory.verify()
        try snapshotPrivate(snapshotStat(descriptor),directory:false,descriptor:descriptor)
        var edge = stat()
        guard fstatat(directory.descriptor,leaf,&edge,AT_SYMLINK_NOFOLLOW) == 0,
              snapshotSame(initial,edge), snapshotSame(initial,try snapshotStat(descriptor)) else { throw snapshotError("Snapshot artifact changed during operation") }
    }
    func digest() throws -> (UInt64,String) { try verify(); let result = try linuxSnapshotHash(descriptor); try verify(); return result }
    var path: String { directory.path + "/" + leaf }
    deinit { close(descriptor) }
}
/// The actual identity-lock open path is shared with the no-VM storage tests.
/// Never normalize an existing lock's ACL: reject and retain it for its owner.
func linuxSnapshotIdentityLock(_ directory: LinuxSnapshotDirectory, restoring: Bool) throws -> Int32 {
    try directory.verify()
    let lock = openat(directory.descriptor,".linux-snapshot.lock",O_RDWR | (restoring ? 0 : O_CREAT) | O_NOFOLLOW | O_CLOEXEC,0o600)
    guard lock >= 0 else { throw snapshotError("Cannot open Linux machine identity lock") }
    do {
        try snapshotPrivate(snapshotStat(lock),directory:false,descriptor:lock)
        guard flock(lock,LOCK_EX | LOCK_NB) == 0 else { throw snapshotError("Linux machine identity is already owned by a VM") }
        try directory.verify()
    } catch { close(lock); throw error }
    return lock
}
/// Only newly created capture/clone artifacts may have their mode normalized.
/// ACL rejection happens before chmod; original inputs are never altered.
func linuxSnapshotSecureNewArtifact(_ descriptor: Int32) throws {
    let value = try snapshotStat(descriptor)
    guard value.st_uid == geteuid(), value.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
          value.st_nlink == 1 else { throw snapshotError("New snapshot artifact ownership/type mismatch") }
    try snapshotNoExtendedACL(descriptor)
    guard fchmod(descriptor,0o600) == 0 else { throw snapshotError("Cannot secure new snapshot artifact mode") }
    try snapshotPrivate(snapshotStat(descriptor),directory:false,descriptor:descriptor)
}
/// Bounded aligned reads with no-cache policy; do not fill host page cache with a VM image.
func linuxSnapshotHash(_ fd: Int32) throws -> (UInt64,String) {
    let before = try snapshotStat(fd)
    guard before.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), before.st_size > 0,
          before.st_size <= 16 * 1024 * 1024 * 1024 * 1024 else { throw snapshotError("Invalid snapshot hash input") }
    guard fcntl(fd,F_NOCACHE,1) == 0, fcntl(fd,F_RDAHEAD,0) == 0 else { throw snapshotError("Cannot disable snapshot read caching") }
    var allocation: UnsafeMutableRawPointer?
    let capacity = 1024 * 1024
    guard posix_memalign(&allocation,Int(getpagesize()),capacity) == 0, let buffer = allocation else { throw snapshotError("Snapshot hash buffer allocation failed") }
    defer { free(buffer) }
    var hash = SHA256(), offset: off_t = 0
    while offset < before.st_size {
        let want = min(capacity,Int(before.st_size-offset))
        let n = pread(fd,buffer,want,offset)
        if n < 0 && errno == EINTR { continue }
        guard n > 0 else { throw snapshotError("Snapshot hash input truncated or unreadable") }
        hash.update(bufferPointer:UnsafeRawBufferPointer(start:buffer,count:n)); offset += off_t(n)
    }
    guard snapshotSame(before,try snapshotStat(fd)) else { throw snapshotError("Snapshot input changed while hashing") }
    return (UInt64(offset),hash.finalize().map { String(format:"%02x",$0) }.joined())
}
private func linuxSnapshotBootHash(_ path: String) throws -> String {
    let fd = open(path,O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
    guard fd >= 0 else { throw snapshotError("Cannot open snapshot boot input") }
    defer { close(fd) }
    let initial = try snapshotStat(fd)
    let result = try linuxSnapshotHash(fd)
    var edge = stat()
    guard lstat(path,&edge) == 0, snapshotSame(initial,edge) else { throw snapshotError("Snapshot boot input path changed") }
    return result.1
}

// SNAPSHOT_STORAGE_CPU_PREFIX_END: runner compiles the byte-exact prefix above.
final class LinuxSnapshotContext {
    let identityDirectory: LinuxSnapshotDirectory
    let machineIdentifier: VZGenericMachineIdentifier
    let macAddress: VZMACAddress
    let machineSHA256: String
    private let identityLock: Int32
    private let config: Config
    var configurationSHA256: String?
    var support: [String:Any]?
    init(config: Config, restoring: Bool) throws {
        guard config.guest == "linux", let path = config.machine_identity_dir else { throw snapshotError("Linux snapshots require persistent machine_identity_dir") }
        self.config = config
        identityDirectory = try LinuxSnapshotDirectory(path,createLeaf:!restoring)
        let lock = try linuxSnapshotIdentityLock(identityDirectory,restoring:restoring)
        do {
            let identity: [String:Any]
            var metadata = stat()
            if fstatat(identityDirectory.descriptor,"linux-identity-v1.json",&metadata,AT_SYMLINK_NOFOLLOW) != 0 {
                guard errno == ENOENT, !restoring else { throw snapshotError("Restore requires existing Linux machine identity") }
                let machine = VZGenericMachineIdentifier()
                let mac = VZMACAddress.randomLocallyAdministered()
                identity = ["schema":1,"vm":config.id,"machine_base64":machine.dataRepresentation.base64EncodedString(),"mac_address":mac.string]
                try identityDirectory.create("linux-identity-v1.json",data:snapshotJSON(identity))
            } else {
                guard let decoded = try JSONSerialization.jsonObject(with:identityDirectory.read("linux-identity-v1.json",maximum:4096)) as? [String:Any] else { throw snapshotError("Malformed Linux machine identity") }
                identity = decoded
            }
            guard Set(identity.keys) == Set(["schema","vm","machine_base64","mac_address"]),
                  let schema = identity["schema"] as? NSNumber, CFGetTypeID(schema) != CFBooleanGetTypeID(), schema.doubleValue == 1,
                  identity["vm"] as? String == config.id,
                  let encoded = identity["machine_base64"] as? String, let data = Data(base64Encoded:encoded),
                  data.count > 0, data.count <= 1024, let machine = VZGenericMachineIdentifier(dataRepresentation:data),
                  let macString = identity["mac_address"] as? String, let mac = VZMACAddress(string:macString),
                  mac.string == macString, let firstMACByte = UInt8(macString.prefix(2),radix:16), firstMACByte & 3 == 2 else { throw snapshotError("Invalid or foreign Linux machine identity") }
            machineIdentifier = machine; macAddress = mac; machineSHA256 = snapshotDigest(data)
        } catch { close(lock); throw error }
        identityLock = lock
    }
    deinit { close(identityLock) }
    func inspect(configuration: VZVirtualMachineConfiguration, diskDescriptor: Int32?) throws -> [String:Any] {
        try identityDirectory.verify()
        try snapshotPrivate(snapshotStat(identityLock),directory:false,descriptor:identityLock)
        try configuration.validateSaveRestoreSupport()
        let diskBytes: UInt64
        if let descriptor = diskDescriptor {
            try snapshotNoExtendedACL(descriptor)
            let metadata = try snapshotStat(descriptor)
            guard metadata.st_size > 0, metadata.st_size % 512 == 0 else { throw snapshotError("Invalid locked snapshot disk extent") }
            diskBytes = UInt64(metadata.st_size)
        }
        else { diskBytes = 0 }
        let kernel = try linuxSnapshotBootHash(required(config.kernel,"kernel"))
        let initialRAM = try config.initrd.map(linuxSnapshotBootHash) ?? "none"
        var operatingSystemBuild = [CChar](repeating:0,count:256), buildLength = 256
        guard sysctlbyname("kern.osversion",&operatingSystemBuild,&buildLength,nil,0) == 0 else { throw snapshotError("Cannot bind snapshot host OS build") }
        let classes: [String:Any] = ["platform":"VZGenericPlatformConfiguration","boot":"VZLinuxBootLoader",
            "graphics":"VZVirtioGraphicsDeviceConfiguration","keyboard":"VZUSBKeyboardConfiguration",
            "pointer":"VZUSBScreenCoordinatePointingDeviceConfiguration","balloon":"VZVirtioTraditionalMemoryBalloonDeviceConfiguration",
            "network":"VZVirtioNetworkDeviceConfiguration/VZNATNetworkDeviceAttachment","entropy":"VZVirtioEntropyDeviceConfiguration",
            "vsock":"VZVirtioSocketDeviceConfiguration","disk":config.disk == nil ? "none" : "VZVirtioBlockDeviceConfiguration/raw-rw",
            "serial":config.serial_log == nil ? "none" : "VZVirtioConsoleDeviceSerialPortConfiguration/FileHandle",
            "share":config.shared_dir == nil ? "none" : "VZVirtioFileSystemDeviceConfiguration/readonly"]
        let semantic: [String:Any] = ["schema":1,"backend":"apple-virtualization","architecture":"arm64","vm":config.id,
            "host_os_build":String(cString:operatingSystemBuild),"machine_sha256":machineSHA256,"network_mac":macAddress.string,
            "cpus":config.cpus,"memory_mib":config.memory_mib,"kernel_sha256":kernel,"initrd_sha256":initialRAM,
            "command_line":config.command_line ?? "console=hvc0","disk_bytes":diskBytes,
            "width":config.width ?? 2560,"height":config.height ?? 1440,"shared_dir":config.shared_dir ?? "none","devices":classes]
        let digest = snapshotDigest(try snapshotJSON(semantic))
        configurationSHA256 = digest
        let response: [String:Any] = ["status":"implemented_unverified","can_save_restore":true,"support_query_passed":true,"runtime_restore_verified":false,
            "capture_retirement_protocol":1,
            "backend_version":"macos-" + String(cString:operatingSystemBuild),"host_architecture":"aarch64","machine":"Apple-VZGeneric-v1",
            "machine_identity_sha256":machineSHA256,"device_configuration_sha256":snapshotDigest(try snapshotJSON(classes)),
            "configuration_sha256":digest,"machine_identifier_sha256":machineSHA256,"disk_bytes":diskBytes,
            "host_os_build":String(cString:operatingSystemBuild),"device_classes":classes,
            "scope":"Apple save/restore configuration query only; same-host encrypted state, no accelerated Linux 3D or executed restore claim",
            "external_state":"NAT connections and readonly host-share contents are external and are not snapshotted"]
        support = response; return response
    }
    func prepareCapture(staging: String, captureID: String, expectedConfiguration: String) throws -> LinuxSnapshotDirectory {
        guard linuxSnapshotHex(captureID,bytes:16), linuxSnapshotHex(expectedConfiguration,bytes:32),
              expectedConfiguration == configurationSHA256 else { throw snapshotError("Snapshot capture/configuration binding mismatch") }
        let directory = try LinuxSnapshotDirectory(staging)
        guard URL(fileURLWithPath:staging).lastPathComponent == ".pending-\(captureID)" else { throw snapshotError("Snapshot staging name must bind capture ID") }
        try directory.absent("machine.vmstate"); try directory.absent("disk.raw")
        return directory
    }
    func finishCapture(directory: LinuxSnapshotDirectory, captureID: String, workerInstance: String,
                       diskDescriptor: Int32?, paused: Bool) throws -> [String:Any] {
        guard paused, linuxSnapshotHex(workerInstance,bytes:32), let configurationSHA256 else { throw snapshotError("Snapshot source quiescence/worker identity unconfirmed") }
        try directory.verify()
        // VZ owns creation through its URL API. Normalize only that newly
        // created, same-UID regular leaf; never follow links or overwrite data.
        let stateFD = openat(directory.descriptor,"machine.vmstate",O_RDWR | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard stateFD >= 0 else { throw snapshotError("Saved state artifact is missing") }
        do {
            try linuxSnapshotSecureNewArtifact(stateFD)
            guard fsync(stateFD) == 0 else { throw snapshotError("Saved state durability unconfirmed") }
        } catch { close(stateFD); throw error }
        close(stateFD)
        let state = try LinuxSnapshotFile(directory:directory,leaf:"machine.vmstate")
        var artifacts: [[String:Any]] = []
        if let source = diskDescriptor {
            try snapshotNoExtendedACL(source)
            let before = try snapshotStat(source)
            guard before.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), before.st_size > 0,
                  before.st_size % 512 == 0 else { throw snapshotError("Invalid locked snapshot disk") }
            guard fcntl(source,F_FULLFSYNC) == 0 else { throw snapshotError("Paused disk flush failed") }
            try directory.absent("disk.raw")
            // APFS clone from the retained owned disk FD; never reopen the live path.
            guard fclonefileat(source,directory.descriptor,"disk.raw",0) == 0 else { throw snapshotError("Paused disk clone failed; source remains paused and no snapshot is published") }
            let cloneFD = openat(directory.descriptor,"disk.raw",O_RDWR | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
            guard cloneFD >= 0 else { throw snapshotError("Cannot open new paused disk clone") }
            do {
                let clone = try snapshotStat(cloneFD)
                guard before.st_dev != clone.st_dev || before.st_ino != clone.st_ino else { throw snapshotError("New paused disk clone identity mismatch") }
                try linuxSnapshotSecureNewArtifact(cloneFD)
            } catch { close(cloneFD); throw error }
            close(cloneFD)
            let disk = try LinuxSnapshotFile(directory:directory,leaf:"disk.raw")
            let copied = try snapshotStat(disk.descriptor)
            guard before.st_dev != copied.st_dev || before.st_ino != copied.st_ino,
                  before.st_size == copied.st_size, snapshotSame(before,try snapshotStat(source)),
                  fsync(disk.descriptor) == 0 else { throw snapshotError("Paused disk clone identity/extent/durability mismatch") }
            let (bytes,digest) = try disk.digest()
            artifacts.append(["role":"disk","bytes":bytes,"sha256":digest])
        }
        let (bytes,digest) = try state.digest()
        artifacts.append(["role":"machine_state","bytes":bytes,"sha256":digest])
        guard fsync(directory.descriptor) == 0 else { throw snapshotError("Snapshot staging durability unconfirmed") }
        try directory.verify()
        var receipt: [String:Any] = ["capture_id":captureID,"source_worker_instance":workerInstance,
            "worker_instance":workerInstance,"backend":"apple-virtualization","configuration_sha256":configurationSHA256,"backend_completed":true,
            "source_quiesced":true,"ram_cpu_devices_saved":true,"disk_in_same_quiesced_interval":true,
            "artifacts":artifacts,"state":"paused","restore_verified":false]
        receipt["backend_receipt_sha256"] = snapshotDigest(try snapshotJSON(receipt))
        return receipt
    }
    func restoreInput(_ request: LinuxSnapshotRestore) throws -> LinuxSnapshotFile {
        guard linuxSnapshotHex(request.sha256,bytes:32), linuxSnapshotHex(request.configurationSHA256,bytes:32),
              linuxSnapshotHex(request.captureID,bytes:16), request.configurationSHA256 == configurationSHA256 else { throw snapshotError("Restore configuration/capture binding mismatch") }
        let url = URL(fileURLWithPath:request.path)
        guard request.path.hasPrefix("/"), url.lastPathComponent == "machine.vmstate" else { throw snapshotError("Restore state must be the fixed private artifact") }
        let directory = try LinuxSnapshotDirectory(url.deletingLastPathComponent().path)
        let state = try LinuxSnapshotFile(directory:directory,leaf:"machine.vmstate")
        let (_,digest) = try state.digest()
        guard digest == request.sha256 else { throw snapshotError("Restore state SHA256 mismatch") }
        return state
    }
}

final class LinuxSnapshotRetirement {
    let directory: LinuxSnapshotDirectory
    let captureID: String
    private let receipt: [String:Any]
    init(path: String, operationID: String, captureID: String, worker: String, configuration: String, vm: String) throws {
        self.captureID = captureID
        guard linuxSnapshotHex(operationID,bytes:16), linuxSnapshotHex(captureID,bytes:16),
              linuxSnapshotHex(worker,bytes:32), linuxSnapshotHex(configuration,bytes:32),
              URL(fileURLWithPath:path).lastPathComponent == operationID,
              URL(fileURLWithPath:path).deletingLastPathComponent().lastPathComponent == "snapshot-operations" else {
            throw snapshotError("Invalid private retirement operation binding")
        }
        directory = try LinuxSnapshotDirectory(path)
        guard let admission = try JSONSerialization.jsonObject(with:directory.read("0000.json",maximum:4096)) as? [String:Any],
              admission["operation_id"] as? String == operationID,
              admission["id"] as? String == vm, admission["action"] as? String == "create",
              admission["worker_instance"] as? String == worker else { throw snapshotError("Retirement journal is not bound to this worker/VM") }
        try directory.absent("native-retired.json")
        receipt = ["schema":1,"operation_id":operationID,"id":vm,"capture_id":captureID,
            "worker_instance":worker,"configuration_sha256":configuration,
            "capture_io_drained":true,"backend_stopped":true,"normal_guest_shutdown":false]
    }
    func persist() throws {
        // Caller has observed snapshotBusy=false and VZ state stopped. No
        // receipt is written on a missing/error stop or hard native termination.
        try directory.create("native-retired.json",data:snapshotJSON(receipt))
    }
}

extension Runner {
    func detachSnapshotInput() { (window?.contentView as? VZVirtualMachineView)?.virtualMachine = nil }
    func attachSnapshotInput() {
        guard !ownerDisconnected else { return }
        (window?.contentView as? VZVirtualMachineView)?.virtualMachine = vm
    }
    func verifySnapshotDisk() throws {
        guard let path = config.disk else { return }
        guard let descriptor = lockedDiskDescriptor else { throw snapshotError("Snapshot source disk is not locked") }
        try snapshotNoExtendedACL(descriptor)
        let held = try snapshotStat(descriptor)
        var current = stat()
        guard lstat(path,&current) == 0, current.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              held.st_dev == current.st_dev, held.st_ino == current.st_ino,
              held.st_size == current.st_size else { throw snapshotError("Snapshot disk path no longer identifies the locked source") }
    }
    func linuxSnapshotCommand(_ object: [String:Any]) {
        let id = object["id"] ?? NSNull()
        let method = object["method"] as? String ?? ""
        func reject(_ code: String, _ message: String) { emit(["id":id,"error":["code":code,"message":message]]) }
        func ok(_ response: [String:Any]) { emit(["id":id,"result":response]) }
        guard config.guest == "linux", let context = linuxSnapshot, let configuration = snapshotConfiguration,
              !installing else { reject("unsupported","Linux snapshots require persistent machine_identity_dir and Apple save/restore support"); return }
        if method == "snapshot_support" {
            guard Set(object.keys).isSubset(of:["id","method"]), !snapshotBusy else { reject("snapshot_busy","Snapshot configuration query is busy or malformed"); return }
            snapshotBusy = true
            snapshotIO.async {
                do {
                    try self.verifySnapshotDisk()
                    let response = try context.inspect(configuration:configuration,diskDescriptor:self.lockedDiskDescriptor)
                    DispatchQueue.main.async { self.snapshotBusy = false; ok(response); self.retireDisconnectedOwnerIfIdle() }
                } catch {
                    DispatchQueue.main.async { self.snapshotBusy = false; reject("snapshot_unsupported",error.localizedDescription); self.retireDisconnectedOwnerIfIdle() }
                }
            }
            return
        }
        guard let captureID = object["capture_id"] as? String, linuxSnapshotHex(captureID,bytes:16) else { reject("invalid_request","A lowercase 128-bit capture ID is required"); return }
        if method == "snapshot_resume" {
            guard Set(object.keys).isSubset(of:["id","method","capture_id"]), !snapshotBusy,
                  snapshotCaptureID == captureID || (vm.state == .running && snapshotCaptureID == nil && snapshotRetirement?.captureID == captureID) else { reject("snapshot_busy","No matching completed snapshot transaction"); return }
            do { try verifySnapshotDisk() } catch { reject("snapshot_recovery_required",error.localizedDescription); return }
            if vm.state == .running {
                // Explicit recovery after a failed pause callback which left the
                // VM running. No earlier failure autonomously resumes anything.
                snapshotCaptureID = nil; attachSnapshotInput()
                ok(["capture_id":captureID,"state":"running","resumed":false,"already_running":true]); return
            }
            guard vm.state == .paused, vm.canResume else { reject("invalid_state","Snapshot source is not resumable from paused state"); return }
            snapshotBusy = true
            vm.resume { result in
                self.snapshotBusy = false
                do {
                    try result.get()
                    guard self.vm.state == .running else { throw snapshotError("Snapshot resume did not reach running state") }
                    self.snapshotCaptureID = nil; self.attachSnapshotInput()
                    ok(["capture_id":captureID,"state":"running","resumed":true]); self.reportState()
                } catch { reject("snapshot_recovery_required",error.localizedDescription) }
                self.retireDisconnectedOwnerIfIdle()
            }
            return
        }
        if method == "snapshot_retire" {
            guard Set(object.keys).isSubset(of:["id","method","capture_id"]), !snapshotBusy,
                  snapshotCaptureID == captureID, vm.state == .paused, vm.canStop else { reject("invalid_state","Retirement requires this transaction's paused source VM"); return }
            snapshotBusy = true; snapshotRetiring = true
            vm.stop { error in
                if let error { self.snapshotBusy = false; self.snapshotRetiring = false; reject("snapshot_recovery_required",error.localizedDescription); self.retireDisconnectedOwnerIfIdle(); return }
                guard self.vm.state == .stopped else { reject("snapshot_recovery_required","Snapshot source retirement did not reach stopped state"); return }
                self.snapshotBusy = false
                if self.ownerDisconnected { self.retireDisconnectedOwnerIfIdle(); return }
                ok(["capture_id":captureID,"state":"stopped","retired":true,"normal_guest_shutdown":false])
                self.reportState(); exit(0)
            }
            return
        }
        guard method == "snapshot_capture", Set(object.keys).isSubset(of:["id","method","capture_id","worker_instance","staging_dir","config_sha256","operation_id","retirement_directory"]),
              let worker = object["worker_instance"] as? String, linuxSnapshotHex(worker,bytes:32),
              let staging = object["staging_dir"] as? String, let expected = object["config_sha256"] as? String,
              linuxSnapshotHex(expected,bytes:32), let operationID = object["operation_id"] as? String,
              let retirementDirectory = object["retirement_directory"] as? String else { reject("invalid_request","Invalid typed Linux snapshot capture"); return }
        guard !snapshotBusy, snapshotCaptureID == nil, guestRequests.isEmpty, guestIOActive == 0,
              serialWritesActive == 0, guestStreamBridge == nil,
              vm.state == .running || vm.state == .paused else {
            ok(["capture_id":captureID,"state":stateName(vm.state),"backend_completed":false,
                "source_quiesced":vm.state == .paused,"snapshot_capture_state":"not_started",
                "error":["code":"snapshot_capture_failed","message":"Snapshot requires an idle Guest transport and running/paused VM"]]); return
        }
        do {
            snapshotRetirement = try LinuxSnapshotRetirement(path:retirementDirectory,operationID:operationID,captureID:captureID,
                worker:worker,configuration:expected,vm:config.id)
        } catch {
            ok(["capture_id":captureID,"state":stateName(vm.state),"backend_completed":false,"source_quiesced":false,
                "snapshot_capture_state":"not_started","error":["code":"snapshot_capture_failed","message":error.localizedDescription]])
            return
        }
        snapshotBusy = true; snapshotCaptureID = captureID; detachSnapshotInput()
        func captureFailure(_ error: Error, pauseAttempted: Bool) {
            self.snapshotBusy = false
            let state = self.vm.state
            if !pauseAttempted { self.snapshotCaptureID = nil; self.attachSnapshotInput() }
            ok(["capture_id":captureID,"state":stateName(state),"backend_completed":false,"source_quiesced":state == .paused,
                "snapshot_capture_state":!pauseAttempted ? "not_started" : state == .paused ? "paused_recovery_required" : "pause_transition_unconfirmed",
                "error":["code":"snapshot_capture_failed","message":error.localizedDescription]])
            self.reportState()
            self.retireDisconnectedOwnerIfIdle()
        }
        snapshotIO.async {
            do {
                try self.verifySnapshotDisk()
                _ = try context.inspect(configuration:configuration,diskDescriptor:self.lockedDiskDescriptor)
                let directory = try context.prepareCapture(staging:staging,captureID:captureID,expectedConfiguration:expected)
                let disk = try self.lockedDiskDescriptor.map { UInt64(try snapshotStat($0).st_size) } ?? 0
                let memory = self.config.memory_mib.multipliedReportingOverflow(by:1024 * 1024)
                let total = memory.partialValue.addingReportingOverflow(disk)
                let withMargin = total.partialValue.addingReportingOverflow(64 * 1024 * 1024)
                guard !memory.overflow, !total.overflow, !withMargin.overflow else { throw snapshotError("Snapshot space extent overflow") }
                try directory.freeSpace(required:withMargin.partialValue)
                DispatchQueue.main.async {
                    func savePaused() {
                        guard self.vm.state == .paused else { captureFailure(snapshotError("Snapshot pause did not reach paused state"),pauseAttempted:true); return }
                        self.vm.saveMachineStateTo(url:URL(fileURLWithPath:directory.path + "/machine.vmstate")) { error in
                            if let error { captureFailure(error,pauseAttempted:true); return }
                            guard self.vm.state == .paused else { captureFailure(snapshotError("Snapshot source left paused state after save"),pauseAttempted:true); return }
                            self.snapshotIO.async {
                                do {
                                    try self.verifySnapshotDisk()
                                    let receipt = try context.finishCapture(directory:directory,captureID:captureID,workerInstance:worker,
                                                                            diskDescriptor:self.lockedDiskDescriptor,paused:true)
                                    DispatchQueue.main.async {
                                        guard self.vm.state == .paused else { captureFailure(snapshotError("Snapshot source changed state during disk capture"),pauseAttempted:true); return }
                                        self.snapshotBusy = false; ok(receipt); self.reportState(); self.retireDisconnectedOwnerIfIdle()
                                    }
                                } catch { DispatchQueue.main.async { captureFailure(error,pauseAttempted:true) } }
                            }
                        }
                    }
                    if self.vm.state == .paused { savePaused() }
                    else if self.vm.state == .running && self.vm.canPause {
                        self.vm.pause { result in
                            do { try result.get(); savePaused() }
                            catch { captureFailure(error,pauseAttempted:true) }
                        }
                    } else { captureFailure(snapshotError("Snapshot source changed state before pause"),pauseAttempted:false) }
                }
            } catch { DispatchQueue.main.async { captureFailure(error,pauseAttempted:false) } }
        }
    }
}
