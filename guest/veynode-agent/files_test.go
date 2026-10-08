package main

import (
	"bufio"
	"crypto/sha256"
	"encoding/base64"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestFilesRoundtripOwnershipNoReplaceAndExpiry(t *testing.T) {
	dir := t.TempDir()
	if err := os.Chmod(dir, 0700); err != nil {
		t.Fatal(err)
	}
	m, err := newFileManager(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer m.close()
	owner := strings.Repeat("a", 64)
	other := strings.Repeat("b", 64)
	body := []byte("你好\x00binary")
	digest := fmt.Sprintf("%x", sha256.Sum256(body))
	begin := func(name string) string {
		t.Helper()
		reply := m.handle(request{ID: "1", Method: "file_upload_begin", Owner: owner, Path: name, Size: int64(len(body)), SHA256: digest})
		if reply.Error != nil {
			t.Fatal(reply.Error)
		}
		return reply.Result.(map[string]any)["token"].(string)
	}
	token := begin("hello.bin")
	if reply := m.handle(request{ID: "2", Method: "file_upload_write", Owner: other, Token: token, Data: base64.StdEncoding.EncodeToString(body)}); reply.Error == nil {
		t.Fatal("other owner wrote chunk")
	}
	if reply := m.handle(request{ID: "3", Method: "file_upload_write", Owner: owner, Token: token, Data: base64.StdEncoding.EncodeToString(body)}); reply.Error != nil {
		t.Fatal(reply.Error)
	}
	if _, err := os.Stat(filepath.Join(dir, "hello.bin")); !os.IsNotExist(err) {
		t.Fatal("upload published before commit")
	}
	if reply := m.handle(request{ID: "4", Method: "file_upload_commit", Owner: owner, Token: token}); reply.Error != nil {
		t.Fatal(reply.Error)
	}
	if actual, err := os.ReadFile(filepath.Join(dir, "hello.bin")); err != nil || string(actual) != string(body) {
		t.Fatal("upload content differs", err)
	}
	read := m.handle(request{ID: "5", Method: "file_download_begin", Owner: owner, Path: "hello.bin"})
	if read.Error != nil {
		t.Fatal(read.Error)
	}
	download := read.Result.(map[string]any)["token"].(string)
	data := m.handle(request{ID: "6", Method: "file_download_read", Owner: owner, Token: download})
	if data.Error != nil {
		t.Fatal(data.Error)
	}
	if data.Result.(map[string]any)["data"] != base64.StdEncoding.EncodeToString(body) {
		t.Fatal("download differs")
	}
	token = begin("collision")
	if reply := m.handle(request{ID: "7", Method: "file_upload_write", Owner: owner, Token: token, Data: base64.StdEncoding.EncodeToString(body)}); reply.Error != nil {
		t.Fatal(reply.Error)
	}
	if err := os.WriteFile(filepath.Join(dir, "collision"), []byte("existing"), 0600); err != nil {
		t.Fatal(err)
	}
	if reply := m.handle(request{ID: "8", Method: "file_upload_commit", Owner: owner, Token: token}); reply.Error == nil || reply.Error.Code != "already_exists" {
		t.Fatal(reply)
	}
	if actual, _ := os.ReadFile(filepath.Join(dir, "collision")); string(actual) != "existing" {
		t.Fatal("existing overwritten")
	}
	token = begin("expire")
	now := time.Now().Add(61 * time.Second)
	m.now = func() time.Time { return now }
	if reply := m.handle(request{ID: "9", Method: "file_upload_commit", Owner: owner, Token: token}); reply.Error == nil {
		t.Fatal("expired transfer accepted")
	}
	if files, _ := filepath.Glob(filepath.Join(dir, ".veynode-staging", ".veynode-upload-*")); len(files) != 0 {
		t.Fatal("partial files leaked", files)
	}
	token = begin("closed")
	m.handle(request{ID: "10", Method: "file_session_end", Owner: owner})
	if reply := m.handle(request{ID: "11", Method: "file_upload_commit", Owner: owner, Token: token}); reply.Error == nil {
		t.Fatal("closed session commit")
	}
	if reply := m.handle(request{ID: "12", Method: "file_upload_begin", Owner: owner, Path: "late", Size: 0, SHA256: fmt.Sprintf("%x", sha256.Sum256(nil))}); reply.Error == nil {
		t.Fatal("late start after cleanup")
	}
}
func TestFilesRejectEscapeSymlinksHardlinksAndNonregular(t *testing.T) {
	dir := t.TempDir()
	if err := os.Chmod(dir, 0700); err != nil {
		t.Fatal(err)
	}
	m, err := newFileManager(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer m.close()
	owner := strings.Repeat("a", 64)
	outside := filepath.Join(t.TempDir(), "secret")
	if err = os.WriteFile(outside, []byte("secret"), 0600); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(outside, filepath.Join(dir, "link")); err != nil {
		t.Fatal(err)
	}
	if err = os.Link(outside, filepath.Join(dir, "hardlink")); err != nil {
		t.Fatal(err)
	}
	if err = os.Mkdir(filepath.Join(dir, "directory"), 0700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"../secret", "/etc/passwd", "link", "link/", "hardlink", "directory", ".veynode-lock"} {
		reply := m.handle(request{ID: "1", Method: "file_download_begin", Owner: owner, Path: name})
		if reply.Error == nil {
			t.Fatalf("unsafe name accepted: %s", name)
		}
	}
	if reply := m.handle(request{ID: "2", Method: "file_upload_begin", Owner: owner, Path: "large", Size: fileMaximum + 1, SHA256: strings.Repeat("0", 64)}); reply.Error == nil {
		t.Fatal("oversized upload accepted")
	}
}

func TestFilesSymlinkReplacementRaceNeverReadsOutside(t *testing.T) {
	dir := t.TempDir()
	if err := os.Chmod(dir, 0700); err != nil {
		t.Fatal(err)
	}
	m, err := newFileManager(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer m.close()
	secret := filepath.Join(t.TempDir(), "secret")
	if err = os.WriteFile(secret, []byte("outside-secret"), 0600); err != nil {
		t.Fatal(err)
	}
	stop := make(chan struct{})
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			select {
			case <-stop:
				return
			default:
			}
			temporary := filepath.Join(dir, "swap")
			_ = os.Remove(temporary)
			_ = os.Symlink(secret, temporary)
			_ = os.Rename(temporary, filepath.Join(dir, "target"))
			_ = os.WriteFile(temporary, []byte("inside"), 0600)
			_ = os.Rename(temporary, filepath.Join(dir, "target"))
		}
	}()
	defer func() { close(stop); <-done }()
	for i := 0; i < 200; i++ {
		owner := strings.Repeat("a", 64)
		reply := m.handle(request{ID: "1", Method: "file_download_begin", Owner: owner, Path: "target"})
		if reply.Error != nil {
			continue
		}
		token := reply.Result.(map[string]any)["token"].(string)
		read := m.handle(request{ID: "2", Method: "file_download_read", Owner: owner, Token: token})
		if read.Error != nil {
			continue
		}
		bytes, err := base64.StdEncoding.DecodeString(read.Result.(map[string]any)["data"].(string))
		if err != nil {
			t.Fatal(err)
		}
		if string(bytes) != "inside" {
			t.Fatalf("outside content escaped root: %q", bytes)
		}
	}
}
func TestFilesCancellationReportsCleanupFailure(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("permission failure requires non-root test process")
	}
	dir := t.TempDir()
	if err := os.Chmod(dir, 0700); err != nil {
		t.Fatal(err)
	}
	m, err := newFileManager(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer m.close()
	owner := strings.Repeat("a", 64)
	reply := m.handle(request{ID: "1", Method: "file_upload_begin", Owner: owner, Path: "pending", Size: 0, SHA256: fmt.Sprintf("%x", sha256.Sum256(nil))})
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	token := reply.Result.(map[string]any)["token"].(string)
	staging := filepath.Join(dir, ".veynode-staging")
	if err = os.Chmod(staging, 0500); err != nil {
		t.Fatal(err)
	}
	defer os.Chmod(staging, 0700)
	cancelled := m.handle(request{ID: "2", Method: "file_cancel", Owner: owner, Token: token})
	if cancelled.Error == nil || cancelled.Error.Code != "cleanup_failed" {
		t.Fatal("false successful cleanup", cancelled)
	}
	if err = os.Chmod(staging, 0700); err != nil {
		t.Fatal(err)
	}
	if cancelled = m.handle(request{ID: "3", Method: "file_cancel", Owner: owner, Token: token}); cancelled.Error != nil {
		t.Fatal(cancelled.Error)
	}
}

func TestDownloadFinalHashRejectsChangedBytesEvenWithRestoredMetadata(t *testing.T) {
	dir := t.TempDir()
	if err := os.Chmod(dir, 0700); err != nil {
		t.Fatal(err)
	}
	m, err := newFileManager(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer m.close()
	path := filepath.Join(dir, "mutable")
	body := []byte(strings.Repeat("a", fileChunk+1))
	if err = os.WriteFile(path, body, 0600); err != nil {
		t.Fatal(err)
	}
	owner := strings.Repeat("a", 64)
	begin := m.handle(request{ID: "1", Method: "file_download_begin", Owner: owner, Path: "mutable"})
	if begin.Error != nil {
		t.Fatal(begin.Error)
	}
	token := begin.Result.(map[string]any)["token"].(string)
	before, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	first := m.handle(request{ID: "2", Method: "file_download_read", Owner: owner, Token: token})
	if first.Error != nil {
		t.Fatal(first.Error)
	}
	file, err := os.OpenFile(path, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	_, err = file.WriteAt([]byte("b"), fileChunk)
	file.Close()
	if err != nil {
		t.Fatal(err)
	}
	if err = os.Chtimes(path, before.ModTime(), before.ModTime()); err != nil {
		t.Fatal(err)
	}
	last := m.handle(request{ID: "3", Method: "file_download_read", Owner: owner, Token: token, Offset: fileChunk})
	if last.Error == nil || last.Error.Code != "changed" {
		t.Fatal("changed file reported successful EOF", last)
	}
	if len(m.items) != 0 {
		t.Fatal("changed download remained active")
	}
}

func TestFileBridgeTimeoutTerminatesUnresponsiveChild(t *testing.T) {
	command := exec.Command("/bin/sleep", "30")
	input, err := command.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	output, err := command.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = command.Start(); err != nil {
		t.Fatal(err)
	}
	bridge := &fileBridge{command: command, input: input, reader: bufio.NewReader(output), gate: make(chan struct{}, 1)}
	defer bridge.stop()
	start := time.Now()
	reply := bridge.call(request{ID: "1", Method: "file_worker_info"})
	if reply.Error == nil || reply.Error.Code != "timeout" {
		t.Fatal("unresponsive worker did not time out", reply)
	}
	if time.Since(start) > 7*time.Second || command.ProcessState == nil {
		t.Fatal("worker termination not bounded")
	}
	reply = bridge.call(request{ID: "2", Method: "file_worker_info"})
	if reply.Error == nil || reply.Error.Code != "unavailable" {
		t.Fatal("dead worker accepted request", reply)
	}
}
