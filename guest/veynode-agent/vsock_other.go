//go:build !linux

package main

import "errors"

func listenVsock(bool, bool) error {
	return errors.New("vsock service currently supports Linux guests only")
}

func listenSerial() error { return errors.New("virtio serial requires Linux") }

func listenSerialExec() error { return errors.New("virtio serial execution requires Linux") }
