package main

import (
	"bufio"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"
)

const fileChunk = 32768
const fileMaximum = 8 * 1024 * 1024
const fileTTL = 60 * time.Second
const fileLifetime = 5 * time.Minute

var files *fileBridge

type fileBridge struct {
	command  *exec.Cmd
	input    io.WriteCloser
	reader   *bufio.Reader
	gate     chan struct{}
	dead     bool
	info     any
	stopOnce sync.Once
}

func startFileBridge(account, directory string) (*fileBridge, error) {
	if runtime.GOOS != "linux" {
		return nil, fmt.Errorf("file integration is Linux-only")
	}
	if account == "" || !filepath.IsAbs(directory) {
		return nil, fmt.Errorf("files require fixed non-root --files-user and absolute --files-root")
	}
	identity, err := resolveExecutionIdentity(account)
	if err != nil {
		return nil, err
	}
	if !identity.Rootless {
		return nil, fmt.Errorf("root file worker forbidden")
	}
	binary, err := os.Executable()
	if err != nil {
		return nil, err
	}
	command := exec.Command(binary, "--file-worker", "--files-root", directory)
	identity.configure(command)
	input, err := command.StdinPipe()
	if err != nil {
		return nil, err
	}
	output, err := command.StdoutPipe()
	if err != nil {
		return nil, err
	}
	command.Stderr = os.Stderr
	if err = command.Start(); err != nil {
		return nil, err
	}
	bridge := &fileBridge{command: command, input: input, reader: bufio.NewReader(output), gate: make(chan struct{}, 1)}
	reply := bridge.call(request{ID: "startup", Method: "file_worker_info"})
	if reply.Error != nil {
		bridge.stop()
		return nil, fmt.Errorf("file worker startup: %s", reply.Error.Message)
	}
	bridge.info = reply.Result
	return bridge, nil
}
func (b *fileBridge) stop() {
	b.stopOnce.Do(func() {
		_ = b.input.Close()
		_ = b.command.Process.Signal(syscall.SIGTERM)
		done := make(chan struct{})
		go func() { _ = b.command.Wait(); close(done) }()
		select {
		case <-done:
		case <-time.After(time.Second):
			_ = b.command.Process.Kill()
			<-done
		}
	})
}

func (b *fileBridge) call(req request) response {
	select {
	case b.gate <- struct{}{}:
	default:
		return failure(req.ID, "busy", "file worker request already active")
	}
	defer func() { <-b.gate }()
	if b.dead {
		return failure(req.ID, "unavailable", "file worker stopped; service restart required")
	}
	done := make(chan response, 1)
	go func() {
		if err := json.NewEncoder(b.input).Encode(req); err != nil {
			done <- failure(req.ID, "unavailable", err.Error())
			return
		}
		line, err := bufio.NewReader(io.LimitReader(b.reader, maxRequest+1)).ReadBytes('\n')
		if err != nil || len(line) > maxRequest {
			done <- failure(req.ID, "unavailable", "file worker response unavailable or oversized")
			return
		}
		var reply response
		if json.Unmarshal(line, &reply) != nil || reply.ID != req.ID {
			done <- failure(req.ID, "unavailable", "invalid file worker response")
			return
		}
		done <- reply
	}()
	select {
	case reply := <-done:
		return reply
	case <-time.After(5 * time.Second):
		b.dead = true
		b.stop()
		return failure(req.ID, "timeout", "file request exceeded 5 seconds; worker terminated, restart service to recover")
	}
}

type transfer struct {
	owner, kind, path, temp, digest string
	file                            *os.File
	info                            os.FileInfo
	size, offset                    int64
	hash                            hash.Hash
	created, touched                time.Time
}
type fileManager struct {
	sync.Mutex
	root   *os.Root
	lock   *os.File
	items  map[string]*transfer
	closed map[string]time.Time
	now    func() time.Time
}

