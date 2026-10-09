package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"testing"
)

type shortWriter struct{}

func (shortWriter) Write(b []byte) (int, error) { return len(b) - 1, nil }

func TestEventSeparatesActualTTYPromptAndPreservesStrictRecord(t *testing.T) {
	var output bytes.Buffer
	output.WriteString("veynode-desktop login: ")
	if err := writeEvent(&output, "target_verified_before_signal", identity{PID: 42}); err != nil {
		t.Fatal(err)
	}
	lines := bytes.Split(output.Bytes(), []byte{'\n'})
	if len(lines) != 3 || string(lines[0]) != "veynode-desktop login: " || !bytes.HasPrefix(lines[1], []byte("VEYNODE_GUEST_CRASH ")) {
		t.Fatalf("TTY boundary missing: %q", output.Bytes())
	}
	var record map[string]any
	if err := json.Unmarshal(bytes.TrimPrefix(lines[1], []byte("VEYNODE_GUEST_CRASH ")), &record); err != nil {
		t.Fatal(err)
	}
	if record["event"] != "target_verified_before_signal" {
		t.Fatal("wrong event")
	}
	if err := writeEvent(shortWriter{}, "watcher_ready", identity{PID: 42}); err != io.ErrShortWrite {
		t.Fatalf("short write not rejected: %v", err)
	}
}

func TestRefusesHostBeforeAnyFilesystemOrSignal(t *testing.T) {
	if runtime.GOOS != "linux" || runtime.GOARCH != "arm64" || os.Geteuid() != 0 {
		if run() == nil {
			t.Fatal("host accepted")
		}
	} else {
		t.Skip("do not invoke live root ARM64 watcher in tests")
	}
}
func TestExactNonceSingleOccurrence(t *testing.T) {
	n := strings.Repeat("a", 64)
	if commandNonce([]byte("console=ttyAMA0 veynode.guest_crash_once="+n), n) != nil {
		t.Fatal("valid nonce rejected")
	}
	for _, v := range []string{"", "veynode.guest_crash_once=" + strings.Repeat("b", 64), "veynode.guest_crash_once=" + n + " veynode.guest_crash_once=" + n} {
		if commandNonce([]byte(v), n) == nil {
			t.Fatal("invalid nonce accepted")
		}
	}
}
func TestStrictConfigAndProcStarttime(t *testing.T) {
	n := strings.Repeat("a", 64)
	valid := fmt.Sprintf(`{"nonce":%q,"agent_sha256":%q}`, n, n)
	if _, e := parseConfig([]byte(valid)); e != nil {
		t.Fatal(e)
	}
	for _, v := range []string{valid + "{}", strings.Replace(valid, "\"nonce\"", "\"unknown\"", 1), strings.Replace(valid, n, "BAD", 1)} {
		if _, e := parseConfig([]byte(v)); e == nil {
			t.Fatal("invalid config")
		}
	}
	fields := make([]string, 20)
	for i := range fields {
		fields[i] = "0"
	}
	fields[0] = "S"
	fields[19] = "123456"
	if value, e := startTicks([]byte("13 (odd ) name) " + strings.Join(fields, " "))); e != nil || value != 123456 {
		t.Fatalf("stat %d %v", value, e)
	}
	if _, e := startTicks([]byte("13 (sh) S 1")); e == nil {
		t.Fatal("short stat")
	}
}
func TestBoundedPrivateRegularFileOnly(t *testing.T) {
	d := t.TempDir()
	p := filepath.Join(d, "arm")
	if e := os.WriteFile(p, []byte("hello"), 0600); e != nil {
		t.Fatal(e)
	}
	if b, _, e := bounded(p, 5, uint32(os.Geteuid()), 0600); e != nil || string(b) != "hello" {
		t.Fatal(e)
	}
	if _, _, e := bounded(p, 4, uint32(os.Geteuid()), 0600); e == nil {
		t.Fatal("oversize accepted")
	}
	if _, _, e := bounded(p, 5, uint32(os.Geteuid()+1), 0600); e == nil {
		t.Fatal("wrong owner accepted")
	}
	link := filepath.Join(d, "link")
	if e := os.Symlink(p, link); e != nil {
		t.Fatal(e)
	}
	if _, _, e := bounded(link, 5, uint32(os.Geteuid()), 0600); e == nil {
		t.Fatal("symlink accepted")
	}
	fifo := filepath.Join(d, "fifo")
	if e := syscall.Mkfifo(fifo, 0600); e != nil {
		t.Fatal(e)
	}
	if _, _, e := bounded(fifo, 5, uint32(os.Geteuid()), 0600); e == nil {
		t.Fatal("fifo accepted")
	}
}

