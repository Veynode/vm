package main

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func sleepRequest() request {
	return request{ID: "start", Method: "exec_start", Argv: []string{"/bin/sh", "-c", "sleep 5"}, TimeoutMS: 10000}
}
func TestJobsShareFourCommandBudget(t *testing.T) {
	m := testManager(t)
	for i := 0; i < 4; i++ {
		if r := m.start(sleepRequest()); r.Error != nil {
			t.Fatal(r)
		}
	}
	if r := m.start(sleepRequest()); r.Error == nil || r.Error.Code != "busy" {
		t.Fatal(r)
	}
	if !m.stopAll(time.Second) {
		t.Fatal("cleanup")
	}
}

type failingCloseBoundary struct{ jobBoundary }

func (b failingCloseBoundary) close() error { return errors.New("injected remove failure") }
func TestAsyncCleanupFailureCannotBecomeClean(t *testing.T) {
	m := testManager(t)
	m.boundary = func(c *exec.Cmd) (jobBoundary, error) { b, e := legacyBoundary(c); return failingCloseBoundary{b}, e }
	if r := m.start(sleepRequest()); r.Error != nil {
		t.Fatal(r)
	}
	if m.stopAll(time.Second) || m.stopAll(time.Second) {
		t.Fatal("failed containment became clean")
	}
	if r := m.start(sleepRequest()); r.Error == nil {
		t.Fatal("failure admitted start")
	}
}
func TestFailedStartCleanupFencesSyncAndAsync(t *testing.T) {
	for _, async := range []bool{false, true} {
		t.Run(fmt.Sprint(async), func(t *testing.T) {
			m := testManager(t)
			m.boundary = func(c *exec.Cmd) (jobBoundary, error) {
				c.Path = "/nonexistent-veynode-executable"
				b, e := legacyBoundary(c)
				return failingCloseBoundary{b}, e
			}
			req := sleepRequest()
			var r response
			if async {
				r = m.start(req)
			} else {
				r = m.execute(req)
			}
			if r.Error == nil || r.Error.Code != "containment_failed" || m.stopAll(time.Second) {
				t.Fatal(r)
			}
		})
	}
}

