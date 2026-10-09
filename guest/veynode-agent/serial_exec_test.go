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

func scopedRequest() request {
	return request{ID: "start", Method: "exec_start", Argv: []string{"/bin/sh", "-c", "sleep 5"}, TimeoutMS: 10000}
}
func scopeWait(t *testing.T, m *jobManager, key string, predicate func(*commandScopeState) bool) {
	t.Helper()
	until := time.Now().Add(time.Second)
	for time.Now().Before(until) {
		m.mu.Lock()
		state := m.scopes[key]
		ok := state != nil && predicate(state)
		m.mu.Unlock()
		if ok {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("scope state not reached")
}
func TestScopePendingStartRetirementAndLostReply(t *testing.T) {
	m := testManager(t)
	entered, unblock := make(chan struct{}), make(chan struct{})
	m.boundary = func(cmd *exec.Cmd) (jobBoundary, error) { close(entered); <-unblock; return legacyBoundary(cmd) }
	started := make(chan response, 1)
	go func() { started <- m.startScoped(scopedRequest(), "owner:1") }()
	<-entered
	revoked := make(chan bool, 1)
	go func() { revoked <- m.revokeScope("owner:1", time.Second) }()
	scopeWait(t, m, "owner:1", func(s *commandScopeState) bool { return s.retired && s.starting == 1 && s.active == 1 })
	select {
	case <-revoked:
		t.Fatal("pending start falsely acknowledged clean")
	default:
	}
	if r := m.startScoped(scopedRequest(), "owner:1"); r.Error == nil || r.Error.Code != "scope_retired" {
		t.Fatal(r)
	}
	close(unblock)
	r := <-started
	if r.Error != nil {
		t.Fatal(r)
	}
	// Deliberately discard the start reply as a caller would after losing its ACK.
	if !<-revoked {
		t.Fatal("real process cancellation did not drain")
	}
	if !m.revokeScope("owner:1", time.Second) {
		t.Fatal("repeat clean scope changed")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.active != 0 || m.scopes["owner:1"].starting != 0 {
		t.Fatal("active remained")
	}
	for _, j := range m.jobs {
		if !j.finished || j.result["cancelled"] != true {
			t.Fatal("cancelled completion missing", j)
		}
	}
}
func TestScopesIsolateJobsAndNeverForgetRetirement(t *testing.T) {
	m := testManager(t)
	r := m.startScoped(scopedRequest(), "a:1")
	if r.Error != nil {
		t.Fatal(r)
	}
	id := r.Result.(map[string]any)["job_id"].(string)
	for _, r := range []response{m.pollScoped(request{JobID: id}, "b:1"), m.cancelScoped(request{JobID: id}, "a:2"), m.poll(request{JobID: id})} {
		if r.Error == nil || r.Error.Code != "not_found" {
			t.Fatal(r)
		}
	}
	if !m.revokeScope("a:1", time.Second) {
		t.Fatal("cleanup")
	}
	if !m.revokeScope("never-started:1", time.Second) {
		t.Fatal("unknown scope retirement")
	}
	if r := m.startScoped(scopedRequest(), "never-started:1"); r.Error == nil || r.Error.Code != "scope_retired" {
		t.Fatal(r)
	}
	r = m.startScoped(scopedRequest(), "a:2")
	if r.Error != nil {
		t.Fatal(r)
	}
	if !m.revokeScope("a:2", time.Second) {
		t.Fatal("new epoch cleanup")
	}
	for i := len(m.scopes); i < maxCommandScopes; i++ {
		if !m.revokeScope(fmt.Sprintf("retired:%d", i), time.Second) {
			t.Fatal(i)
		}
	}
	if r := m.startScoped(scopedRequest(), "over-capacity"); r.Error == nil || r.Error.Code != "busy" {
		t.Fatal(r)
	}
	if r := m.startScoped(scopedRequest(), "a:1"); r.Error == nil || r.Error.Code != "scope_retired" {
		t.Fatal(r)
	}
	if m.revokeScope("cannot-record", time.Second) || m.stopAll(time.Second) {
		t.Fatal("unrecordable retirement must fence")
	}
}
func TestScopesShareFourCommandBudget(t *testing.T) {
	m := testManager(t)
	for i := 0; i < 4; i++ {
		if r := m.startScoped(scopedRequest(), fmt.Sprint(i)); r.Error != nil {
			t.Fatal(r)
		}
	}
	if r := m.startScoped(scopedRequest(), "fifth"); r.Error == nil || r.Error.Code != "busy" {
		t.Fatal(r)
	}
	if !m.stopAll(time.Second) {
		t.Fatal("cleanup")
	}
}

type failingCloseBoundary struct{ jobBoundary }

func (b failingCloseBoundary) close() error { return errors.New("injected remove failure") }
func TestScopedCleanupFailureCannotBecomeClean(t *testing.T) {
	m := testManager(t)
	m.boundary = func(c *exec.Cmd) (jobBoundary, error) { b, e := legacyBoundary(c); return failingCloseBoundary{b}, e }
	if r := m.startScoped(scopedRequest(), "scope"); r.Error != nil {
		t.Fatal(r)
	}
	if m.revokeScope("scope", time.Second) || m.revokeScope("scope", time.Second) || m.stopAll(time.Second) {
		t.Fatal("failed containment became clean")
	}
	if r := m.startScoped(scopedRequest(), "next"); r.Error == nil {
		t.Fatal("failure admitted start")
	}
}
func TestFailedStartCleanupFencesSyncAndScoped(t *testing.T) {
	for _, scoped := range []bool{false, true} {
		t.Run(fmt.Sprint(scoped), func(t *testing.T) {
			m := testManager(t)
			m.boundary = func(c *exec.Cmd) (jobBoundary, error) {
				c.Path = "/nonexistent-veynode-executable"
				b, e := legacyBoundary(c)
				return failingCloseBoundary{b}, e
			}
			req := scopedRequest()
			var r response
			if scoped {
				r = m.startScoped(req, "scope")
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
func TestSerialV2HeartbeatDuringPendingStartAndRevoke(t *testing.T) {
	m := testManager(t)
	entered, unblock := make(chan struct{}), make(chan struct{})
	m.boundary = func(c *exec.Cmd) (jobBoundary, error) { close(entered); <-unblock; return legacyBoundary(c) }
	c, done, base := serialExecPair(t, m)
	scope := &serialScope{strings.Repeat("a", 64), 1}
	r := scopedRequest()
	r.Scope = scope
	v2Send(t, c, base, 1, r)
	<-entered
	v2Send(t, c, base, 2, request{ID: "revoke", Method: "scope_revoke", Scope: scope})
	key, _ := scope.key()
	scopeWait(t, m, key, func(s *commandScopeState) bool { return s.retired && s.starting == 1 })
	v2Send(t, c, base, 3, request{ID: "heart", Method: "heartbeat"})
	heartbeat := v2Read(t, c)
	if heartbeat.Sequence != 3 || heartbeat.Response.ID != "heart" {
		t.Fatal("heartbeat blocked by start", heartbeat)
	}
	close(unblock)
	seen := map[uint64]bool{}
	for i := 0; i < 2; i++ {
		r := v2Read(t, c)
		if r.Response.Error != nil {
			t.Fatal(r)
		}
		seen[r.Sequence] = true
		if r.Sequence == 2 {
			b, _ := json.Marshal(r.Response.Result)
			if string(b) != "{\"cgroup_empty\":true,\"pending_starts\":0,\"revoked\":true}" {
				t.Fatal(string(b))
			}
		}
	}
	if !seen[1] || !seen[2] {
		t.Fatal(seen)
	}
	c.Close()
	serialEnded(t, done)
}
func TestSerialV2WatchdogCancelsRealJob(t *testing.T) {
	m := testManager(t)
	c, done, base := serialExecPair(t, m)
	r := scopedRequest()
	r.Scope = &serialScope{strings.Repeat("a", 64), 1}
	v2Send(t, c, base, 1, r)
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
	for _, kind := range []string{"sequence", "nonce", "unknown", "duplicate", "scope-extra", "fractional-epoch", "old-version", "sync-exec", "files"} {
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
			case "scope-extra", "fractional-epoch":
				scope := `{"owner":"` + strings.Repeat("a", 64) + `","epoch":1,"extra":1}`
				if kind == "fractional-epoch" {
					scope = `{"owner":"` + strings.Repeat("a", 64) + `","epoch":1.1}`
				}
				s = strings.Replace(s, `"method":"heartbeat"`, `"method":"scope_revoke","scope":`+scope, 1)
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
	for _, scope := range []*serialScope{nil, {strings.Repeat("a", 64), 0}, {strings.Repeat("A", 64), 1}, {strings.Repeat("a", 64), maxScopeEpoch + 1}} {
		if _, e := scope.key(); e == nil {
			t.Fatal(scope)
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
	req := scopedRequest()
	req.Argv = []string{"true"}
	req.Scope = &serialScope{strings.Repeat("a", 64), 1}
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

func TestSerialV2InfoAdvertisesOnlyScopedExecution(t *testing.T) {
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
	if value["transport_max_scopes"] != float64(1024) || value["transport_heartbeat_timeout_ms"] != float64(2000) {
		t.Fatal(value)
	}
	c.Close()
	serialEnded(t, done)
}
func TestSerialV1RejectsNewScope(t *testing.T) {
	c, done := serialPair(t)
	r := serialHost(t, c)
	r.Type = "request"
	r.Sequence = 1
	r.Request = &request{ID: "info", Method: "info", Scope: &serialScope{strings.Repeat("a", 64), 1}}
	if err := serialWrite(c, r); err != nil {
		t.Fatal(err)
	}
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
	for _, lane := range []string{"ordinary", "revoke"} {
		t.Run(lane, func(t *testing.T) {
			m := testManager(t)
			entered, release := make(chan struct{}), make(chan struct{})
			var once sync.Once
			t.Cleanup(func() { once.Do(func() { close(release) }) })
			m.boundary = func(c *exec.Cmd) (jobBoundary, error) { close(entered); <-release; return legacyBoundary(c) }
			c, done, base := serialExecPair(t, m)
			scope := &serialScope{strings.Repeat("a", 64), 1}
			req := scopedRequest()
			req.Scope = scope
			v2Send(t, c, base, 1, req)
			<-entered
			if lane == "ordinary" {
				v2Send(t, c, base, 2, request{ID: "info", Method: "info"})
			} else {
				v2Send(t, c, base, 2, request{ID: "revoke1", Method: "scope_revoke", Scope: scope})
				key, _ := scope.key()
				scopeWait(t, m, key, func(s *commandScopeState) bool { return s.retired })
				v2Send(t, c, base, 3, request{ID: "revoke2", Method: "scope_revoke", Scope: scope})
			}
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
		})
	}
}
