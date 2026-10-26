package main

import (
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
