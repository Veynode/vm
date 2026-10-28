//go:build linux

package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"syscall"
	"time"
	"unsafe"
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
// connected; the agent waits for the next host on the reopened port without
// polling, so an idle Guest makes no wakeups for it.
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
		waited := awaitPortHost(f) == nil
		err = serveDriverFrames(f)
		f.Close()
		// Without the wait, or after anything but the host closing the port,
		// look again after a pause rather than spin.
		if !waited || !errors.Is(err, io.EOF) {
			time.Sleep(250 * time.Millisecond)
		}
	}
}

// awaitPortHost blocks until the host side of virtio port f is open. A port
// with no host polls as POLLHUP, and the driver wakes the port's waiters when
// the host opens it; the netpoller's edge-triggered registration turns that
// wakeup into a retry. An error means f cannot be waited on this way.
func awaitPortHost(f *os.File) error {
	raw, err := f.SyscallConn()
	if err != nil {
		return err
	}
	var failed error
	err = raw.Read(func(fd uintptr) bool {
		hung, e := pollHangup(int(fd))
		if e != nil {
			failed = e
			return true
		}
		return !hung
	})
	if err != nil {
		return err
	}
	return failed
}

// pollHangup reports, without blocking, whether fd polls as POLLHUP.
func pollHangup(fd int) (bool, error) {
	const pollHup = 0x10
	p := struct {
		fd      int32
		events  int16
		revents int16
	}{fd: int32(fd)}
	var zero syscall.Timespec
	for {
		_, _, e := syscall.Syscall6(syscall.SYS_PPOLL, uintptr(unsafe.Pointer(&p)), 1, uintptr(unsafe.Pointer(&zero)), 0, 0, 0)
		if e == syscall.EINTR {
			continue
		}
		if e != 0 {
			return false, e
		}
		return p.revents&pollHup != 0, nil
	}
}
