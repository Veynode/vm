//go:build linux

package main

import (
	"errors"
	"fmt"
	"os"
	"syscall"
	"unsafe"
)

// Linux UAPI sockaddr_vm; fixed Linux ABI, not a host-specific networking struct.
type sockaddrVM struct {
	Family   uint16
	Reserved uint16
	Port     uint32
	CID      uint32
	Flags    uint8
	Zero     [3]uint8
}

var errNotHost = errors.New("vsock peer is not the host")

func vsockListen(port uint32, backlog int) (int, error) {
	fd, err := syscall.Socket(40, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		return -1, err
	}
	addr := sockaddrVM{Family: 40, Port: port, CID: 0xffffffff}
	_, _, e := syscall.RawSyscall(syscall.SYS_BIND, uintptr(fd), uintptr(unsafe.Pointer(&addr)), unsafe.Sizeof(addr))
	if e != 0 {
		syscall.Close(fd)
		return -1, e
	}
	if err = syscall.Listen(fd, backlog); err != nil {
		syscall.Close(fd)
		return -1, err
	}
	return fd, nil
}

// vsockAccept returns the next host (CID 2) connection; other peers are
// closed and reported as errNotHost.
func vsockAccept(fd int) (*os.File, error) {
	peer := sockaddrVM{}
	length := uint32(unsafe.Sizeof(peer))
	// Syscall6 (not RawSyscall) releases the Go P during blocking accept.
	conn, _, e := syscall.Syscall6(syscall.SYS_ACCEPT4, uintptr(fd), uintptr(unsafe.Pointer(&peer)), uintptr(unsafe.Pointer(&length)), syscall.SOCK_CLOEXEC, 0, 0)
	if e != 0 {
		return nil, e
	}
	if peer.CID != 2 || peer.Family != 40 {
		syscall.Close(int(conn))
		return nil, errNotHost
	}
	if err := syscall.SetNonblock(int(conn), true); err != nil {
		syscall.Close(int(conn))
		return nil, errNotHost
	}
	return os.NewFile(conn, fmt.Sprintf("vsock:%d", peer.CID)), nil
}

func listenVsock(allow, shutdown bool) error {
	fd, err := vsockListen(5000, 16)
	if err != nil {
		return err
	}
	defer syscall.Close(fd)
	slots := make(chan struct{}, 16)
	for {
		slots <- struct{}{}
		f, err := vsockAccept(fd)
		if err != nil {
			<-slots
			if errors.Is(err, syscall.EINTR) || errors.Is(err, errNotHost) {
				continue
			}
			return err
		}
		go func() { defer func() { <-slots }(); serve(f, allow, shutdown) }()
	}
}
