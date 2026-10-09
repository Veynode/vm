// Test-only Linux ARM64 one-shot fault injector. Never install in a product image.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

const root = "/var/lib/veynode-crash-once"
const agent = "/usr/local/bin/veynode-guest"

type config struct {
	Nonce    string `json:"nonce"`
	AgentSHA string `json:"agent_sha256"`
}
type identity struct {
	PID    int    `json:"pid"`
	Start  uint64 `json:"start_ticks"`
	Inode  uint64 `json:"executable_inode"`
	Device uint64 `json:"executable_device"`
	SHA    string `json:"executable_sha256"`
}

func check(ok bool, message string) error {
	if !ok {
		return errors.New(message)
	}
	return nil
}
func bounded(path string, max int64, uid uint32, mode uint32) ([]byte, syscall.Stat_t, error) {
	var st syscall.Stat_t
	fd, e := syscall.Open(path, syscall.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK|syscall.O_CLOEXEC, 0)
	if e != nil {
		return nil, st, e
	}
	f := os.NewFile(uintptr(fd), path)
	defer f.Close()
	if e = syscall.Fstat(fd, &st); e != nil {
		return nil, st, e
	}
	if st.Mode&syscall.S_IFMT != syscall.S_IFREG || st.Uid != uid || uint32(st.Mode)&0777 != mode {
		return nil, st, errors.New("unsafe fixed input")
	}
	b, e := io.ReadAll(io.LimitReader(f, max+1))
	if e != nil {
		return nil, st, e
	}
	if int64(len(b)) > max {
		return nil, st, errors.New("input bound")
	}
	var after syscall.Stat_t
	if e = syscall.Fstat(fd, &after); e != nil {
		return nil, st, e
	}
	if st.Ino != after.Ino || st.Size != after.Size {
		return nil, st, errors.New("input changed")
	}
	return b, st, nil
}
func proc(path string, max int64) ([]byte, error) {
	f, e := os.Open(path)
	if e != nil {
		return nil, e
	}
	defer f.Close()
	b, e := io.ReadAll(io.LimitReader(f, max+1))
	if int64(len(b)) > max {
		return nil, errors.New("proc bound")
	}
	return b, e
}
func startTicks(data []byte) (uint64, error) {
	s := string(data)
	i := strings.LastIndex(s, ") ")
	if i < 1 {
		return 0, errors.New("invalid proc stat")
	}
	f := strings.Fields(s[i+2:])
	if len(f) < 20 {
		return 0, errors.New("short proc stat")
	}
	return strconv.ParseUint(f[19], 10, 64)
}
func validHex(s string) bool {
	b, e := hex.DecodeString(s)
	return e == nil && len(b) == 32 && strings.ToLower(s) == s
}
func parseConfig(data []byte) (config, error) {
	var c config
	d := json.NewDecoder(bytes.NewReader(data))
	d.DisallowUnknownFields()
	if e := d.Decode(&c); e != nil {
		return c, e
	}
	var extra any
	if d.Decode(&extra) != io.EOF {
		return c, errors.New("extra config")
	}
	if !validHex(c.Nonce) || !validHex(c.AgentSHA) {
		return c, errors.New("invalid config hashes")
	}
	return c, nil
}
func commandNonce(cmd []byte, nonce string) error {
	n := 0
	for _, f := range strings.Fields(string(cmd)) {
		if strings.HasPrefix(f, "veynode.guest_crash_once=") {
			n++
			if f != "veynode.guest_crash_once="+nonce {
				return errors.New("fault nonce mismatch")
			}
		}
	}
	return check(n == 1, "explicit once nonce required")
}
func identify(pid int, expected string) (identity, error) {
	result := identity{PID: pid}
	prefix := fmt.Sprintf("/proc/%d/", pid)
	exe, e := os.Readlink(prefix + "exe")
	if e != nil {
		return result, e
	}
	if exe != agent {
		return result, errors.New("unexpected executable")
	}
	status, e := proc(prefix+"status", 16384)
	if e != nil {
		return result, e
	}
	found := false
	for _, line := range strings.Split(string(status), "\n") {
		if strings.HasPrefix(line, "Uid:") {
			if strings.Join(strings.Fields(line), " ") != "Uid: 0 0 0 0" {
				return result, errors.New("service must be root")
			}
			found = true
		}
	}
	if !found {
		return result, errors.New("service UID missing")
	}
	argv, e := proc(prefix+"cmdline", 1024)
	if e != nil {
		return result, e
	}
	if !bytes.Equal(argv, []byte(agent+"\x00--virtio-serial-exec\x00--allow-execute\x00--exec-user=veynode\x00")) {
		return result, errors.New("service argv mismatch")
	}
	statBytes, e := proc(prefix+"stat", 4096)
	if e != nil {
		return result, e
	}
	result.Start, e = startTicks(statBytes)
	if e != nil {
		return result, e
	}
	f, e := os.Open(prefix + "exe")
	if e != nil {
		return result, e
	}
	defer f.Close()
	var st, installed syscall.Stat_t
	if e = syscall.Fstat(int(f.Fd()), &st); e != nil {
		return result, e
	}
	if e = syscall.Lstat(agent, &installed); e != nil {
		return result, e
	}
	if st.Ino != installed.Ino || st.Dev != installed.Dev || st.Uid != 0 || installed.Mode&syscall.S_IFMT != syscall.S_IFREG || installed.Mode&022 != 0 {
		return result, errors.New("executable inode mismatch")
	}
	b, e := io.ReadAll(io.LimitReader(f, 32*1024*1024+1))
	if e != nil {
		return result, e
	}
	if len(b) > 32*1024*1024 {
		return result, errors.New("executable bound")
	}
	sum := sha256.Sum256(b)
	result.SHA = hex.EncodeToString(sum[:])
	if result.SHA != expected {
		return result, errors.New("executable hash mismatch")
	}
	result.Inode = st.Ino
	result.Device = uint64(st.Dev)
	return result, nil
}

