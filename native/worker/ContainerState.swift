import Foundation
import Virtualization
import Darwin

// Warm checkpoints of Linux micro-VM containers (`veynodevm checkpoint`).
//
// Separate from LinuxSnapshot's transactional named snapshots: the host CLI
// owns the checkpoint directory. `machine_state_save` pauses the VM, saves
// RAM/CPU/device state with saveMachineStateTo, clones the writable disk while
// still paused (same quiesced interval) and resumes. A worker started with
// `--restore-machine-state PATH` restores that state instead of booting and
// resumes; any failure exits with an error and the host cold-boots instead.
// Restore needs the same configuration: a persistent generic machine
// identifier (`machine_identifier_file`) and a fixed `mac_address`.

func containerStateError(_ message: String) -> NativeError { NativeError(message:message) }

/// Load the container's persistent generic machine identifier, creating it
/// privately on first use.
func containerMachineIdentifier(path: String) throws -> VZGenericMachineIdentifier {
    guard path.hasPrefix("/") else { throw containerStateError("machine_identifier_file must be absolute") }
    var metadata = stat()
    if lstat(path,&metadata) == 0 {
        guard metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_uid == getuid(),
              metadata.st_mode & 0o077 == 0, metadata.st_size > 0, metadata.st_size <= 1024 else {
            throw containerStateError("machine_identifier_file must be a small private regular file")
        }
        guard let identifier = VZGenericMachineIdentifier(dataRepresentation:try Data(contentsOf:URL(fileURLWithPath:path))) else {
            throw containerStateError("Invalid machine identifier")
        }
        return identifier
    }
    guard errno == ENOENT else { throw containerStateError("Cannot inspect machine_identifier_file") }
    let identifier = VZGenericMachineIdentifier()
    let data = identifier.dataRepresentation
    let fd = open(path,O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,0o600)
    guard fd >= 0 else { throw containerStateError("Cannot create machine_identifier_file") }
    defer { close(fd) }
    let written = data.withUnsafeBytes { write(fd,$0.baseAddress,data.count) }
    guard written == data.count, fsync(fd) == 0 else { throw containerStateError("Cannot write machine_identifier_file") }
    return identifier
}

private func milliseconds(since start: DispatchTime) -> UInt64 {
    (DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1_000_000
}

/// A new leaf path in an existing private directory owned by this user.
private func newLeaf(_ path: String) throws {
    guard path.hasPrefix("/"), !path.utf8.contains(0) else { throw containerStateError("State paths must be absolute") }
    var metadata = stat()
    guard lstat(path,&metadata) != 0, errno == ENOENT else { throw containerStateError("State path already exists") }
    let parent = URL(fileURLWithPath:path).deletingLastPathComponent().path
    guard lstat(parent,&metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
          metadata.st_uid == getuid(), metadata.st_mode & 0o077 == 0 else {
        throw containerStateError("State directory must be a private directory owned by you")
    }
}

extension Runner {
    func machineStateSave(_ object: [String:Any]) {
        let id = object["id"] ?? NSNull()
        func reject(_ code: String, _ message: String) { emit(["id":id,"error":["code":code,"message":message]]) }
        guard config.guest == "linux", config.machine_identifier_file != nil, config.mac_address != nil, linuxSnapshot == nil else {
            reject("unsupported","Warm checkpoints need a Linux container with machine_identifier_file and mac_address"); return
        }
        guard Set(object.keys).isSubset(of:["id","method","path","disk_clone"]),
              let path = object["path"] as? String, let clone = object["disk_clone"] as? String else {
            reject("invalid_request","Expected path and disk_clone"); return
        }
        do { try newLeaf(path); try newLeaf(clone) } catch { reject("invalid_request",error.localizedDescription); return }
        guard !snapshotBusy, snapshotCaptureID == nil, vm.state == .running, vm.canPause else {
            reject("invalid_state","VM must be running and idle to save its state"); return
        }
        snapshotBusy = true
        let start = DispatchTime.now()
        var timing: [String:UInt64] = [:]
        func finish(_ failure: Error?) {
            if vm.state == .paused && vm.canResume {
                let resume = DispatchTime.now()
                vm.resume { result in
                    timing["resume_ms"] = milliseconds(since:resume)
                    self.snapshotBusy = false
                    self.reportState()
                    var message = failure?.localizedDescription
                    if case .failure(let error) = result { message = (message.map { $0 + "; " } ?? "") + "resume failed: \(error.localizedDescription)" }
                    if let message {
                        unlink(path); unlink(clone)
                        reject("machine_state_save_failed",message)
                    } else {
                        var size = stat()
                        _ = stat(path,&size)
                        timing["paused_ms"] = milliseconds(since:start)
                        emit(["id":id,"result":["state":"running","state_bytes":size.st_size,"timing":timing]])
                    }
                    self.retireDisconnectedOwnerIfIdle()
                }
            } else {
                snapshotBusy = false
                unlink(path); unlink(clone)
                reject("machine_state_save_failed",(failure?.localizedDescription ?? "VM left the paused state") + " (state \(stateName(vm.state)))")
                retireDisconnectedOwnerIfIdle()
            }
        }
        vm.pause { result in
            timing["pause_ms"] = milliseconds(since:start)
            if case .failure(let error) = result { finish(error); return }
            let save = DispatchTime.now()
            self.vm.saveMachineStateTo(url:URL(fileURLWithPath:path)) { error in
                timing["save_ms"] = milliseconds(since:save)
                if let error { finish(error); return }
                let copy = DispatchTime.now()
                do {
                    if let descriptor = self.lockedDiskDescriptor {
                        // Paused: push this inode's dirty pages, then clone from the held FD.
                        guard fsync(descriptor) == 0 else { throw containerStateError("Paused disk flush failed") }
                        let parent = URL(fileURLWithPath:clone).deletingLastPathComponent().path
                        let directory = open(parent,O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                        guard directory >= 0 else { throw containerStateError("Cannot open disk clone directory") }
                        defer { close(directory) }
                        guard fclonefileat(descriptor,directory,URL(fileURLWithPath:clone).lastPathComponent,0) == 0 else {
                            throw containerStateError("Paused disk clone failed: \(String(cString:strerror(errno)))")
                        }
                        guard chmod(clone,0o400) == 0 else { throw containerStateError("Cannot protect disk clone") }
                    }
                    guard chmod(path,0o400) == 0 else { throw containerStateError("Cannot protect saved state") }
                    timing["disk_clone_ms"] = milliseconds(since:copy)
                    finish(nil)
                } catch { finish(error) }
            }
        }
    }

    /// Restore a saved container state into this freshly configured VM and run it.
    func restoreContainerState(path: String) {
        guard config.guest == "linux", config.machine_identifier_file != nil, config.mac_address != nil, linuxSnapshot == nil else {
            fail("Warm restore needs a Linux container with machine_identifier_file and mac_address")
        }
        let start = DispatchTime.now()
        vm.restoreMachineStateFrom(url:URL(fileURLWithPath:path)) { error in
            if let error { fail("Saved container state restore failed: \(error.localizedDescription)") }
            let restored = milliseconds(since:start)
            self.vm.resume { result in
                if case .failure(let error) = result { fail("Restored container did not resume: \(error.localizedDescription)") }
                emit(["event":"machine_state_restored","vm_id":self.config.id,"restore_ms":restored,"total_ms":milliseconds(since:start)])
                self.reportState()
                self.retireDisconnectedOwnerIfIdle()
            }
        }
    }
}
