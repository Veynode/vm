//go:build linux || darwin

package main

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestXauthorityFIFORefusedWithoutWaitingForWriter(t *testing.T) {
	path := filepath.Join(t.TempDir(), "authority.fifo")
	if err := syscall.Mkfifo(path, 0600); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { _, err := readXauthority(path, 0); done <- err }()
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("FIFO authority accepted")
		}
	case <-time.After(time.Second):
		// Unblock the known old implementation before failing; no leaked goroutine
		// or persistent writer is allowed even in the intentional regression run.
		writer, err := os.OpenFile(path, os.O_WRONLY|syscall.O_NONBLOCK, 0)
		if err != nil {
			t.Fatal("cannot unblock owned FIFO reader:", err)
		}
		writer.Close()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Fatal("owned FIFO reader did not retire")
		}
		t.Fatal("Xauthority read waited for a FIFO writer outside the connection budget")
	}
}

func TestXauthorityRejectsNonregularFiles(t *testing.T) {
	for _, path := range []string{t.TempDir(), "/dev/null", "/dev/zero"} {
		if _, err := readXauthority(path, 0); err == nil {
			t.Errorf("nonregular authority accepted: %s", path)
		}
	}
}

func TestXauthorityRegularSymlinkAndSizeBound(t *testing.T) {
	cookie := bytes.Repeat([]byte{7}, 16) // synthetic test cookie, not a credential
	data := binary.BigEndian.AppendUint16(nil, 256)
	for _, field := range [][]byte{[]byte("guest"), []byte("0"), []byte("MIT-MAGIC-COOKIE-1"), cookie} {
		data = binary.BigEndian.AppendUint16(data, uint16(len(field)))
		data = append(data, field...)
	}
	dir := t.TempDir()
	regular, link := filepath.Join(dir, "authority"), filepath.Join(dir, "authority-link")
	if err := os.WriteFile(regular, data, 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(regular, link); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{regular, link} {
		got, err := readXauthority(path, 0)
		if err != nil || !bytes.Equal(got, cookie) {
			t.Fatal("regular authority rejected", err)
		}
	}
	// A valid leading entry must not evade the existing whole-file byte bound.
	data = append(data, make([]byte, xMaxAuthorityFile+1-len(data))...)
	if err := os.WriteFile(regular, data, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := readXauthority(link, 0); err == nil {
		t.Fatal("oversized authority accepted")
	}
}