const (
	pidfdPollIn   int16 = 0x001
	pidfdPollErr  int16 = 0x008
	pidfdPollHup  int16 = 0x010
	pidfdPollNval int16 = 0x020
)

type pollObservation struct {
	ready   uintptr
	revents int16
	errno   syscall.Errno
}

func pollPidfd(fd int) pollObservation {
	p := struct {
		FD      int32
		Events  int16
		Revents int16
	}{int32(fd), pidfdPollIn, 0}
	t := syscall.Timespec{}
	n, _, e := syscall.Syscall6(73, uintptr(unsafe.Pointer(&p)), 1, uintptr(unsafe.Pointer(&t)), 0, 0, 0)
	return pollObservation{n, p.Revents, e}
}

func exitedWithPoll(fd int, poll func(int) pollObservation) (bool, error) {
	if fd < 0 || int64(fd) > int64(1<<31-1) {
		return false, errors.New("invalid pidfd")
	}
	// Each poll is nonblocking. A runtime signal may interrupt the syscall;
	// retry without manufacturing process death or extending the arm deadline.
	// Bound signal storms as observation failure, never as alive/dead proof.
	for attempt := 0; attempt < 32; attempt++ {
		p := poll(fd)
		if p.errno == syscall.EINTR {
			continue
		}
		if p.errno != 0 {
			return false, fmt.Errorf("pidfd ppoll failed: errno=%d", p.errno)
		}
		if p.ready == 0 && p.revents == 0 {
			return false, nil
		}
		// One requested descriptor: require a readable pidfd. HUP may coexist
		// after reap, but HUP alone or error/unknown bits cannot prove POLLIN.
		if p.ready != 1 || p.revents&pidfdPollIn == 0 || p.revents & ^(pidfdPollIn|pidfdPollHup) != 0 {
			return false, fmt.Errorf("pidfd ppoll ambiguous: count=%d revents=0x%x", p.ready, uint16(p.revents))
		}
		return true, nil
	}
	return false, errors.New("pidfd ppoll interrupted retry bound")
}

func exited(fd int) (bool, error) {
	return exitedWithPoll(fd, pollPidfd)
}

