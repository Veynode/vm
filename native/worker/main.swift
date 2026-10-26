import AppKit
import Virtualization
import Foundation
import Darwin

let outputLock = NSLock()
private var workerOutputDisconnected = false
private var workerOutputDisconnectHandler: (() -> Void)?
// Keep the worker alive long enough to drain framework writers on owner loss.
private let ignoredWorkerPipeSignal: Void = { _ = signal(SIGPIPE, SIG_IGN) }()
func suppressWorkerOutput() {
    outputLock.lock(); workerOutputDisconnected = true; outputLock.unlock()
}
func setWorkerOutputDisconnectHandler(_ handler: @escaping () -> Void) {
    outputLock.lock()
    workerOutputDisconnectHandler = handler
    let disconnected = workerOutputDisconnected
    outputLock.unlock()
    if disconnected { DispatchQueue.main.async(execute:handler) }
}
func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes]),
          let line = String(data: data, encoding: .utf8) else { return }
    _ = ignoredWorkerPipeSignal
    outputLock.lock()
    guard !workerOutputDisconnected else { outputLock.unlock(); return }
    do { try FileHandle.standardOutput.write(contentsOf:Data((line + "\n").utf8)); outputLock.unlock() }
    catch {
        workerOutputDisconnected = true
        let handler = workerOutputDisconnectHandler
        outputLock.unlock()
        if let handler { DispatchQueue.main.async(execute:handler) }
    }
}
// Compact standard JSON preserves the full 32 KiB binary chunk contract without
// spending the 64 KiB line budget on optional escaping of base64 slash bytes.
// driver_rpc carries cua-driver JSON-RPC frames whose user text (type_text, set_value,
// clipboard_write) can exceed 64 KiB, so it gets 1 MiB like replies.
let guestRequestLimit = 65536
let driverRequestLimit = 1024 * 1024
let guestRequestSeconds = 35
let driverMaxDeadlineMilliseconds = 600_000
func guestRequestData(_ request: [String: Any], limit: Int = guestRequestLimit) -> Data? {
    guard var data = try? JSONSerialization.data(withJSONObject: request, options: [.withoutEscapingSlashes]),
          data.count < limit else { return nil }
    data.append(10)
    return data
}
func fail(_ message: String) -> Never { emit(["event":"state","state":"error","message":message]); exit(1) }
struct Config: Decodable {
    let id: String
    let guest: String
    let cpus: Int
    let memory_mib: UInt64
    var kernel: String?
    var initrd: String?
    var command_line: String?
    var disk: String?
    var boot: String?
    var efi_vars: String?
    var cdrom: String?
    var restore_image: String?
    var machine_identity_dir: String?
    var window: Bool?
    var serial_log: String?
    var shared_dir: String?
    var width: Int?
    var height: Int?
    var extra_disks: [ExtraDisk]?
    var shares: [Share]?
    var mac_address: String?
    var screen_sharing_login: String?
    var machine_identifier_file: String?
}
struct ExtraDisk: Decodable { let path: String; let read_only: Bool }
struct Share: Decodable { let tag: String; let path: String; var read_only: Bool? }
struct NativeError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}
func required(_ value: String?, _ name: String) throws -> String {
    guard let value, !value.isEmpty else { throw NativeError(message: "Missing \(name)") }
    return value
}
func stateName(_ state: VZVirtualMachine.State) -> String {
    switch state {
    case .stopped: return "stopped"
    case .running: return "running"
    case .paused: return "paused"
    case .error: return "error"
    case .starting: return "starting"
    case .pausing: return "pausing"
    case .resuming: return "resuming"
    case .stopping: return "stopping"
    case .saving: return "saving"
    case .restoring: return "restoring"
    @unknown default: return "unknown"
    }
}

// Raw key codes and modifier flags can reconstruct typed input on a known layout.
// Enable only with test input, never secrets; the log is private (0600) and bounded.
enum KeyEventLogType: String { case keyDown, keyUp, flagsChanged }
func keyEventLogLine(timestamp: UInt64, type: KeyEventLogType, keyCode: UInt16,
                     modifierFlags: UInt, hasCharactersIgnoringModifiers: Bool,
                     isARepeat: Bool, windowIsKey: Bool) -> Data {
    let fields: [String:Any] = ["monotonic_ns":timestamp,"event_type":type.rawValue,
        "keyCode":keyCode,"modifierFlags":modifierFlags,
        "hasCharactersIgnoringModifiers":hasCharactersIgnoringModifiers,
        "isARepeat":isARepeat,"windowIsKey":windowIsKey]
    var line = try! JSONSerialization.data(withJSONObject:fields, options:[.sortedKeys])
    line.append(10); return line
}
final class KeyEventLogWriter {
    static let limit = 1024 * 1024
    static let finalLine = Data("{\"event_type\":\"logging_stopped\",\"reason\":\"size_limit\"}\n".utf8)
    private let fd: Int32
    private let lock = NSLock()
    private var stopped = false
    init(path: String) throws {
        guard !path.isEmpty, !path.utf8.contains(0) else { throw NativeError(message:"Invalid key event log path") }
        let fd = open(path,O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK,0o600)
        guard fd >= 0 else { throw NativeError(message:"Cannot open key event log") }
        var metadata = stat()
        guard fstat(fd,&metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              metadata.st_uid == getuid(), metadata.st_mode & 0o777 == 0o600, metadata.st_nlink == 1,
              metadata.st_size <= Self.limit - Self.finalLine.count,
              flock(fd,LOCK_EX | LOCK_NB) == 0 else {
            close(fd); throw NativeError(message:"Key event log must be a private regular file with space remaining and no other writer")
        }
        self.fd = fd
    }
    deinit {
        let descriptor = fd
        KeyEventLogQueue.writerQueue.async { close(descriptor) }
    }
    func append(_ line: Data) {
        lock.lock(); defer { lock.unlock() }
        guard !stopped else { return }
        var metadata = stat()
        guard fstat(fd,&metadata) == 0, metadata.st_size <= Self.limit - Self.finalLine.count else {
            stopped = true; return
        }
        let remaining = Self.limit - Self.finalLine.count - Int(metadata.st_size)
        if line.count > remaining { stopped = true; writeLine(Self.finalLine) }
        else { writeLine(line) }
    }
    private func writeLine(_ line: Data) {
        line.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let count = write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { stopped = true; return }
                offset += count
            }
        }
    }
}
final class KeyEventLogQueue {
    static let capacity = 256
    static let writerQueue = DispatchQueue(label:"veynode.key-event-log")
    private let lock = NSLock()
    private var lines = [Data]()
    private var draining = false
    private var dropped = 0
    private let write: (Data) -> Void

