package main

import (
	"encoding/binary"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// layerOwnerFile records which container a writable layer belongs to.
const layerOwnerFile = ".veynodevm-container-id"

// layerIdentityFiles are per-machine identities that a forked layer must not
// share with its source. Removing them from the overlay upper directory
// reveals the image's own copy, if any; ensureMachineID then writes a fresh
// /etc/machine-id for systemd containers.
var layerIdentityFiles = []string{"etc/machine-id", "var/lib/dbus/machine-id"}

// adoptWritableLayer runs before the overlay is mounted. A layer last booted
// by another container id was cloned (`veynodevm fork`): drop its machine
// identity. It reports whether the layer was adopted from another container.
// Layers written before the owner file existed are claimed without changes.
func adoptWritableLayer(disk, upper, id string) (bool, error) {
	if id == "" {
		return false, nil
	}
	owner := filepath.Join(disk, layerOwnerFile)
	previous, err := os.ReadFile(owner)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return false, err
	}
	current := strings.TrimSpace(string(previous))
	if current == id {
		return false, nil
	}
	forked := current != ""
	if forked {
		for _, name := range layerIdentityFiles {
			path := filepath.Join(upper, name)
			// Only regular files and links: a directory here is not an identity.
			if info, err := os.Lstat(path); err == nil && !info.IsDir() {
				if err := os.Remove(path); err != nil {
					return forked, err
				}
			}
		}
	}
	return forked, os.WriteFile(owner, []byte(id+"\n"), 0o644)
}

// ext4Bytes is the size of the ext4 file system whose superblock (the 1 KiB
// at byte 1024 of the device) is `super`, or 0 when it is not ext4.
func ext4Bytes(super []byte) int64 {
	if len(super) < 0x158 || binary.LittleEndian.Uint16(super[0x38:]) != 0xef53 {
		return 0
	}
	blocks := uint64(binary.LittleEndian.Uint32(super[0x4:]))
	// INCOMPAT_64BIT: the high half of the block count is valid.
	if binary.LittleEndian.Uint32(super[0x60:])&0x80 != 0 {
		blocks |= uint64(binary.LittleEndian.Uint32(super[0x150:])) << 32
	}
	return int64(blocks << (10 + binary.LittleEndian.Uint32(super[0x18:])))
}