func aliveBefore(fd int, phase string, poll func(int) (bool, error)) error {
	dead, err := poll(fd)
	if err != nil {
		return fmt.Errorf("target observation failed %s: %w", phase, err)
	}
	if dead {
		return fmt.Errorf("target exited %s: pidfd POLLIN", phase)
	}
	return nil
}
func writeEvent(output io.Writer, event string, data any) error {
	b, err := json.Marshal(map[string]any{"event": event, "monotonic_note": "Guest wall timestamp for correlation only", "unix_ms": time.Now().UnixMilli(), "data": data})
	if err != nil {
		return err
	}
	// Getty prompts do not end in LF. Keep the strict line-start parser by
	// establishing our own boundary, in the same bounded write as the record.
	line := append([]byte("\nVEYNODE_GUEST_CRASH "), b...)
	line = append(line, '\n')
	if len(line) > 4096 {
		return errors.New("event bound")
	}
	n, err := output.Write(line)
	if err != nil {
		return err
	}
	if n != len(line) {
		return io.ErrShortWrite
	}
	return nil
}
func logEvent(event string, data any) error {
	return writeEvent(os.Stdout, event, data)
}
func run() error {
	if runtime.GOOS != "linux" || runtime.GOARCH != "arm64" || os.Geteuid() != 0 {
		return errors.New("root Linux ARM64 fixture only")
	}
	for path, mode := range map[string]uint32{root: 0711, root + "/state": 0700} {
		var s syscall.Stat_t
		if e := syscall.Lstat(path, &s); e != nil {
			return e
		}
		if s.Mode&syscall.S_IFMT != syscall.S_IFDIR || s.Uid != 0 || uint32(s.Mode)&0777 != mode {
			return errors.New("private fixture directory required")
		}
	}
	b, _, e := bounded("/opt/veynode-crash/config.json", 4096, 0, 0600)
	if e != nil {
		return e
	}
	cfg, e := parseConfig(b)
	if e != nil {
		return e
	}
	cmd, e := proc("/proc/cmdline", 8192)
	if e != nil {
		return e
	}
	if e = commandNonce(cmd, cfg.Nonce); e != nil {
		return e
	}
	// Consumed before waiting: no second boot or automatic restart can fire again.
	stamp, e := os.OpenFile(root+"/state/attempted", os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if e != nil {
		return e
	}
	if _, e = stamp.WriteString("one-shot-v1\n"); e == nil {
		e = stamp.Sync()
	}
	stamp.Close()
	if e != nil {
		return e
	}
	directory, e := os.Open(root + "/state")
	if e != nil {
		return e
	}
	e = directory.Sync()
	directory.Close()
	if e != nil {
		return e
	}
	pidBytes, _, e := bounded("/run/veynode-agent.pid", 64, 0, 0644)
	if e != nil {
		return e
	}
	pid, e := strconv.Atoi(strings.TrimSpace(string(pidBytes)))
	if e != nil || pid <= 1 {
		return errors.New("invalid service PID")
	}
	fd, _, errno := syscall.Syscall(434, uintptr(pid), 0, 0)
	if errno != 0 {
		return errno
	}
	defer syscall.Close(int(fd))
	original, e := identify(pid, cfg.AgentSHA)
	if e != nil {
		return e
	}
	if e = aliveBefore(int(fd), "before readiness", exited); e != nil {
		return e
	}
	initial, armIdentity, e := bounded(root+"/arm", 65, 1000, 0600)
	if e != nil {
		return e
	}
	if len(initial) != 0 {
		return errors.New("arm file not initially empty")
	}
	if e = logEvent("watcher_ready", original); e != nil {
		return e
	}
	deadline := time.Now().Add(120 * time.Second)
	for time.Now().Before(deadline) {
		arm, st, e := bounded(root+"/arm", 65, 1000, 0600)
		if e != nil {
			return e
		}
		if st.Ino != armIdentity.Ino || st.Dev != armIdentity.Dev {
			return errors.New("arm inode replaced")
		}
		if len(arm) != 0 {
			if string(arm) != cfg.Nonce+"\n" {
				return errors.New("arm content mismatch")
			}
			current, e := identify(pid, cfg.AgentSHA)
			if e != nil {
				return e
			}
			if current != original {
				return errors.New("target incarnation changed")
			}
			if e = aliveBefore(int(fd), "before signal", exited); e != nil {
				return e
			}
			if e = logEvent("target_verified_before_signal", current); e != nil {
				return e
			}
			_, _, errno = syscall.Syscall6(424, fd, uintptr(syscall.SIGKILL), 0, 0, 0, 0)
			if errno != 0 {
				return errno
			}
			return logEvent("sigkill_sent_to_pinned_service", original)
		}
		if e = aliveBefore(int(fd), "before arm", exited); e != nil {
			return e
		}
		time.Sleep(25 * time.Millisecond)
	}
	return errors.New("arm deadline")
}
func main() {
	if e := run(); e != nil {
		_ = logEvent("refused", map[string]string{"reason": e.Error()})
		os.Exit(1)
	}
}
