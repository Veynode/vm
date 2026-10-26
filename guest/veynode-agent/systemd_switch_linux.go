//go:build linux

package main

import (
	"fmt"
	"os"
	"syscall"
)

// handOffToSystemd makes the prepared container root the system root and
// execs the image's init as PID 1, like switch_root(8) leaving an initramfs.
// The guest service is not started here: systemd starts it as a unit.
// It returns only on failure.
func handOffToSystemd(config containerConfig) error {
	root := containerRootDir
	env := withDefaultEnv(config.Env, "/root")
	if err := prepareSystemdRuntime(root, "/veynode/agent", config, env); err != nil {
		return err
	}
	if config.Transport == "virtio-serial-exec" {
		// devtmpfs is shared with the new root; udev recreates these later.
		linkVirtioPorts()
	}
	// The initramfs mounts backing the overlay (/run/disk, /run/lower) and the
	// bind sources for /etc/{resolv.conf,hosts,hostname} stay attached to the
	// old root, which becomes unreachable but pinned, as with switch_root.
	if err := syscall.Chdir(root); err != nil {
		return err
	}
	if err := syscall.Mount(root, "/", "", syscall.MS_MOVE, ""); err != nil {
		return fmt.Errorf("move container root to /: %w", err)
	}
	if err := syscall.Chroot("."); err != nil {
		return fmt.Errorf("chroot: %w", err)
	}
	if err := syscall.Chdir("/"); err != nil {
		return err
	}
	marker("started", "1")
	_ = os.Stdout.Sync()
	// The kernel passes init only HOME and TERM; image ENV is for exec.
	err := syscall.Exec(config.Argv[0], config.Argv, []string{"HOME=/", "TERM=linux"})
	return fmt.Errorf("exec %s: %w", config.Argv[0], err)
}