func validHex(value string, length int) bool {
	if len(value) != length {
		return false
	}
	for _, v := range value {
		if !(v >= '0' && v <= '9' || v >= 'a' && v <= 'f') {
			return false
		}
	}
	return true
}
func fileName(name string) error {
	if len(name) == 0 || len(name) > 255 || name == "." || name == ".." || strings.ContainsAny(name, "/\\\x00") || strings.HasPrefix(name, ".veynode-") {
		return fmt.Errorf("path must be a single non-reserved filename inside the configured directory")
	}
	return nil
}
func newFileManager(directory string) (*fileManager, error) {
	info, err := os.Lstat(directory)
	if err != nil {
		return nil, err
	}
	if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 || info.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) || info.Mode().Perm()&0077 != 0 {
		return nil, fmt.Errorf("files root must be an existing private directory owned by file worker")
	}
	root, err := os.OpenRoot(directory)
	if err != nil {
		return nil, err
	}
	bound, err := root.Stat(".")
	if err != nil || !os.SameFile(info, bound) || !bound.IsDir() || bound.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) || bound.Mode().Perm()&0077 != 0 {
		root.Close()
		return nil, fmt.Errorf("files root changed during open or bound directory has unsafe ownership/permissions")
	}
	lock, err := root.OpenFile(".veynode-lock", os.O_CREATE|os.O_RDWR|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		root.Close()
		return nil, err
	}
	info, err = lock.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Sys().(*syscall.Stat_t).Nlink != 1 || info.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) {
		lock.Close()
		root.Close()
		return nil, fmt.Errorf("unsafe files lock")
	}
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		lock.Close()
		root.Close()
		return nil, fmt.Errorf("file root already in use")
	}
	manager := &fileManager{root: root, lock: lock, items: map[string]*transfer{}, closed: map[string]time.Time{}, now: time.Now}
	if err = root.Mkdir(".veynode-staging", 0700); err != nil && !errors.Is(err, os.ErrExist) {
		manager.close()
		return nil, err
	}
	info, err = root.Lstat(".veynode-staging")
	if err != nil || !info.IsDir() || info.Mode().Perm()&0077 != 0 || info.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) {
		manager.close()
		return nil, fmt.Errorf("unsafe staging directory")
	}
	dir, err := root.Open(".veynode-staging")
	if err != nil {
		manager.close()
		return nil, err
	}
	entries, err := dir.ReadDir(-1)
	dir.Close()
	if err != nil {
		manager.close()
		return nil, err
	}
	for _, entry := range entries {
		name := entry.Name()
		token := strings.TrimSuffix(strings.TrimPrefix(name, ".veynode-upload-"), ".part")
		if name == ".veynode-upload-"+token+".part" && validHex(token, 32) {
			if err = root.Remove(".veynode-staging/" + name); err != nil {
				manager.close()
				return nil, err
			}
		}
	}
	return manager, nil
}
func (m *fileManager) remove(token string) error {
	if t := m.items[token]; t != nil {
		t.file.Close()
		if t.temp != "" {
			if err := m.root.Remove(t.temp); err != nil && !errors.Is(err, os.ErrNotExist) {
				return err
			}
		}
		delete(m.items, token)
	}
	return nil
}
func (m *fileManager) prune() {
	now := m.now()
	for owner, until := range m.closed {
		if !now.Before(until) {
			delete(m.closed, owner)
		}
	}
	for token, t := range m.items {
		if now.Sub(t.touched) >= fileTTL || now.Sub(t.created) >= fileLifetime {
			m.remove(token)
		}
	}
}
func (m *fileManager) close() {
	m.Lock()
	defer m.Unlock()
	for token := range m.items {
		m.remove(token)
	}
	m.lock.Close()
	m.root.Close()
}
func fileError(id string, err error) response {
	code := "file_error"
	if errors.Is(err, os.ErrExist) {
		code = "already_exists"
	}
	if errors.Is(err, os.ErrNotExist) {
		code = "not_found"
	}
	if errors.Is(err, os.ErrPermission) {
		code = "forbidden"
	}
	return failure(id, code, err.Error())
}
func (m *fileManager) stagedCount() int {
	dir, err := m.root.Open(".veynode-staging")
	if err != nil {
		return -1
	}
	defer dir.Close()
	entries, err := dir.ReadDir(-1)
	if err != nil {
		return -1
	}
	return len(entries)
}
func (m *fileManager) handle(req request) response {
	m.Lock()
	defer m.Unlock()
	m.prune()
	success := func(value any) response { return response{ID: req.ID, Result: value} }
	if req.Method == "file_worker_info" {
		return success(map[string]any{"uid": os.Geteuid(), "gid": os.Getegid(), "max_file_bytes": fileMaximum, "chunk_bytes": fileChunk, "idle_timeout_seconds": 60, "absolute_timeout_seconds": 300, "path_scope": "single filename within configured files root", "root": m.root.Name(), "ownership": "published files belong to fixed files worker account; execute permission does not grant access", "max_transfers": 16, "active_transfers": len(m.items), "staged_upload_files": m.stagedCount()})
	}
	if !validHex(req.Owner, 64) {
		return failure(req.ID, "invalid_request", "invalid session owner")
	}
	if req.Method == "file_session_end" {
		count := 0
		var cleanupError error
		for token, t := range m.items {
			if t.owner == req.Owner {
				if err := m.remove(token); err != nil {
					cleanupError = err
				} else {
					count++
				}
			}
		}
		if _, exists := m.closed[req.Owner]; exists || len(m.closed) < 256 {
			m.closed[req.Owner] = m.now().Add(fileLifetime)
		}
		if cleanupError != nil {
			return failure(req.ID, "cleanup_failed", cleanupError.Error())
		}
		return success(map[string]any{"cancelled_transfers": count})
	}
	if _, closed := m.closed[req.Owner]; closed {
		return failure(req.ID, "forbidden", "file session closed")
	}
	if req.Method == "file_upload_begin" || req.Method == "file_download_begin" {
		if len(m.closed) >= 256 {
			return failure(req.ID, "busy", "closed-session retention capacity reached")
		}
		if err := fileName(req.Path); err != nil {
			return failure(req.ID, "invalid_request", err.Error())
		}
		if len(m.items) >= 16 {
			return failure(req.ID, "busy", "16 file transfers already active")
		}
		random := make([]byte, 16)
		if _, err := rand.Read(random); err != nil {
			return fileError(req.ID, err)
		}
		token := hex.EncodeToString(random)
		t := &transfer{owner: req.Owner, path: req.Path, created: m.now(), touched: m.now(), hash: sha256.New()}
		if req.Method == "file_upload_begin" {
			if req.Size < 0 || req.Size > fileMaximum || !validHex(req.SHA256, 64) {
				return failure(req.ID, "invalid_request", "upload needs size 0..8MiB and lowercase SHA256")
			}
			if _, err := m.root.Lstat(req.Path); err == nil {
				return failure(req.ID, "already_exists", "destination exists; overwrite forbidden")
			} else if !errors.Is(err, os.ErrNotExist) {
				return fileError(req.ID, err)
			}
			t.kind = "upload"
			t.size = req.Size
			t.digest = req.SHA256
			t.temp = ".veynode-staging/.veynode-upload-" + token + ".part"
			file, err := m.root.OpenFile(t.temp, os.O_CREATE|os.O_EXCL|os.O_RDWR|syscall.O_NOFOLLOW, 0600)
			if err != nil {
				return fileError(req.ID, err)
			}
			t.file = file
		} else {
			file, err := m.root.OpenFile(req.Path, os.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0)
			if err != nil {
				return fileError(req.ID, err)
			}
			info, err := file.Stat()
			if err != nil {
				file.Close()
				return fileError(req.ID, err)
			}
			if !info.Mode().IsRegular() || info.Sys().(*syscall.Stat_t).Nlink != 1 || info.Size() > fileMaximum || info.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) {
				file.Close()
				return failure(req.ID, "invalid_request", "download requires owned single-link regular file of at most 8MiB")
			}
			t.kind = "download"
			t.file = file
			t.info = info
			t.size = info.Size()
			if _, err = io.Copy(t.hash, io.LimitReader(file, fileMaximum+1)); err != nil {
				file.Close()
				return fileError(req.ID, err)
			}
			if _, err = file.Seek(0, io.SeekStart); err != nil {
				file.Close()
				return fileError(req.ID, err)
			}
			t.digest = hex.EncodeToString(t.hash.Sum(nil))
			t.hash = sha256.New()
		}
		m.items[token] = t
		return success(map[string]any{"token": token, "size": t.size, "sha256": t.digest, "chunk_bytes": fileChunk, "idle_timeout_seconds": 60})
	}
	if !validHex(req.Token, 32) {
		return failure(req.ID, "invalid_request", "invalid transfer token")
	}
	t := m.items[req.Token]
	if t == nil || t.owner != req.Owner {
		return failure(req.ID, "not_found", "transfer unknown, expired or not owned by this session")
	}
	t.touched = m.now()
	if req.Method == "file_cancel" {
		if err := m.remove(req.Token); err != nil {
			return failure(req.ID, "cleanup_failed", err.Error())
		}
		return success(map[string]any{"cancelled": true})
	}
	if req.Method == "file_upload_write" && t.kind == "upload" {
		data, err := base64.StdEncoding.Strict().DecodeString(req.Data)
		if err != nil || len(data) == 0 || len(data) > fileChunk || req.Offset != t.offset || int64(len(data)) > t.size-t.offset {
			return failure(req.ID, "invalid_request", "chunk must be valid base64, 1..32KiB and match sequential offset/declared size")
		}
		if _, err = t.file.Write(data); err != nil {
			return fileError(req.ID, errors.Join(err, m.remove(req.Token)))
		}
		t.hash.Write(data)
		t.offset += int64(len(data))
		return success(map[string]any{"offset": t.offset})
	}
	if req.Method == "file_upload_commit" && t.kind == "upload" {
		if t.offset != t.size || hex.EncodeToString(t.hash.Sum(nil)) != t.digest {
			if err := m.remove(req.Token); err != nil {
				return failure(req.ID, "cleanup_failed", "upload integrity rejected; staging cleanup failed: "+err.Error())
			}
			return failure(req.ID, "integrity_error", "upload length or SHA256 differs; temporary file removed")
		}
		if err := t.file.Sync(); err != nil {
			return fileError(req.ID, errors.Join(err, m.remove(req.Token)))
		}
		// Link is an atomic no-replace publication; ordinary Rename could overwrite a competing destination.
		if err := m.root.Link(t.temp, t.path); err != nil {
			return fileError(req.ID, errors.Join(err, m.remove(req.Token)))
		}
		size, digest := t.size, t.digest
		if err := m.remove(req.Token); err != nil {
			return failure(req.ID, "published_cleanup_failed", "file published but staging cleanup failed: "+err.Error())
		}
		dir, err := m.root.Open(".")
		if err == nil {
			err = dir.Sync()
			dir.Close()
		}
		if err != nil {
			return failure(req.ID, "durability_unknown", "file published but directory sync failed; inspect destination before retrying")
		}
		return success(map[string]any{"published": true, "size": size, "sha256": digest})
	}
	if req.Method == "file_download_read" && t.kind == "download" {
		if req.Offset != t.offset {
			return failure(req.ID, "invalid_request", "download offset must be sequential")
		}
		info, err := t.file.Stat()
		if err != nil {
			return fileError(req.ID, errors.Join(err, m.remove(req.Token)))
		}
		if !os.SameFile(info, t.info) || info.Size() != t.size || !info.ModTime().Equal(t.info.ModTime()) || info.Sys().(*syscall.Stat_t).Nlink != 1 || info.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) {
			m.remove(req.Token)
			return failure(req.ID, "changed", "source changed during transfer")
		}
		buffer := make([]byte, min(int64(fileChunk), t.size-t.offset))
		if _, err = io.ReadFull(t.file, buffer); err != nil {
			return fileError(req.ID, errors.Join(err, m.remove(req.Token)))
		}
		t.offset += int64(len(buffer))
		t.hash.Write(buffer)
		eof := t.offset == t.size
		if eof {
			after, err := t.file.Stat()
			if err != nil {
				return fileError(req.ID, errors.Join(err, m.remove(req.Token)))
			}
			if !os.SameFile(after, t.info) || after.Size() != t.size || !after.ModTime().Equal(t.info.ModTime()) || after.Sys().(*syscall.Stat_t).Nlink != 1 || after.Sys().(*syscall.Stat_t).Uid != uint32(os.Geteuid()) || hex.EncodeToString(t.hash.Sum(nil)) != t.digest {
				cleanup := m.remove(req.Token)
				if cleanup != nil {
					return failure(req.ID, "cleanup_failed", "download changed; cleanup failed: "+cleanup.Error())
				}
				return failure(req.ID, "changed", "download metadata or transmitted SHA256 changed before final chunk; discard partial data")
			}
		}
		reply := map[string]any{"data": base64.StdEncoding.EncodeToString(buffer), "offset": t.offset, "eof": eof, "sha256": t.digest}
		if eof {
			m.remove(req.Token)
		}
		return success(reply)
	}
	return failure(req.ID, "invalid_request", "method does not match transfer kind")
}
func runFileWorker(directory string) error {
	if runtime.GOOS != "linux" || os.Geteuid() == 0 {
		return fmt.Errorf("file worker requires non-root Linux identity")
	}
	manager, err := newFileManager(directory)
	if err != nil {
		return err
	}
	defer manager.close()
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, syscall.SIGTERM, syscall.SIGINT)
	go func() { <-signals; manager.close(); os.Exit(0) }()
	done := make(chan struct{})
	defer close(done)
	go func() {
		ticker := time.NewTicker(time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-done:
				return
			case <-ticker.C:
				manager.Lock()
				manager.prune()
				manager.Unlock()
			}
		}
	}()
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), maxRequest)
	for scanner.Scan() {
		var req request
		decoder := json.NewDecoder(strings.NewReader(scanner.Text()))
		decoder.DisallowUnknownFields()
		if err := decoder.Decode(&req); err != nil {
			return err
		}
		reply := manager.handle(req)
		if err := json.NewEncoder(os.Stdout).Encode(reply); err != nil {
			return err
		}
	}
	return scanner.Err()
}