    init(write: @escaping (Data) -> Void) { self.write = write }
    static func open(path: String, completion: @escaping (Result<KeyEventLogQueue, Error>) -> Void) {
        writerQueue.async {
            do {
                let writer = try KeyEventLogWriter(path:path)
                completion(.success(KeyEventLogQueue(write:writer.append)))
            } catch { completion(.failure(error)) }
        }
    }
    var counts: (queued: Int, dropped: Int) {
        lock.lock(); defer { lock.unlock() }
        return (lines.count, dropped)
    }
    func append(_ line: Data) {
        lock.lock()
        guard lines.count < Self.capacity else { dropped += 1; lock.unlock(); return }
        lines.append(line)
        let start = !draining; draining = true
        lock.unlock()
        if start { Self.writerQueue.async { self.drain() } }
    }
    private func drain() {
        while true {
            lock.lock()
            guard !lines.isEmpty else { draining = false; lock.unlock(); return }
            let line = lines.removeFirst()
            lock.unlock()
            // No queue lock is held during file I/O, including the final marker.
            write(line)
        }
    }
}
final class KeyEventLoggingVMView: VZVirtualMachineView {
    var keyEventLog: KeyEventLogQueue?
    private func record(_ event: NSEvent, _ type: KeyEventLogType) {
        // AppKit's character/repeat accessors apply only to key up/down events.
        let key = type != .flagsChanged
        keyEventLog?.append(keyEventLogLine(timestamp:DispatchTime.now().uptimeNanoseconds,
            type:type,keyCode:event.keyCode,modifierFlags:event.modifierFlags.rawValue,
            hasCharactersIgnoringModifiers:key && event.charactersIgnoringModifiers != nil,
            isARepeat:key && event.isARepeat,windowIsKey:window?.isKeyWindow ?? false))
    }
    override func keyDown(with event: NSEvent) { super.keyDown(with:event); record(event,.keyDown) }
    override func keyUp(with event: NSEvent) { super.keyUp(with:event); record(event,.keyUp) }
    override func flagsChanged(with event: NSEvent) { super.flagsChanged(with:event); record(event,.flagsChanged) }
}

