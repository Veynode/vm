package main

import (
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func testManager(t *testing.T) *jobManager {
	t.Helper()
	manager := newJobManager(time.Now)
	t.Cleanup(func() {
		manager.mu.Lock()
		for _, job := range manager.jobs {
			if !job.finished && !job.cancelRequested {
				job.cancelRequested = true
				job.control.requestCancel()
			}
		}
		manager.mu.Unlock()
		deadline := time.Now().Add(time.Second)
		for time.Now().Before(deadline) {
			manager.mu.Lock()
			active := manager.active
			manager.mu.Unlock()
			if active == 0 {
				return
			}
			time.Sleep(time.Millisecond)
		}
		t.Error("test left commands active")
	})
	return manager
}
func startTestJob(t *testing.T, manager *jobManager, argv []string, timeout int) string {
	t.Helper()
	reply := handleWithManager(manager, request{ID: "start", Method: "exec_start", Argv: argv, TimeoutMS: timeout}, true)
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	result := reply.Result.(map[string]any)
	if result["state"] != "running" {
		t.Fatal(result)
	}
	id := result["job_id"].(string)
	if !validJobID(id) {
		t.Fatal("invalid job identifier", id)
	}
	return id
}
func waitTestJob(t *testing.T, manager *jobManager, id string) map[string]any {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		reply := handleWithManager(manager, request{ID: "poll", Method: "exec_poll", JobID: id}, true)
		if reply.Error != nil {
			t.Fatal(reply.Error)
		}
		state := reply.Result.(map[string]any)
		if state["state"] == "finished" {
			if err, ok := state["error"]; ok {
				t.Fatal(err)
			}
			return state["result"].(map[string]any)
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("job never finished")
	return nil
}
func TestJobsExecutionPermissionAndInfo(t *testing.T) {
	manager := testManager(t)
	for _, method := range []string{"exec_start", "exec_poll", "exec_cancel"} {
		reply := handleWithManager(manager, request{ID: "1", Method: method}, false)
		if reply.Error == nil || reply.Error.Code != "forbidden" {
			t.Fatal(method, reply)
		}
	}
	for _, allow := range []bool{false, true} {
		info := handleWithManager(manager, request{ID: "i", Method: "info"}, allow).Result.(map[string]any)
		if info["capabilities"].(map[string]bool)["async_exec"] != allow {
			t.Fatal(info)
		}
	}
}
func TestAsyncOutputAndNonzero(t *testing.T) {
	manager := testManager(t)
	id := startTestJob(t, manager, []string{"/bin/sh", "-c", "printf '你好'; printf error >&2; exit 7"}, 1000)
	result := waitTestJob(t, manager, id)
	if result["stdout"] != "你好" || result["stderr"] != "error" || result["exit_code"] != 7 || result["cancelled"] != false || result["timed_out"] != false {
		t.Fatal(result)
	}
	id = startTestJob(t, manager, []string{"/bin/sh", "-c", "yes x | head -c 100000"}, 1000)
	result = waitTestJob(t, manager, id)
	if len(result["stdout"].(string)) != maxOutput || result["stdout_truncated"] != true {
		t.Fatal(result)
	}
}
func TestCancelFiveSecondJobKillsDescendantsWithinOneSecond(t *testing.T) {
	manager := testManager(t)
	directory := t.TempDir()
	ready := filepath.Join(directory, "ready")
	leak := filepath.Join(directory, "leak")
	id := startTestJob(t, manager, []string{"/bin/sh", "-c", "(sleep 0.4; printf escaped > \"$2\") & sleep 5 & printf ready > \"$1\"; wait", "test", ready, leak}, 5000)
	deadline := time.Now().Add(time.Second)
	for {
		if _, err := os.Stat(ready); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("child not ready")
		}
		time.Sleep(time.Millisecond)
	}
	started := time.Now()
	reply := handleWithManager(manager, request{ID: "cancel", Method: "exec_cancel", JobID: id}, true)
	if reply.Error != nil || reply.Result.(map[string]any)["cancel_requested"] != true {
		t.Fatal(reply)
	}
	result := waitTestJob(t, manager, id)
	if time.Since(started) >= time.Second || result["cancelled"] != true || result["timed_out"] != false {
		t.Fatal(result, time.Since(started))
	}
	time.Sleep(500 * time.Millisecond)
	if _, err := os.Stat(leak); !os.IsNotExist(err) {
		t.Fatal("cancelled descendant wrote after cancellation", err)
	}
	reply = handleWithManager(manager, request{ID: "cancel", Method: "exec_cancel", JobID: id}, true)
	if reply.Result.(map[string]any)["cancel_requested"] != false {
		t.Fatal("finished job cancellation was accepted")
	}
}
func TestAsyncTimeoutDistinctFromCancellation(t *testing.T) {
	manager := testManager(t)
	id := startTestJob(t, manager, []string{"/bin/sh", "-c", "sleep 5 & wait"}, 40)
	result := waitTestJob(t, manager, id)
	if result["timed_out"] != true || result["cancelled"] != false {
		t.Fatal(result)
	}
}
func TestSyncAndAsyncShareAdmissionBudget(t *testing.T) {
	manager := testManager(t)
	for i := 0; i < 3; i++ {
		startTestJob(t, manager, []string{"/bin/sh", "-c", "sleep 5 & wait"}, 5000)
	}
	done := make(chan response, 1)
	go func() {
		done <- handleWithManager(manager, request{ID: "sync", Method: "exec", Argv: []string{"/bin/sh", "-c", "sleep 5 & wait"}, TimeoutMS: 150}, true)
	}()
	deadline := time.Now().Add(time.Second)
	for {
		manager.mu.Lock()
		active := manager.active
		manager.mu.Unlock()
		if active == 4 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("sync did not consume admission")
		}
		time.Sleep(time.Millisecond)
	}
	for _, method := range []string{"exec", "exec_start"} {
		reply := handleWithManager(manager, request{ID: "overload", Method: method, Argv: []string{"/bin/true"}, TimeoutMS: 100}, true)
		if reply.Error == nil || reply.Error.Code != "busy" {
			t.Fatal("overload not rejected", reply)
		}
	}
	// Poll/cancel remain available while every command slot is occupied.
	manager.mu.Lock()
	var id string
	for candidate := range manager.jobs {
		id = candidate
		break
	}
	manager.mu.Unlock()
	if reply := manager.poll(request{ID: "poll", JobID: id}); reply.Error != nil {
		t.Fatal(reply)
	}
	if reply := manager.cancel(request{ID: "cancel", JobID: id}); reply.Error != nil {
		t.Fatal(reply)
	}
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("synchronous command stuck")
	}
}
func TestFinishedJobsTTLAndCapacityNeverEvictActive(t *testing.T) {
	manager := testManager(t)
	var clock atomic.Int64
	clock.Store(time.Now().UnixNano())
	manager.now = func() time.Time { return time.Unix(0, clock.Load()) }
	active := startTestJob(t, manager, []string{"/bin/sh", "-c", "sleep 5 & wait"}, 5000)
	oldest := ""
	for i := 0; i < maxFinishedJobs+3; i++ {
		id := startTestJob(t, manager, []string{"/usr/bin/true"}, 1000)
		if oldest == "" {
			oldest = id
		}
		waitTestJob(t, manager, id)
	}
	manager.mu.Lock()
	count := len(manager.jobs)
	_, stillActive := manager.jobs[active]
	manager.mu.Unlock()
	if count != maxFinishedJobs+1 || !stillActive {
		t.Fatal("cache count or active job damaged", count, stillActive)
	}
	if reply := manager.poll(request{ID: "expired", JobID: oldest}); reply.Error == nil || reply.Error.Code != "not_found" {
		t.Fatal("oldest result not evicted", reply)
	}
	clock.Add(int64(finishedJobTTL))
	if reply := manager.poll(request{ID: "active", JobID: active}); reply.Error != nil || reply.Result.(map[string]any)["state"] != "running" {
		t.Fatal("TTL evicted active job", reply)
	}
	manager.mu.Lock()
	count = len(manager.jobs)
	manager.mu.Unlock()
	if count != 1 {
		t.Fatal("finished jobs not expired at TTL", count)
	}
}
func TestJobStartFailureDoesNotLeakAdmissionAndInvalidIDs(t *testing.T) {
	manager := testManager(t)
	for i := 0; i < 8; i++ {
		reply := manager.start(request{ID: "bad", Argv: []string{"/veynode-definitely-missing"}, TimeoutMS: 100})
		if reply.Error == nil || reply.Error.Code != "exec_failed" {
			t.Fatal(reply)
		}
	}
	manager.mu.Lock()
	active := manager.active
	manager.mu.Unlock()
	if active != 0 {
		t.Fatal("admission leak", active)
	}
	for _, id := range []string{"", strings.Repeat("z", 32), strings.Repeat("0", 31)} {
		if reply := manager.poll(request{ID: "p", JobID: id}); reply.Error == nil || reply.Error.Code != "invalid_request" {
			t.Fatal(reply)
		}
	}
	if reply := manager.cancel(request{ID: "missing", JobID: strings.Repeat("0", 32)}); reply.Error == nil || reply.Error.Code != "not_found" {
		t.Fatal(reply)
	}
}
