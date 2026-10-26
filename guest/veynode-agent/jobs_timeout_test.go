//go:build !windows

package main

import (
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"os/user"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestCommandTimeoutPolicyBoundaries(t *testing.T) {
	for _, test := range []struct {
		os, mode string
		maximum  int
	}{
		{"linux", "cgroup-v2-clone3-kill", 120000},
		{"linux", "legacy-process-group-not-contained", 30000},
		{"linux", "unavailable", 30000},
		{"darwin", "cgroup-v2-clone3-kill", 30000},
		{"windows", "cgroup-v2-clone3-kill", 30000},
	} {
		t.Run(test.os+"/"+test.mode, func(t *testing.T) {
			maximum := asyncCommandTimeoutMS(test.os, test.mode)
			if maximum != test.maximum {
				t.Fatalf("maximum %d, want %d", maximum, test.maximum)
			}
			for _, timeout := range []int{-1, 0, 1, 30000, 30001, 90000, 120000, 120001} {
				err := validateCommandTimeout(request{Argv: []string{"/bin/true"}, TimeoutMS: timeout}, maximum)
				if (err == nil) != (timeout >= 1 && timeout <= test.maximum) {
					t.Fatalf("timeout %d: %v", timeout, err)
				}
			}
		})
	}
}

func TestSyncAndLegacyNeverAdmitLongTimeout(t *testing.T) {
	manager := testManager(t)
	starts := 0
	manager.boundary = func(*exec.Cmd) (jobBoundary, error) {
		starts++
		return nil, errors.New("invalid requests must not reach process containment")
	}
	for _, mode := range []string{"legacy-process-group-not-contained", "cgroup-v2-clone3-kill"} {
		manager.containmentMode = mode
		for _, method := range []string{"exec", "exec_start"} {
			for _, timeout := range []int{30001, 90000, 120000, 120001} {
				// Sync stays bounded even if Method is changed or reused internally.
				reply := manager.execute(request{ID: "sync", Method: method, Argv: []string{"/bin/true"}, TimeoutMS: timeout})
				if reply.Error == nil || reply.Error.Code != "invalid_request" {
					t.Fatal(mode, method, timeout, reply)
				}
			}
		}
	}
	manager.containmentMode = "legacy-process-group-not-contained"
	for _, timeout := range []int{30001, 90000, 120000, 120001} {
		reply := manager.start(request{ID: "legacy", Method: "exec_start", Argv: []string{"/bin/true"}, TimeoutMS: timeout})
		if reply.Error == nil || reply.Error.Code != "invalid_request" {
			t.Fatal(timeout, reply)
		}
	}
	if starts != 0 || manager.active != 0 || len(manager.jobs) != 0 {
		t.Fatal("invalid command reached admission or process creation", starts, manager.active)
	}
}

func TestAsyncTimeoutInfoAndSerialPreflightAgree(t *testing.T) {
	manager := testManager(t)
	for _, mode := range []string{"legacy-process-group-not-contained", "cgroup-v2-clone3-kill"} {
		// This mode assignment is a policy seam, not Linux containment proof.
		manager.containmentMode = mode
		maximum := asyncCommandTimeoutMS(runtime.GOOS, mode)
		for _, allowed := range []bool{false, true} {
			info := handleWithManager(manager, request{ID: "info", Method: "info"}, allowed).Result.(map[string]any)
			if info["max_timeout_ms"] != 30000 || info["max_async_timeout_ms"] != maximum {
				t.Fatal(mode, info)
			}
		}
		for _, timeout := range []int{0, 30000, 30001, 90000, 120000, 120001} {
			req := request{ID: "start", Method: "exec_start", Argv: []string{"/bin/true"}, TimeoutMS: timeout}
			envelope := serialEnvelope{Version: 2, Type: "request", GuestNonce: strings.Repeat("a", 64),
				HostNonce: strings.Repeat("b", 64), Sequence: 1, Request: &req}
			wire, err := json.Marshal(envelope)
			if err != nil {
				t.Fatal(err)
			}
			envelope.wire = wire
			want := timeout >= 1 && timeout <= maximum
			if err := manager.validateSerialV2(envelope, false); (err == nil) != want {
				t.Fatal(mode, timeout, "serial preflight", err)
			}
			if err := manager.validateAsyncCommand(req); (err == nil) != want {
				t.Fatal(mode, timeout, "manager admission", err)
			}
		}
	}
}

func TestRealContainedLongAsyncImmediateCancel(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("requires a real root Linux cgroup-v2/clone3 Guest; Darwin policy tests are not containment acceptance")
	}
	if os.Getenv("VEYNODE_TEST_REAL_CGROUP") != "1" || os.Geteuid() != 0 {
		t.Skip("opt in only in a dedicated root Linux test Guest with VEYNODE_TEST_REAL_CGROUP=1")
	}
	account, err := user.Lookup("nobody")
	if err != nil {
		t.Fatal(err)
	}
	account.HomeDir = "/tmp"
	identity, err := identityForAccount(account, 0)
	if err != nil {
		t.Fatal(err)
	}
	factory, mode, err := configureCommandContainment(identity, false)
	if err != nil {
		t.Fatal("real containment startup probe failed", err)
	}
	manager := testManager(t)
	manager.identity, manager.boundary, manager.containmentMode = identity, factory, mode
	for _, timeout := range []int{90000, 120000} {
		req := request{ID: "long", Method: "exec_start", Argv: []string{"/bin/sh", "-c", "sleep 120 & wait"}, TimeoutMS: timeout}
		reply := manager.start(req)
		if reply.Error != nil {
			t.Fatal("contained asynchronous start rejected", timeout, reply.Error)
		}
		id := reply.Result.(map[string]any)["job_id"].(string)
		cancel := manager.cancel(request{ID: "cancel", JobID: id})
		if cancel.Error != nil || cancel.Result.(map[string]any)["cancel_requested"] != true {
			t.Fatal(cancel)
		}
		deadline := time.Now().Add(2 * time.Second)
		for {
			poll := manager.poll(request{ID: "poll", JobID: id})
			if poll.Error != nil {
				t.Fatal(poll.Error)
			}
			state := poll.Result.(map[string]any)
			if state["state"] == "finished" {
				result, ok := state["result"].(map[string]any)
				if !ok || result["cancelled"] != true || result["timed_out"] != false {
					t.Fatal("long async cancellation/cleanup unconfirmed", state)
				}
				break
			}
			if time.Now().After(deadline) {
				t.Fatal("long asynchronous cancellation exceeded bounded cleanup")
			}
			time.Sleep(time.Millisecond)
		}
		if manager.cleanupFailed {
			t.Fatal("long job cleanup unconfirmed")
		}
		entries, err := os.ReadDir("/sys/fs/cgroup/veynode-exec")
		if err != nil {
			t.Fatal(err)
		}
		for _, entry := range entries {
			if entry.IsDir() && strings.HasPrefix(entry.Name(), "job-") {
				t.Fatal("long job cgroup remained after confirmed cancellation", entry.Name())
			}
		}
	}
}

func TestSerialPreflightAcceptsExecIdentity(t *testing.T) {
	manager := testManager(t)
	envelope := func(req request) serialEnvelope {
		e := serialEnvelope{Version: 2, Type: "request", GuestNonce: strings.Repeat("a", 64),
			HostNonce: strings.Repeat("b", 64), Sequence: 1, Request: &req}
		wire, err := json.Marshal(e)
		if err != nil {
			t.Fatal(err)
		}
		e.wire = wire
		return e
	}
	base := request{ID: "start", Method: "exec_start", Argv: []string{"id"}, TimeoutMS: 1000}
	withIdentity := base
	withIdentity.User, withIdentity.Env, withIdentity.Workdir = "veynode", []string{"A=1"}, "/tmp"
	for _, req := range []request{base, withIdentity} {
		if err := manager.validateSerialV2(envelope(req), false); err != nil {
			t.Fatal(req, err)
		}
	}
	// Other fields still fence.
	e := envelope(base)
	e.wire = []byte(strings.Replace(string(e.wire), `"timeout_ms"`, `"shell":true,"timeout_ms"`, 1))
	if manager.validateSerialV2(e, false) == nil {
		t.Fatal("unknown exec_start field accepted")
	}
}