func TestPidfdInterruptedPollRetriesWithoutClaimingDeath(t *testing.T) {
	for _, final := range []pollObservation{{0, 0, 0}, {1, pidfdPollIn, 0}} {
		calls := 0
		dead, err := exitedWithPoll(42, func(fd int) pollObservation {
			if fd != 42 {
				t.Fatal("wrong original pidfd")
			}
			calls++
			if calls <= 2 {
				return pollObservation{^uintptr(0), pidfdPollNval, syscall.EINTR}
			}
			return final
		})
		if err != nil || dead != (final.ready == 1) || calls != 3 {
			t.Fatalf("interrupted poll: dead=%v err=%v calls=%d", dead, err, calls)
		}
	}
}

func TestPidfdErrorAndUnknownEventsNeverBecomeDeathProof(t *testing.T) {
	for _, p := range []pollObservation{
		{^uintptr(0), 0, syscall.EBADF}, {^uintptr(0), 0, syscall.EINVAL},
		{1, pidfdPollNval, 0}, {1, pidfdPollErr, 0}, {1, pidfdPollHup, 0},
		{1, pidfdPollIn | pidfdPollErr, 0}, {1, pidfdPollIn | pidfdPollNval, 0},
		{1, pidfdPollIn | 0x004, 0}, {0, pidfdPollIn, 0}, {1, 0, 0}, {2, pidfdPollIn, 0},
	} {
		dead, err := exitedWithPoll(42, func(int) pollObservation { return p })
		if dead || err == nil {
			t.Fatalf("invalid poll became proof: %+v -> %v %v", p, dead, err)
		}
	}
	for _, p := range []pollObservation{{1, pidfdPollIn, 0}, {1, pidfdPollIn | pidfdPollHup, 0}} {
		dead, err := exitedWithPoll(42, func(int) pollObservation { return p })
		if !dead || err != nil {
			t.Fatalf("clear POLLIN rejected: %+v %v", p, err)
		}
	}
}

func TestPidfdSignalStormIsBoundedObservationFailure(t *testing.T) {
	calls := 0
	dead, err := exitedWithPoll(42, func(int) pollObservation {
		calls++
		return pollObservation{^uintptr(0), 0, syscall.EINTR}
	})
	if dead || err == nil || calls != 32 || !strings.Contains(err.Error(), "retry bound") {
		t.Fatalf("unbounded or misclassified signal storm: %v %v %d", dead, err, calls)
	}
	for _, fd := range []int{-1, 1 << 32} {
		_, err := exitedWithPoll(fd, func(int) pollObservation { t.Fatal("invalid fd reached syscall"); return pollObservation{} })
		if err == nil {
			t.Fatal("invalid fd accepted")
		}
	}
}

func TestAllLivenessPhasesDistinguishSystemFailureFromTargetExit(t *testing.T) {
	for _, phase := range []string{"before readiness", "before arm", "before signal"} {
		err := aliveBefore(42, phase, func(int) (bool, error) { return false, syscall.EINTR })
		if err == nil || !strings.Contains(err.Error(), "observation failed "+phase) || strings.Contains(err.Error(), "target exited") {
			t.Fatalf("system failure mislabeled: %v", err)
		}
		err = aliveBefore(42, phase, func(int) (bool, error) { return true, nil })
		if err == nil || !strings.Contains(err.Error(), "target exited "+phase+": pidfd POLLIN") {
			t.Fatalf("death classification: %v", err)
		}
		if err = aliveBefore(42, phase, func(int) (bool, error) { return false, nil }); err != nil {
			t.Fatal(err)
		}
	}
}