final class Runner: NSObject, VZVirtualMachineDelegate, NSWindowDelegate, NSApplicationDelegate {
    let config: Config
    let keyEventLog: KeyEventLogQueue?
    var vm: VZVirtualMachine!
    var window: NSWindow?
    // A person's input in the window, for the daemon's control lease: at most one
    // person_input event a second, none for synthetic events.
    var personInputMonitor: Any?
    var lastPersonInput = Date.distantPast
    var serialInput: Pipe?
    var serialOutput: FileHandle?
    let serialWriteQueue = DispatchQueue(label:"veynode.serial.write")
    var installer: VZMacOSInstaller?
    var progressObservation: NSKeyValueObservation?
    var installing = false
    var guestRequests = Set<UUID>()
    var guestStreamBridge: GuestStreamBridge?
    let guestStreamProbe = GuestStreamProbeCoordinator()
    var guestStreamBinding:(worker:String,bridge:String)?
    var diskLock: Int32?
    lazy var screenSharing: ScreenSharingGuest? = {
        guard config.guest == "macos", let mac = config.mac_address, let login = config.screen_sharing_login else { return nil }
        return ScreenSharingGuest(mac: mac, loginFile: login)
    }()
    var linuxSnapshot: LinuxSnapshotContext?
    let snapshotRestore: LinuxSnapshotRestore?
    var machineStateRestore: String?
    var snapshotConfiguration: VZVirtualMachineConfiguration?
    var snapshotBusy = false
    var snapshotCaptureID: String?
    var snapshotRetiring = false
    var ownerDisconnected = false
    var ownerRetiring = false
    var snapshotRetirement: LinuxSnapshotRetirement?
    var guestIOActive = 0
    var serialWritesActive = 0
    let snapshotIO = DispatchQueue(label:"veynode.linux-snapshot.io",qos:.utility)
    init(_ config: Config, keyEventLog: KeyEventLogQueue? = nil, snapshotRestore: LinuxSnapshotRestore? = nil) {
        self.config = config; self.keyEventLog = keyEventLog; self.snapshotRestore = snapshotRestore
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
    var lockedDiskDescriptor: Int32?
    deinit {
        if let diskLock { close(diskLock) }
        if let lockedDiskDescriptor { close(lockedDiskDescriptor) }
    }
    func lockWritableDisk() throws {
        guard let path = config.disk else { return }
        let descriptor = open(path,O_RDWR | O_CLOEXEC)
        guard descriptor >= 0 else { throw NativeError(message:"Cannot open writable disk: \(String(cString:strerror(errno)))") }
        var retained = false
        defer { if !retained { close(descriptor) } }
        var metadata = stat()
        guard fstat(descriptor,&metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else {
            throw NativeError(message:"Writable disk must be a regular file")
        }
        // Virtualization.framework takes its own exclusive lock in the VM XPC
        // process: retaining a flock on the image itself prevents VZ startup.
        // Our cooperating workers therefore lock an owner-private inode key.
        let support = URL(fileURLWithPath:NSHomeDirectory()).appendingPathComponent("Library/Application Support")
        let supportFD = open(support.path,O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard supportFD >= 0 else { throw NativeError(message:"Cannot open Application Support lock parent") }
        defer { close(supportFD) }
        func directory(_ parent:Int32,_ name:String,privateOnly:Bool) throws -> Int32 {
            if mkdirat(parent,name,0o700) != 0 && errno != EEXIST { throw NativeError(message:"Cannot create disk lock directory") }
            let fd = openat(parent,name,O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
            guard fd >= 0 else { throw NativeError(message:"Disk lock directory must not be a symlink") }
            var st = stat()
            guard fstat(fd,&st) == 0,st.st_uid == getuid(),st.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
                  st.st_mode & mode_t(privateOnly ? 0o077 : 0o022) == 0 else {
                close(fd);throw NativeError(message:"Disk lock directory has unsafe ownership or permissions")
            }
            return fd
        }
        let productFD = try directory(supportFD,"VeynodeVM",privateOnly:false)
        defer { close(productFD) }
        let locksFD = try directory(productFD,"locks",privateOnly:true)
        defer { close(locksFD) }
        let key = "\(metadata.st_dev)-\(metadata.st_ino).lock"
        let lockFD = openat(locksFD,key,O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC,0o600)
        guard lockFD >= 0 else { throw NativeError(message:"Cannot open inode lock file") }
        var lockMetadata = stat()
        guard fstat(lockFD,&lockMetadata) == 0,lockMetadata.st_uid == getuid(),lockMetadata.st_nlink == 1,
              lockMetadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),lockMetadata.st_mode & 0o077 == 0 else {
            close(lockFD);throw NativeError(message:"Disk lock file has unsafe ownership, links, or permissions")
        }
        guard flock(lockFD,LOCK_EX | LOCK_NB) == 0 else {
            let reason = String(cString:strerror(errno));close(lockFD)
            throw NativeError(message:"Writable disk is already in use or cannot be locked: \(reason)")
        }
        diskLock = lockFD
        lockedDiskDescriptor = descriptor
        retained = true
    }
    func makeConfiguration(restore: VZMacOSRestoreImage?) throws -> VZVirtualMachineConfiguration {
        let c = VZVirtualMachineConfiguration()
        c.cpuCount = config.cpus
        guard config.memory_mib <= UInt64.max / (1024 * 1024) else { throw NativeError(message:"memory_mib overflow") }
        c.memorySize = config.memory_mib * 1024 * 1024
        let width = config.width ?? 2560, height = config.height ?? 1440
        guard width >= 640 && width <= 7680 && height >= 480 && height <= 4320 else { throw NativeError(message:"Invalid display dimensions") }
        let uefi = (config.boot ?? "kernel") == "uefi"
        guard ["kernel","uefi"].contains(config.boot ?? "kernel"), !uefi || config.guest == "linux" else { throw NativeError(message:"Unsupported boot mode") }
        if config.guest == "linux" {
            if uefi {
                // The distribution's own bootloader (GRUB/shim) from disk or installer medium.
                guard config.kernel == nil, config.initrd == nil, config.command_line == nil else { throw NativeError(message:"UEFI boot does not take kernel, initrd or command_line") }
                c.bootLoader = try efiBootLoader(path:try required(config.efi_vars,"efi_vars"))
            } else {
                let loader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: try required(config.kernel,"kernel")))
                if let initrd = config.initrd { loader.initialRamdiskURL = URL(fileURLWithPath:initrd) }
                loader.commandLine = config.command_line ?? "console=hvc0"
                c.bootLoader = loader
            }
            let platform = VZGenericPlatformConfiguration()
            if config.machine_identity_dir != nil {
                let context = try LinuxSnapshotContext(config:config,restoring:snapshotRestore != nil)
                linuxSnapshot = context
                platform.machineIdentifier = context.machineIdentifier
            } else if let path = config.machine_identifier_file {
                platform.machineIdentifier = try containerMachineIdentifier(path:path)
            }
            c.platform = platform
            let graphics = VZVirtioGraphicsDeviceConfiguration()
            graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: width, heightInPixels: height)]
            c.graphicsDevices = [graphics]
            c.keyboards = [VZUSBKeyboardConfiguration()]
            c.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]
            c.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]
        } else if config.guest == "macos" {
            let dir = URL(fileURLWithPath: try required(config.machine_identity_dir,"machine_identity_dir"), isDirectory: true)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes:[.posixPermissions:0o700])
            let platform = VZMacPlatformConfiguration()
            let hardwareURL = dir.appendingPathComponent("hardware-model.bin")
            let machineURL = dir.appendingPathComponent("machine-id.bin")
            let auxiliaryURL = dir.appendingPathComponent("auxiliary-storage.bin")
            if let restore {
                guard let requirements = restore.mostFeaturefulSupportedConfiguration else { throw NativeError(message:"Restore image unsupported on this host") }
                guard config.cpus >= requirements.minimumSupportedCPUCount && c.memorySize >= requirements.minimumSupportedMemorySize else { throw NativeError(message:"Configuration below restore image CPU/memory requirements") }
                guard !FileManager.default.fileExists(atPath: hardwareURL.path), !FileManager.default.fileExists(atPath: auxiliaryURL.path) else { throw NativeError(message:"Installation identity already exists; refusing overwrite") }
                platform.hardwareModel = requirements.hardwareModel
                platform.machineIdentifier = VZMacMachineIdentifier()
                platform.auxiliaryStorage = try VZMacAuxiliaryStorage(creatingStorageAt:auxiliaryURL, hardwareModel:requirements.hardwareModel)
                try platform.hardwareModel.dataRepresentation.write(to:hardwareURL, options:.atomic)
                try platform.machineIdentifier.dataRepresentation.write(to:machineURL, options:.atomic)
            } else {
                // A clone of an image without machine-id.bin gets its own identity.
                if !FileManager.default.fileExists(atPath: machineURL.path) {
                    try VZMacMachineIdentifier().dataRepresentation.write(to:machineURL, options:.atomic)
                }
                guard let hardware = VZMacHardwareModel(dataRepresentation:try Data(contentsOf:hardwareURL)), hardware.isSupported,
                      let machine = VZMacMachineIdentifier(dataRepresentation:try Data(contentsOf:machineURL)) else { throw NativeError(message:"Invalid or unsupported macOS machine identity") }
                guard FileManager.default.fileExists(atPath: auxiliaryURL.path) else { throw NativeError(message:"Missing auxiliary storage") }
                platform.hardwareModel = hardware
                platform.machineIdentifier = machine
                platform.auxiliaryStorage = VZMacAuxiliaryStorage(url:auxiliaryURL)
            }
            c.platform = platform
            c.bootLoader = VZMacOSBootLoader()
            let graphics = VZMacGraphicsDeviceConfiguration()
            graphics.displays = [VZMacGraphicsDisplayConfiguration(widthInPixels:width,heightInPixels:height,pixelsPerInch:110)]
            c.graphicsDevices = [graphics]
            c.keyboards = [VZMacKeyboardConfiguration()]
            c.pointingDevices = [VZMacTrackpadConfiguration()]
        } else { throw NativeError(message:"Unsupported guest: \(config.guest)") }
        let network = VZVirtioNetworkDeviceConfiguration()
        if let context = linuxSnapshot { network.macAddress = context.macAddress }
        network.attachment = VZNATNetworkDeviceAttachment()
        if let mac = config.mac_address {
            guard let address = VZMACAddress(string:mac) else { throw NativeError(message:"Invalid mac_address") }
            network.macAddress = address
        }
        c.networkDevices = [network]
        c.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        c.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        var shares:[(tag:String,path:String,readOnly:Bool)] = []
        if let directory = config.shared_dir { shares.append((tag:"veynode",path:directory,readOnly:true)) }
        for share in config.shares ?? [] {
            guard share.tag != "veynode" else { throw NativeError(message:"share tag veynode is reserved") }
            shares.append((tag:share.tag,path:share.path,readOnly:share.read_only ?? false))
        }
        var sharingDevices:[VZDirectorySharingDeviceConfiguration] = []
        for share in shares {
            let url = URL(fileURLWithPath:share.path).standardizedFileURL.resolvingSymlinksInPath()
            var isDirectory:ObjCBool = false
            guard FileManager.default.fileExists(atPath:url.path,isDirectory:&isDirectory), isDirectory.boolValue else { throw NativeError(message:"shared directories must exist") }
            try VZVirtioFileSystemDeviceConfiguration.validateTag(share.tag)
            let filesystem = VZVirtioFileSystemDeviceConfiguration(tag:share.tag)
            filesystem.share = VZSingleDirectoryShare(directory:VZSharedDirectory(url:url,readOnly:share.readOnly))
            sharingDevices.append(filesystem)
        }
        c.directorySharingDevices = sharingDevices
        if let disk = config.disk {
            var lockedMetadata = stat(),pathMetadata = stat()
            guard let descriptor = lockedDiskDescriptor,fstat(descriptor,&lockedMetadata) == 0,
                  stat(disk,&pathMetadata) == 0,lockedMetadata.st_dev == pathMetadata.st_dev,lockedMetadata.st_ino == pathMetadata.st_ino else {
                throw NativeError(message:"Writable disk path changed after lock acquisition")
            }
            let attachment = try VZDiskImageStorageDeviceAttachment(url: URL(fileURLWithPath:disk), readOnly:false)
            c.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment:attachment)]
        } else if config.guest == "macos" { throw NativeError(message:"macOS requires disk") }
        if let cdrom = config.cdrom {
            guard uefi else { throw NativeError(message:"cdrom requires UEFI boot") }
            var metadata = stat()
            guard lstat(cdrom,&metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { throw NativeError(message:"cdrom must be a regular file") }
            // Installer media appear as a read-only USB stick, which both the
            // firmware and distribution installers boot and mount.
            let attachment = try VZDiskImageStorageDeviceAttachment(url: URL(fileURLWithPath:cdrom), readOnly:true)
            c.storageDevices.append(VZUSBMassStorageDeviceConfiguration(attachment:attachment))
        }
        for extra in config.extra_disks ?? [] {
            // Only read-only extras are accepted: writable images need the primary disk lock.
            guard extra.read_only else { throw NativeError(message:"extra disks must be read-only") }
            let attachment = try VZDiskImageStorageDeviceAttachment(url: URL(fileURLWithPath:extra.path), readOnly:true)
            c.storageDevices.append(VZVirtioBlockDeviceConfiguration(attachment:attachment))
        }
        if let log = config.serial_log {
            let url = URL(fileURLWithPath:log)
            if !FileManager.default.fileExists(atPath:log) { guard FileManager.default.createFile(atPath:log, contents:nil, attributes:[.posixPermissions:0o600]) else { throw NativeError(message:"Cannot create serial log") } }
            let output = try FileHandle(forWritingTo:url)
            try output.seekToEnd()
            serialOutput = output
            let input = Pipe()
            serialInput = input
            let fd = input.fileHandleForWriting.fileDescriptor
            _ = fcntl(fd,F_SETFL,fcntl(fd,F_GETFL) | O_NONBLOCK)
            let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
            serial.attachment = VZFileHandleSerialPortAttachment(fileHandleForReading:input.fileHandleForReading,fileHandleForWriting:output)
            c.serialPorts = [serial]
        }
        try c.validate()
        return c
    }
    /// Per-VM UEFI variable store: created private on first start, then reused
    /// so the installed distribution's boot entries persist.
    func efiBootLoader(path: String) throws -> VZEFIBootLoader {
        let url = URL(fileURLWithPath:path)
        var metadata = stat()
        let store: VZEFIVariableStore
        if lstat(path,&metadata) == 0 {
            guard metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_uid == getuid(), metadata.st_mode & 0o077 == 0 else {
                throw NativeError(message:"efi_vars must be a private regular file owned by you")
            }
            store = VZEFIVariableStore(url:url)
        } else {
            guard errno == ENOENT else { throw NativeError(message:"Cannot inspect efi_vars: \(String(cString:strerror(errno)))") }
            store = try VZEFIVariableStore(creatingVariableStoreAt:url, options:[])
            guard chmod(path,0o600) == 0 else { throw NativeError(message:"Cannot make efi_vars private") }
        }
        let loader = VZEFIBootLoader()
        loader.variableStore = store
        return loader
    }
    func start(restore: VZMacOSRestoreImage? = nil) {
        do {
            try lockWritableDisk()
            let configuration = try makeConfiguration(restore:restore)
            snapshotConfiguration = configuration
            vm = VZVirtualMachine(configuration:configuration)
            vm.delegate = self
            if config.window ?? false { showWindow() }
            readCommands()
            if restore != nil {
                installing = true
                let installer = VZMacOSInstaller(virtualMachine:vm, restoringFromImageAt:URL(fileURLWithPath:config.restore_image!))
                self.installer = installer
                progressObservation = installer.progress.observe(\.fractionCompleted, options:[.new]) { progress, _ in
                    emit(["event":"installation_progress","fraction":progress.fractionCompleted])
                }
                installer.install { result in
                    self.installing = false
                    switch result {
                    case .success: emit(["event":"installation_complete","state":stateName(self.vm.state)]); self.reportState()
                    case .failure(let error): fail(error.localizedDescription)
                    }
                }
            } else if let saved = snapshotRestore {
                guard config.guest == "linux", let context = linuxSnapshot else { throw NativeError(message:"Saved-state restore requires Linux and persistent machine identity") }
                // Never cold boot a restore candidate. It stays stopped until
                // the state and actual boot/config identity have been checked.
                snapshotBusy = true; snapshotCaptureID = saved.captureID
                detachSnapshotInput()
                snapshotIO.async {
                    do {
                        _ = try context.inspect(configuration:configuration,diskDescriptor:self.lockedDiskDescriptor)
                        let input = try context.restoreInput(saved)
                        DispatchQueue.main.async {
                            self.vm.restoreMachineStateFrom(url:URL(fileURLWithPath:input.path)) { error in
                                if let error { fail("Linux saved-state restore failed: \(error.localizedDescription)") }
                                guard self.vm.state == .paused else { fail("Linux restore did not leave the VM paused") }
                                do { try input.verify(); try self.verifySnapshotDisk() }
                                catch { fail(error.localizedDescription) }
                                self.snapshotBusy = false
                                emit(["event":"snapshot_restored","state":"paused","capture_id":saved.captureID,
                                      "configuration_sha256":saved.configurationSHA256,"vm_id":self.config.id,
                                      "restored":true,"guest_continuation_verified":false])
                                self.reportState()
                                self.retireDisconnectedOwnerIfIdle()
                            }
                        }
                    } catch { fail(error.localizedDescription) }
                }
            } else if let path = machineStateRestore {
                restoreContainerState(path:path)
            } else {
                vm.start { result in
                    switch result {
                    case .success: self.reportState(); self.retireDisconnectedOwnerIfIdle()
                    case .failure(let error): fail(error.localizedDescription)
                    }
                }
            }
        } catch { fail(error.localizedDescription) }
    }
    func reportState() { emit(["event":"state","state":stateName(vm.state),"vm_id":config.id]) }
    func showWindow() {
        if let window { focusWindow(window); return }
        let view: VZVirtualMachineView
        if let keyEventLog {
            let loggingView = KeyEventLoggingVMView()
            loggingView.keyEventLog = keyEventLog; view = loggingView
        } else { view = VZVirtualMachineView() }
        view.virtualMachine = vm
        view.capturesSystemKeys = true
        view.automaticallyReconfiguresDisplay = true
        let window = NSWindow(contentRect:NSRect(x:0,y:0,width:1280,height:720),styleMask:[.titled,.closable,.miniaturizable,.resizable],backing:.buffered,defer:false)
        window.title = "Veynode — \(config.id)"
        window.contentView = view
        window.isReleasedWhenClosed = false
        window.delegate = self
        self.window = window
        window.center()
        if personInputMonitor == nil {
            let kinds: NSEvent.EventTypeMask = [.keyDown, .leftMouseDown, .rightMouseDown, .otherMouseDown, .scrollWheel, .leftMouseDragged]
            personInputMonitor = NSEvent.addLocalMonitorForEvents(matching: kinds) { [weak self] event in
                if let self, let window = self.window, event.window === window, Date().timeIntervalSince(self.lastPersonInput) >= 1 {
                    self.lastPersonInput = Date()
                    emit(["event":"person_input","vm_id":self.config.id])
                }
                return event
            }
        }
        emit(["event":"window_opened","vm_id":config.id])
        focusWindow(window)
    }
    func focusWindow(_ window: NSWindow) {
        NSApplication.shared.setActivationPolicy(.regular)
        NSApplication.shared.activate(ignoringOtherApps:true)
        window.makeKeyAndOrderFront(nil)
        if let view = window.contentView as? VZVirtualMachineView {
            window.makeFirstResponder(view)
        }
    }
    func windowWillClose(_ notification: Notification) { window = nil; emit(["event":"window_closed","vm_id":config.id]) }
    func stopGuestStreamBridge(_ completion: @escaping () -> Void) {
        guestStreamBinding = nil
        guard let bridge = guestStreamBridge else { completion(); return }
        // Retain the bridge while its own serial queue closes owned descriptors.
        // No descriptor is closed from the main/UI queue.
        bridge.stop { self.guestStreamBridge = nil; completion() }
    }
    func controlOwnerDisconnected() {
        guard !ownerDisconnected else { return }
        ownerDisconnected = true
        suppressWorkerOutput()
        detachSnapshotInput()
        retireDisconnectedOwnerIfIdle()
    }
    func retireDisconnectedOwnerIfIdle() {
        guard ownerDisconnected, !ownerRetiring, !snapshotBusy else { return }
        if installing { installer?.progress.cancel(); exit(1) }
        guard vm.state == .stopped || vm.canStop else {
            DispatchQueue.main.asyncAfter(deadline:.now() + .milliseconds(50)) { self.retireDisconnectedOwnerIfIdle() }
            return
        }
        ownerRetiring = true; snapshotRetiring = true
        stopGuestStreamBridge {
            func finished(_ error: Error?) {
                guard error == nil, self.vm.state == .stopped, !self.snapshotBusy else { exit(1) }
                do { try self.snapshotRetirement?.persist(); exit(0) }
                catch { exit(1) } // Without a receipt the host retains its fence.
            }
            if self.vm.state == .stopped { finished(nil) }
            else { self.vm.stop { error in finished(error) } }
        }
    }
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        reportState(); if !installing && !snapshotRetiring { stopGuestStreamBridge { exit(0) } }
    }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        stopGuestStreamBridge { fail(error.localizedDescription) }
    }
    func readCommands() {
        DispatchQueue.global(qos:.userInitiated).async {
            var pending = Data()
            var oversized = false
            var buffer = [UInt8](repeating:0,count:4096)
            while true {
                let count = read(STDIN_FILENO, &buffer, buffer.count)
                if count < 0 && errno == EINTR { continue }
                if count <= 0 { break }
                for byte in buffer.prefix(count) {
                    if byte == 10 {
                        if !oversized && !pending.isEmpty {
                            do {
                                guard let object = try JSONSerialization.jsonObject(with:pending) as? [String:Any] else { throw NativeError(message:"Expected object") }
                                DispatchQueue.main.async { self.command(object) }
                            } catch { emit(["error":["code":"invalid_request","message":error.localizedDescription]]) }
                        }
                        pending.removeAll(keepingCapacity:true)
                        oversized = false
                    } else if !oversized {
                        if pending.count == 1024 * 1024 {
                            emit(["error":["code":"request_too_large","message":"Command exceeds 1 MiB"]])
                            pending.removeAll(keepingCapacity:true)
                            oversized = true
                        } else { pending.append(byte) }
                    }
                }
            }
            DispatchQueue.main.async {
                self.controlOwnerDisconnected()
            }
        }
    }
    func guestRequest(_ object: [String:Any]) {
        let id = object["id"] ?? NSNull()
        func reject(_ code: String, _ message: String) { emit(["id":id,"error":["code":code,"message":message]]) }
        guard vm.state == .running, let socket = vm.socketDevices.first as? VZVirtioSocketDevice else { reject("invalid_state","VM must be running with a virtio socket"); return }
        if let sharing = screenSharing, let request = object["request"] as? [String:Any], let method = request["method"] as? String, ScreenSharingGuest.handles(method) {
            sharing.handle(request) { envelope in emit(["id":id,"result":envelope]) }
            return
        }
        guard guestRequests.count < 16 else { reject("busy","Too many concurrent guest requests"); return }
        guard let request = object["request"] as? [String:Any], let requestID = request["id"], let method = request["method"] as? String else {
            reject("invalid_request","Expected guest request with id/method"); return
        }
        let driver = method == "driver_rpc"
        var milliseconds = guestRequestSeconds * 1000
        if driver {
            guard let arguments = request["arguments"] as? [String:Any], arguments["frame"] is [String:Any],
                  let requested = (arguments["deadline_ms"] as? NSNumber)?.intValue, requested > 0 else {
                reject("invalid_request","driver_rpc needs arguments.frame and a positive arguments.deadline_ms"); return
            }
            // A little over the agent's own deadline so the agent's error reply wins the race.
            milliseconds = min(requested, driverMaxDeadlineMilliseconds) + 2000
        }
        guard let data = guestRequestData(request, limit: driver ? driverRequestLimit : guestRequestLimit) else {
            reject("invalid_request", driver ? "driver_rpc request exceeds 1 MiB" : "Expected guest request within a 64 KiB line including newline"); return
        }
        let key = UUID()
        guestRequests.insert(key)
        guestIOActive += 1
        let deadline = DispatchTime.now() + .milliseconds(milliseconds)
        func finish(_ response:[String:Any]) {
            guard self.guestRequests.remove(key) != nil else { return }
            emit(response)
        }
        DispatchQueue.main.asyncAfter(deadline:deadline) { finish(["id":id,"error":["code":"timeout","message":"Guest request exceeded \(milliseconds / 1000) seconds"]]) }
        socket.connect(toPort:5000) { result in
            guard self.guestRequests.contains(key) else {
                if case .success(let connection) = result { connection.close() }
                self.guestIOActive -= 1
                return
            }
            switch result {
            case .failure(let error):
                self.guestIOActive -= 1
                finish(["id":id,"error":["code":"guest_unavailable","message":error.localizedDescription]])
            case .success(let connection):
                DispatchQueue.global(qos:.userInitiated).async {
                    defer {
                        connection.close()
                        DispatchQueue.main.async { self.guestIOActive -= 1 }
                    }
                    let fd = connection.fileDescriptor
                    _ = fcntl(fd,F_SETFL,fcntl(fd,F_GETFL) | O_NONBLOCK)
                    var noSignal:Int32 = 1
                    _ = setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&noSignal,socklen_t(MemoryLayout<Int32>.size))
                    func ready(_ events:Int16) throws {
                        while true {
                            let now = DispatchTime.now().uptimeNanoseconds
                            guard now < deadline.uptimeNanoseconds else { throw NativeError(message:"Guest request timed out") }
                            var descriptor = pollfd(fd:fd,events:events,revents:0)
                            let milliseconds = Int32(min((deadline.uptimeNanoseconds-now)/1_000_000,1000))
                            let result = poll(&descriptor,1,milliseconds)
                            if result < 0 && errno == EINTR { continue }
                            if result < 0 { throw NativeError(message:"Guest socket poll failed") }
                            if result == 0 { continue }
                            if descriptor.revents & events != 0 { return }
                            throw NativeError(message:"Guest socket disconnected")
                        }
                    }
                    do {
                        try data.withUnsafeBytes { bytes in
                            var offset = 0
                            while offset < bytes.count {
                                try ready(Int16(POLLOUT))
                                let count = write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
                                if count < 0 && (errno == EINTR || errno == EAGAIN) { continue }
                                guard count > 0 else { throw NativeError(message:"Guest socket write failed") }
                                offset += count
                            }
                        }
                        var response = Data()
                        var buffer = [UInt8](repeating:0,count:4096)
                        var complete = false
                        while !complete {
                            try ready(Int16(POLLIN))
                            let count = read(fd,&buffer,buffer.count)
                            if count < 0 && (errno == EINTR || errno == EAGAIN) { continue }
                            guard count > 0 else { throw NativeError(message:"Guest closed socket without complete response") }
                            for byte in buffer.prefix(count) {
                                if byte == 10 { complete = true; break }
                                guard response.count < 1024 * 1024 else { throw NativeError(message:"Guest response exceeds 1 MiB") }
                                response.append(byte)
                            }
                        }
                        guard let decoded = try JSONSerialization.jsonObject(with:response) as? [String:Any],
                              let responseID = decoded["id"] as? NSObject, let expectedID = requestID as? NSObject,
                              responseID.isEqual(expectedID), (decoded["result"] != nil) != (decoded["error"] != nil) else { throw NativeError(message:"Invalid guest response envelope or id") }
                        DispatchQueue.main.async { finish(["id":id,"result":decoded]) }
                    } catch {
                        DispatchQueue.main.async { finish(["id":id,"error":["code":"guest_transport_error","message":error.localizedDescription]]) }
                    }
                }
            }
        }
    }
    func command(_ object:[String:Any]) {
        guard !ownerDisconnected else { return }
        let id = object["id"] ?? NSNull()
        func ok(_ result:Any) { emit(["id":id,"result":result]) }
        func error(_ code:String,_ message:String) { emit(["id":id,"error":["code":code,"message":message]]) }
        if config.guest == "linux", (snapshotBusy || snapshotCaptureID != nil),
           !["status","snapshot_support","snapshot_resume","snapshot_retire","close_window"].contains(object["method"] as? String ?? "") {
            error("snapshot_busy","Linux snapshot transaction holds this VM paused; other commands are fenced"); return
        }
        switch object["method"] as? String {
        case "snapshot_support", "snapshot_capture", "snapshot_resume", "snapshot_retire":
            linuxSnapshotCommand(object)
        case "machine_state_save": machineStateSave(object)
        case "guest_request": guestRequest(object)
        case "guest_stream_probe":
            guard Set(object.keys).isSubset(of:["id","method"]) else { error("invalid_request","Expected only id and method"); return }
            guard !installing, vm.state == .running, let socket = vm.socketDevices.first as? VZVirtioSocketDevice else {
                ok(GuestStreamProbe.response("invalid_state")); return
            }
            guestStreamProbe.run(connector:{ completion in
                socket.connect(toPort:5001) { result in
                    switch result {
                    case .failure(let failure): completion(.failure(failure))
                    case .success(let connection):
                        completion(.success {
                            // EOF releases the Guest parser slot without a start request.
                            _ = shutdown(connection.fileDescriptor,SHUT_WR)
                            connection.close()
                        })
                    }
                }
            }, completion:{ response in ok(response) })
        case "start_guest_stream_bridge":
            guard Set(object.keys).isSubset(of:["id","method","socket_path","worker_instance","bridge_instance"]), let path = object["socket_path"] as? String else {
                error("invalid_request","Expected only id, method and absolute socket_path"); return
            }
            let workerNonce=object["worker_instance"] as? String,bridgeNonce=object["bridge_instance"] as? String
            func nonce(_ v:String?)->Bool {guard let v else{return false};return v.utf8.count==64 && v.utf8.allSatisfy{(48...57).contains($0)||(97...102).contains($0)}}
            let hasBinding=object["worker_instance"] != nil || object["bridge_instance"] != nil
            guard !hasBinding || (nonce(workerNonce) && nonce(bridgeNonce)) else {error("invalid_request","Paired lowercase256-bit binding nonces required");return}
            guard !installing, vm.state == .running, vm.socketDevices.first is VZVirtioSocketDevice else {
                error("invalid_state","VM must be running with a virtio socket"); return
            }
            guard guestStreamBridge == nil else { error("busy","Guest stream bridge already exists or is stopping"); return }
            do {
                let bridge = try GuestStreamBridge(path:path, connector:{ [weak self] completion in
                    DispatchQueue.main.async {
                        guard let self, self.vm.state == .running,
                              let socket = self.vm.socketDevices.first as? VZVirtioSocketDevice else {
                            completion(.failure(NativeError(message:"Guest stream VM is not running"))); return
                        }
                        // Fixed product diagnostic endpoint; never an arbitrary port/CID proxy.
                        socket.connect(toPort:5001) { result in
                            switch result {
                            case .failure(let failure): completion(.failure(failure))
                            case .success(let connection):
                                let descriptor = dup(connection.fileDescriptor)
                                guard descriptor >= 0 else {
                                    connection.close(); completion(.failure(NativeError(message:"Cannot retain Guest stream descriptor"))); return
                                }
                                completion(.success(GuestStreamEndpoint(fd:descriptor, onClose:{
                                    DispatchQueue.main.async { connection.close() }
                                })))
                            }
                        }
                    }
                }, event:{ reason in emit(["event":"guest_stream_bridge_connection_closed","reason":reason]) })
                guestStreamBridge = bridge; bridge.start()
                var response:[String:Any]=["listening":true,"guest_connected":false,"socket_path":path,"guest_port":5001,"scope":"trusted-local-diagnostic"]
                if let workerNonce,let bridgeNonce {
                    guestStreamBinding=(workerNonce,bridgeNonce)
                    response["binding"]=try bridge.binding(vmID:config.id,workerInstance:workerNonce,bridgeInstance:bridgeNonce)
                }
                ok(response)
            } catch let failure { error("bridge_error",failure.localizedDescription) }
        case "guest_stream_bridge_binding":
            guard Set(object.keys).isSubset(of:["id","method","worker_instance","bridge_instance"]),
                  let identity=guestStreamBinding, let bridge=guestStreamBridge,
                  object["worker_instance"] as? String==identity.worker,object["bridge_instance"] as? String==identity.bridge,
                  vm.state == .running else {error("binding_unavailable","No matching live bound bridge");return}
            do {ok(["binding":try bridge.binding(vmID:config.id,workerInstance:identity.worker,bridgeInstance:identity.bridge)])}
            catch _ {error("binding_unavailable","Bridge socket incarnation unavailable")}
        case "stop_guest_stream_bridge":
            guard Set(object.keys).isSubset(of:["id","method"]) else { error("invalid_request","Expected only id and method"); return }
            stopGuestStreamBridge { ok(["stopped":true]) }
        case "status":
            var status: [String:Any] = ["state":stateName(vm.state),"installing":installing,"window_open":window != nil,"vm_id":config.id]
            if config.guest == "linux", linuxSnapshot != nil {
                status["snapshot_busy"] = snapshotBusy
                status["snapshot_capture_id"] = (snapshotCaptureID as Any?) ?? NSNull()
            }
            ok(status)
        case "shutdown":
            guard !installing, vm.canRequestStop else { error("invalid_state","Guest cannot accept shutdown in current state"); return }
            do { try vm.requestStop(); ok(["requested":true]) } catch let e { error("backend_error",e.localizedDescription) }
        case "stop":
            guard !installing, vm.canStop else { error("invalid_state","Cannot stop in current state"); return }
            vm.stop { e in
                if let e { error("backend_error",e.localizedDescription) }
                else { self.stopGuestStreamBridge { ok(["state":"stopped"]); self.reportState(); exit(0) } }
            }
        case "show_window": showWindow(); ok(["window_open":true])
        case "close_window": window?.close(); ok(["window_open":false,"state":stateName(vm.state)])
        case "serial_write":
            guard let input = serialInput else { error("unsupported","serial_log not configured"); return }
            guard let encoded = object["data"] as? String, let data = Data(base64Encoded:encoded), data.count <= 65536 else { error("invalid_request","Expected base64 data up to 64 KiB"); return }
            serialWritesActive += 1
            serialWriteQueue.async {
                defer { DispatchQueue.main.async { self.serialWritesActive -= 1 } }
                let deadline = DispatchTime.now().uptimeNanoseconds + 5_000_000_000
                let fd = input.fileHandleForWriting.fileDescriptor
                do {
                    try data.withUnsafeBytes { bytes in
                        var offset = 0
                        while offset < bytes.count {
                            guard DispatchTime.now().uptimeNanoseconds < deadline else { throw NativeError(message:"Serial input timed out") }
                            var descriptor = pollfd(fd:fd,events:Int16(POLLOUT),revents:0)
                            let result = poll(&descriptor,1,100)
                            if result < 0 && errno == EINTR { continue }
                            if result == 0 { continue }
                            guard result > 0, descriptor.revents & Int16(POLLOUT) != 0 else { throw NativeError(message:"Serial input disconnected") }
                            let count = write(fd,bytes.baseAddress!.advanced(by:offset),bytes.count-offset)
                            if count < 0 && (errno == EINTR || errno == EAGAIN) { continue }
                            guard count > 0 else { throw NativeError(message:"Serial input failed") }
                            offset += count
                        }
                    }
                    ok(["bytes":data.count])
                } catch let e { error("backend_error",e.localizedDescription) }
            }
        default: error("unsupported","Unsupported native backend method")
        }
    }
}

