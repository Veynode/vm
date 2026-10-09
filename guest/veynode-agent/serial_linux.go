//go:build linux

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"syscall"
)

func listenSerial() error     { return listenSerialMode(false) }
func listenSerialExec() error { return listenSerialMode(true) }
func listenSerialMode(execution bool) error {
	resolved, err := filepath.EvalSymlinks(serialPort)
	if err != nil {
		return err
	}
	if filepath.Dir(resolved) != "/dev" {
		return fmt.Errorf("serial device outside /dev")
	}
	before, err := os.Lstat(resolved)
	if err != nil {
		return err
	}
	st, ok := before.Sys().(*syscall.Stat_t)
	if !ok || st.Uid != 0 || before.Mode()&os.ModeCharDevice == 0 || before.Mode().Perm() != 0600 {
		return fmt.Errorf("serial device must be root-owned private 0600 character device")
	}
	fd, err := syscall.Open(resolved, syscall.O_RDWR|syscall.O_NONBLOCK|syscall.O_CLOEXEC|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return err
	}
	f := os.NewFile(uintptr(fd), "veynode-serial")
	defer f.Close()
	after, err := f.Stat()
	if err != nil || !os.SameFile(before, after) {
		return fmt.Errorf("serial device changed")
	}
	if execution {
		return serveSerialExec(f, commands)
	}
	return serveSerial(f)
}