// These host tests use real subprocesses and Unix transport. Setting the policy
// tag here is only a protocol seam: it does NOT exercise Linux clone3/cgroups.
func serialExecPair(t *testing.T, m *jobManager) (net.Conn, <-chan error, serialEnvelope) {
	t.Helper()
	m.containmentMode = "cgroup-v2-clone3-kill"
	m.identity.Rootless = true
	if m.identity.UID == 0 {
		t.Skip("host protocol seam requires existing nonroot user")
	}
	dir, err := os.MkdirTemp("/tmp", "vn-v2-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	listener, err := net.Listen("unix", filepath.Join(dir, "agent.sock"))
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		defer listener.Close()
		conn, err := listener.Accept()
		if err != nil {
			done <- err
			return
		}
		done <- serveSerialExec(conn.(deadlineStream), m)
	}()
	b, err := net.Dial("unix", filepath.Join(dir, "agent.sock"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { b.Close() })
	ready := serialHost(t, b)
	if ready.Version != 2 {
		t.Fatal(ready)
	}
	ready.Type = "request"
	return b, done, ready
}
func v2Send(t *testing.T, c net.Conn, base serialEnvelope, seq uint64, r request) {
	t.Helper()
	base.Sequence = seq
	base.Request = &r
	if e := serialWrite(c, base); e != nil {
		t.Fatal(e)
	}
}
func v2Read(t *testing.T, c net.Conn) serialEnvelope {
	t.Helper()
	r, e := serialRead(c, false)
	if e != nil {
		t.Fatal(e)
	}
	return r
}
func TestSerialV2HeartbeatDuringPendingStart(t *testing.T) {
	m := testManager(t)
	entered, unblock := make(chan struct{}), make(chan struct{})
	m.boundary = func(c *exec.Cmd) (jobBoundary, error) { close(entered); <-unblock; return legacyBoundary(c) }
	c, done, base := serialExecPair(t, m)
	v2Send(t, c, base, 1, sleepRequest())
	<-entered
	v2Send(t, c, base, 2, request{ID: "heart", Method: "heartbeat"})
	heartbeat := v2Read(t, c)
	if heartbeat.Sequence != 2 || heartbeat.Response.ID != "heart" {
		t.Fatal("heartbeat blocked by start", heartbeat)
	}
	close(unblock)
	if r := v2Read(t, c); r.Sequence != 1 || r.Response.Error != nil {
		t.Fatal(r)
	}
	c.Close()
	serialEnded(t, done)
}
func TestSerialV2WatchdogCancelsRealJob(t *testing.T) {
	m := testManager(t)
	c, done, base := serialExecPair(t, m)
	v2Send(t, c, base, 1, sleepRequest())
	if reply := v2Read(t, c); reply.Response.Error != nil {
		t.Fatal(reply)
	}
	begin := time.Now()
	serialEnded(t, done)
	if time.Since(begin) > 3500*time.Millisecond {
		t.Fatal("watchdog cleanup too slow")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.active != 0 || !m.stopping {
		t.Fatal("unfenced live job")
	}
	for _, job := range m.jobs {
		if !job.finished || job.result["cancelled"] != true {
			t.Fatal(job)
		}
	}
}
func TestSerialV2RejectsBindingAndUnknownFields(t *testing.T) {
	for _, kind := range []string{"sequence", "nonce", "unknown", "duplicate", "scope", "old-version", "sync-exec", "files"} {
		t.Run(kind, func(t *testing.T) {
			m := testManager(t)
			c, done, base := serialExecPair(t, m)
			base.Sequence = 1
			base.Request = &request{ID: "one", Method: "heartbeat"}
			b, _ := json.Marshal(base)
			s := string(b)
			switch kind {
			case "sequence":
				s = strings.Replace(s, `"sequence":1`, `"sequence":2`, 1)
			case "nonce":
				s = strings.Replace(s, base.HostNonce, strings.Repeat("b", 64), 1)
			case "unknown":
				s = strings.Replace(s, `"method":"heartbeat"`, `"method":"heartbeat","extra":1`, 1)
			case "duplicate":
				s = strings.Replace(s, `"method":"heartbeat"`, `"method":"heartbeat","method":"heartbeat"`, 1)
			case "scope":
				s = strings.Replace(s, `"method":"heartbeat"`, `"method":"scope_revoke"`, 1)
			case "old-version":
				s = strings.Replace(s, `"version":2`, `"version":1`, 1)
			case "sync-exec":
				s = strings.Replace(s, `heartbeat`, `exec`, 1)
			case "files":
				s = strings.Replace(s, `heartbeat`, `file_upload_begin`, 1)
			}
			var h [4]byte
			binary.BigEndian.PutUint32(h[:], uint32(len(s)))
			if _, e := c.Write(append(h[:], []byte(s)...)); e != nil {
				t.Fatal(e)
			}
			serialEnded(t, done)
		})
	}
}
func TestSerialV2PolicyAndBoundedJSON(t *testing.T) {
	for _, s := range []string{strings.Repeat("[", 17) + "0" + strings.Repeat("]", 17), `{"a":1,"a":2}`, string([]byte{'"', 255, '"'})} {
		if uniqueSerialJSON([]byte(s)) {
			t.Fatal("accepted", s)
		}
	}
	m := testManager(t)
	a, b := net.Pipe()
	defer b.Close()
	if e := serveSerialExec(a, m); e == nil {
		t.Fatal("legacy containment admitted")
	}
}

type delayedFailCloseBoundary struct {
	jobBoundary
	release <-chan struct{}
}

func (b delayedFailCloseBoundary) close() error {
	<-b.release
	return errors.New("injected asynchronous cleanup failure")
}
func TestSerialV2AsynchronousCleanupFailureFencesWithoutPoll(t *testing.T) {
	m := testManager(t)
	release := make(chan struct{})
	m.boundary = func(c *exec.Cmd) (jobBoundary, error) {
		b, e := legacyBoundary(c)
		return delayedFailCloseBoundary{b, release}, e
	}
	c, done, base := serialExecPair(t, m)
	req := sleepRequest()
	req.Argv = []string{"true"}
	v2Send(t, c, base, 1, req)
	r := v2Read(t, c)
	if r.Response.Error != nil {
		close(release)
		t.Fatal(r)
	}
	start := time.Now()
	close(release)
	serialEnded(t, done)
	if time.Since(start) > time.Second {
		t.Fatal("asynchronous failure waited for heartbeat timeout or poll")
	}
	if m.stopAll(time.Second) {
		t.Fatal("unconfirmed cleanup hidden")
	}
}

func TestSerialV2CLIRejectsUnsafeModesBeforeContainment(t *testing.T) {
	if raw := os.Getenv("VEYNODE_SERIAL_V2_CLI_ARGS"); raw != "" {
		var args []string
		if json.Unmarshal([]byte(raw), &args) != nil {
			panic("invalid test arguments")
		}
		flag.CommandLine = flag.NewFlagSet("serial-v2-cli", flag.ExitOnError)
		os.Args = append([]string{"serial-v2-cli"}, args...)
		main()
		return
	}
	cases := [][]string{{"--virtio-serial-exec"}, {"--virtio-serial-exec", "--allow-execute"}, {"--virtio-serial-exec", "--exec-user=nobody"}}
	for _, extra := range []string{"--allow-files", "--allow-shutdown", "--files-root=/tmp", "--files-user=nobody", "--file-worker", "--legacy-exec-process-groups", "--virtio-serial"} {
		cases = append(cases, []string{"--virtio-serial-exec", "--allow-execute", "--exec-user=nobody", extra})
	}
	for _, args := range cases {
		raw, _ := json.Marshal(args)
		cmd := exec.Command(os.Args[0], "-test.run=^TestSerialV2CLIRejectsUnsafeModesBeforeContainment$")
		cmd.Env = append(os.Environ(), "VEYNODE_SERIAL_V2_CLI_ARGS="+string(raw), "GORACE=atexit_sleep_ms=0")
		out, err := cmd.CombinedOutput()
		if err == nil || (!strings.Contains(string(out), "files/shutdown/legacy forbidden") && !strings.Contains(string(out), "observer-only; privilege options rejected")) {
			t.Fatalf("%v: %v %s", args, err, out)
		}
	}
}

func TestSerialV2InfoAdvertisesExecution(t *testing.T) {
	m := testManager(t)
	c, done, base := serialExecPair(t, m)
	v2Send(t, c, base, 1, request{ID: "info", Method: "info"})
	reply := v2Read(t, c)
	value := reply.Response.Result.(map[string]any)
	if value["transport"] != "virtio-serial-private-exec-v2" || value["transport_reconnect"] != false || value["execution_containment"] != "cgroup-v2-clone3-kill" {
		t.Fatal(value)
	}
	caps := value["capabilities"].(map[string]any)
	for _, k := range []string{"exec", "async_exec", "descendant_containment"} {
		if caps[k] != true {
			t.Fatal(k, caps)
		}
	}
	for _, k := range []string{"files", "shutdown", "desktop"} {
		if caps[k] != false {
			t.Fatal(k, caps)
		}
	}
	if value["transport_heartbeat_timeout_ms"] != float64(2000) {
		t.Fatal(value)
	}
	c.Close()
	serialEnded(t, done)
}

// Test executable only, never a shipped CLI option. Protocol interoperability
// with real host subprocesses is deliberately not a Linux cgroup acceptance test.
func TestSerialExecInteropProcess(t *testing.T) {
	path := os.Getenv("VEYNODE_SERIAL_EXEC_SOCKET")
	if path == "" {
		return
	}
	if !filepath.IsAbs(path) {
		t.Fatal("absolute test socket required")
	}
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: path, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	if err = listener.SetDeadline(time.Now().Add(20 * time.Second)); err != nil {
		t.Fatal(err)
	}
	c, err := listener.AcceptUnix()
	if err != nil {
		t.Fatal(err)
	}
	m := testManager(t)
	if m.identity.UID == 0 {
		c.Close()
		t.Fatal("interop fixture requires nonroot host user")
	}
	m.identity.Rootless = true
	m.containmentMode = "cgroup-v2-clone3-kill"
	done := make(chan error, 1)
	go func() { done <- serveSerialExec(c, m) }()
	select {
	case err = <-done:
		if err == nil {
			t.Fatal("EOF must terminate incarnation")
		}
	case <-time.After(20 * time.Second):
		c.Close()
		if !m.stopAll(time.Second) {
			t.Error("test cleanup unconfirmed")
		}
		t.Fatal("interop exceeded20s")
	}
	if !m.stopAll(time.Second) {
		t.Fatal("test subprocesses not cleaned")
	}
}

func TestSerialV2RejectsSecondOccupiedOperationLane(t *testing.T) {
	m := testManager(t)
	entered, release := make(chan struct{}), make(chan struct{})
	var once sync.Once
	t.Cleanup(func() { once.Do(func() { close(release) }) })
	m.boundary = func(c *exec.Cmd) (jobBoundary, error) { close(entered); <-release; return legacyBoundary(c) }
	c, done, base := serialExecPair(t, m)
	v2Send(t, c, base, 1, sleepRequest())
	<-entered
	v2Send(t, c, base, 2, request{ID: "info", Method: "info"})
	until := time.Now().Add(time.Second)
	fenced := false
	for time.Now().Before(until) {
		m.mu.Lock()
		fenced = m.stopping
		m.mu.Unlock()
		if fenced {
			break
		}
		time.Sleep(time.Millisecond)
	}
	once.Do(func() { close(release) })
	if !fenced {
		t.Fatal("second occupied lane was not fenced")
	}
	serialEnded(t, done)
	if !m.stopAll(time.Second) {
		t.Fatal("real pending command did not clean")
	}
}