let args = CommandLine.arguments
if args.count < 2 { fail("Usage: vm-apple probe | latest-restore | run --config FILE | install --config FILE") }
switch args[1] {
#if JSON_FRAMING_TESTS
case "framing-test": jsonFramingSelfTest()
#endif
#if KEY_EVENT_LOG_TESTS
case "key-event-log-test": keyEventLogSelfTest()
#endif
case "probe":
    emit(["backend":"apple-virtualization","protocol_version":1,"architecture":"arm64","virtualization_supported":VZVirtualMachine.isSupported,"host_os":ProcessInfo.processInfo.operatingSystemVersionString,"memory_bytes":ProcessInfo.processInfo.physicalMemory,"logical_cpus":ProcessInfo.processInfo.processorCount,"limits":["min_cpus":VZVirtualMachineConfiguration.minimumAllowedCPUCount,"max_cpus":VZVirtualMachineConfiguration.maximumAllowedCPUCount,"min_memory_bytes":VZVirtualMachineConfiguration.minimumAllowedMemorySize,"max_memory_bytes":VZVirtualMachineConfiguration.maximumAllowedMemorySize],"guests":["linux":["boot":"direct-kernel","uefi_boot":"VZEFIBootLoader + per-VM VZEFIVariableStore; installer ISO as read-only USB mass storage","display":"VZVirtioGraphicsDevice + VZVirtualMachineView","gpu_3d":"unverified; virtio scanout is not a 3D claim","headless_lifecycle":true],"macos":["boot":"VZMacOSBootLoader","install":"VZMacOSInstaller","display":"VZMacGraphicsDevice + VZVirtualMachineView","gpu_3d":"paravirtualized Metal (AppleParavirtGPU); compute and game-like rendering verified in a macOS 26.6 guest; 60 Hz display","headless_lifecycle":true],"windows":["supported":false]],"remote_capture":false,"snapshot_restore":"unverified","native_display":true,"guest_transport":"virtio-vsock","entitlement_required":"com.apple.security.virtualization"])
case "latest-restore":
    VZMacOSRestoreImage.fetchLatestSupported { result in
        switch result {
        case .success(let image):
            var output:[String:Any] = ["url":image.url.absoluteString,"build":image.buildVersion,"supported":image.isSupported,"version":"\(image.operatingSystemVersion.majorVersion).\(image.operatingSystemVersion.minorVersion).\(image.operatingSystemVersion.patchVersion)"]
            if let requirements = image.mostFeaturefulSupportedConfiguration { output["minimum_cpus"] = requirements.minimumSupportedCPUCount; output["minimum_memory_bytes"] = requirements.minimumSupportedMemorySize }
            emit(output); exit(0)
        case .failure(let error): fail(error.localizedDescription)
        }
    }
    RunLoop.main.run()
case "run", "install":
    guard args.count >= 4, args[2] == "--config", (args.count - 4) % 2 == 0 else { fail("Expected --config FILE and optional paired arguments") }
    do {
        var options: [String:String] = [:]
        let known = Set(["--log-key-events","--restore-machine-state","--restore-state","--restore-state-sha256","--restore-config-sha256","--restore-capture-id"])
        for index in stride(from:4,to:args.count,by:2) {
            guard known.contains(args[index]), options[args[index]] == nil else { throw NativeError(message:"Unknown or duplicate worker argument") }
            options[args[index]] = args[index+1]
        }
        let config = try JSONDecoder().decode(Config.self,from:Data(contentsOf:URL(fileURLWithPath:args[3])))
        let restoreFields = ["--restore-state","--restore-state-sha256","--restore-config-sha256","--restore-capture-id"]
        let restoreCount = restoreFields.filter { options[$0] != nil }.count
        guard restoreCount == 0 || (restoreCount == 4 && args[1] == "run" && config.guest == "linux") else { throw NativeError(message:"Linux saved-state restore requires all four restore bindings") }
        let saved = restoreCount == 0 ? nil : LinuxSnapshotRestore(path:options[restoreFields[0]]!,sha256:options[restoreFields[1]]!,
            configurationSHA256:options[restoreFields[2]]!,captureID:options[restoreFields[3]]!)
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        var runner: Runner?
        func launch(_ keyEventLog: KeyEventLogQueue?) throws {
            let worker = Runner(config,keyEventLog:keyEventLog,snapshotRestore:saved)
            if let path = options["--restore-machine-state"] {
                guard args[1] == "run", config.guest == "linux", saved == nil else { fail("--restore-machine-state is for a Linux run without snapshot restore") }
                worker.machineStateRestore = path
            }
            runner = worker; app.delegate = worker
            setWorkerOutputDisconnectHandler { [weak worker] in worker?.controlOwnerDisconnected() }
            if args[1] == "install" {
                guard config.guest == "macos" else { fail("install requires macos guest") }
                let restoreURL = URL(fileURLWithPath:try required(config.restore_image,"restore_image"))
                VZMacOSRestoreImage.load(from:restoreURL) { result in
                    DispatchQueue.main.async {
                        switch result {
                        case .success(let image): worker.start(restore:image)
                        case .failure(let error): fail(error.localizedDescription)
                        }
                    }
                }
            } else { worker.start() }
        }
        if let logPath = options["--log-key-events"] {
            KeyEventLogQueue.open(path:logPath) { result in
                DispatchQueue.main.async {
                    do { try launch(result.get()) }
                    catch { fail(error.localizedDescription) }
                }
            }
        } else { try launch(nil) }
        app.run()
        withExtendedLifetime(runner) {}
    } catch { fail(error.localizedDescription) }
default: fail("Unknown command")
}
