//go:build linux

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"syscall"
	"time"
)

func listenSerial() error     { return listenSerialMode(false) }
func listenSerialExec() error { return listenSerialMode(true) }
func listenSerialMode(execution bool) error {
	f, err := openPrivatePort(serialPort, "veynode-serial")
	if err != nil {
		return err
	}
	defer f.Close()
	if execution {
		return serveSerialExec(f, commands)
	}
	return serveSerial(f)
}

// openPrivatePort opens a virtio port that only root can open.
func openPrivatePort(path, name string) (*os.File, error) {
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return nil, err
	}
	if filepath.Dir(resolved) != "/dev" {
		return nil, fmt.Errorf("serial device outside /dev")
	}
	before, err := os.Lstat(resolved)
	if err != nil {
		return nil, err
	}
	st, ok := before.Sys().(*syscall.Stat_t)
	if !ok || st.Uid != 0 || before.Mode()&os.ModeCharDevice == 0 || before.Mode().Perm() != 0600 {
		return nil, fmt.Errorf("serial device must be root-owned private 0600 character device")
	}
	fd, err := syscall.Open(resolved, syscall.O_RDWR|syscall.O_NONBLOCK|syscall.O_CLOEXEC|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return nil, err
	}
	f := os.NewFile(uintptr(fd), name)
	after, err := f.Stat()
	if err != nil || !os.SameFile(before, after) {
		f.Close()
		return nil, fmt.Errorf("serial device changed")
	}
	return f, nil
}

// serveDriverPort serves cua-driver frames on the QEMU driver port, when the
// VM has one, for the agent's lifetime. The port reads EOF while no host is
// connected, so it is reopened after a pause.
func serveDriverPort() {
	if _, err := os.Lstat(driverPort); err != nil {
		return
	}
	for {
		f, err := openPrivatePort(driverPort, "veynode-driver")
		if err != nil {
			fmt.Fprintln(os.Stderr, "driver port unavailable:", err)
			return
		}
		_ = serveDriverFrames(f)
		f.Close()
		time.Sleep(250 * time.Millisecond)
	}
}
