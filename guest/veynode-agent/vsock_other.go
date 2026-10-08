//go:build !linux

package main

import "errors"

func listenVsock(bool, bool) error {
	return errors.New("vsock service currently supports Linux guests only")
}
