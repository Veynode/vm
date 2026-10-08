package main

import (
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestContainmentSetupFailureDoesNotExecute(t *testing.T) {
	manager := testManager(t)
	marker := filepath.Join(t.TempDir(), "must-not-execute")
	manager.boundary = func(*exec.Cmd) (jobBoundary, error) { return nil, errors.New("unsupported atomic containment") }
	reply := manager.execute(request{ID: "failure", Argv: []string{"/bin/sh", "-c", "printf unsafe > \"$1\"", "test", marker}, TimeoutMS: 1000})
	if reply.Error == nil || !strings.Contains(reply.Error.Message, "containment unavailable") {
		t.Fatal(reply)
	}
	if _, err := os.Stat(marker); !os.IsNotExist(err) {
		t.Fatal("ran before containment setup", err)
	}
}

type failingCleanupBoundary struct{ jobBoundary }

func (b failingCleanupBoundary) terminate() error {
	_ = b.jobBoundary.terminate()
	return errors.New("simulated cgroup kill failure")
}
func TestContainmentCleanupFailureCannotReportSuccess(t *testing.T) {
	manager := testManager(t)
	manager.boundary = func(cmd *exec.Cmd) (jobBoundary, error) {
		b, _ := legacyBoundary(cmd)
		return failingCleanupBoundary{b}, nil
	}
	reply := manager.execute(request{ID: "cleanup", Argv: []string{"true"}, TimeoutMS: 1000})
	if reply.Error == nil || reply.Error.Code != "containment_failed" {
		t.Fatal(reply)
	}
	if manager.stopAll(time.Second) {
		t.Fatal("service claimed cleanup success after boundary failure")
	}
	denied := manager.execute(request{ID: "after-failure", Argv: []string{"true"}, TimeoutMS: 1000})
	if denied.Error == nil || denied.Error.Code != "busy" {
		t.Fatal("admitted command after containment failure", denied)
	}

}
func TestLegacyBoundaryNeverClaimsDescendantContainment(t *testing.T) {
	manager := testManager(t)
	info := handleWithManager(manager, request{ID: "info", Method: "info"}, true).Result.(map[string]any)
	if info["execution_containment"] != "legacy-process-group-not-contained" || info["capabilities"].(map[string]bool)["descendant_containment"] {
		t.Fatal(info)
	}
}
