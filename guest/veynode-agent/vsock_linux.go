//go:build linux

package main

import (
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

func listenVsock(allow, shutdown bool) error {
	fd, err := syscall.Socket(40, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		return err
	}
	defer syscall.Close(fd)
	addr := sockaddrVM{Family: 40, Port: 5000, CID: 0xffffffff}
	_, _, e := syscall.RawSyscall(syscall.SYS_BIND, uintptr(fd), uintptr(unsafe.Pointer(&addr)), unsafe.Sizeof(addr))
	if e != 0 {
		return e
	}
	if err = syscall.Listen(fd, 16); err != nil {
		return err
	}
	slots := make(chan struct{}, 16)
	for {
		slots <- struct{}{}
		peer := sockaddrVM{}
		length := uint32(unsafe.Sizeof(peer))
		// Syscall6 (not RawSyscall) releases the Go P during blocking accept.
		conn, _, e := syscall.Syscall6(syscall.SYS_ACCEPT4, uintptr(fd), uintptr(unsafe.Pointer(&peer)), uintptr(unsafe.Pointer(&length)), syscall.SOCK_CLOEXEC, 0, 0)
		if e != 0 {
			<-slots
			if e == syscall.EINTR {
				continue
			}
			return e
		}
		if peer.CID != 2 || peer.Family != 40 {
			syscall.Close(int(conn))
			<-slots
			continue
		}
		if err = syscall.SetNonblock(int(conn), true); err != nil {
			syscall.Close(int(conn))
			<-slots
			continue
		}
		f := os.NewFile(conn, fmt.Sprintf("vsock:%d", peer.CID))
		go func() { defer func() { <-slots }(); serve(f, allow, shutdown) }()
	}
}
