package main

import (
	"encoding/json"
	"os"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

func TestExecutionIdentityRejectsPrivilegeAndInvalidHome(t *testing.T) {
	directory := t.TempDir()
	account := user.User{Username: "veynode", Uid: "1001", Gid: "1001", HomeDir: directory}
	identity, err := identityForAccount(&account, 0)
	if err != nil {
		t.Fatal(err)
	}
	if identity.UID != 1001 || identity.GID != 1001 || !identity.Rootless || identity.CWD != directory || identity.credential == nil || len(identity.credential.Groups) != 0 || identity.credential.NoSetGroups {
		t.Fatal(identity)
	}
	for _, change := range []func(*user.User){func(u *user.User) { u.Uid = "0" }, func(u *user.User) { u.Gid = "0" }, func(u *user.User) { u.Uid = "-1" }, func(u *user.User) { u.HomeDir = "relative" }, func(u *user.User) { u.HomeDir = directory + "/missing" }} {
		bad := account
		change(&bad)
		if _, err := identityForAccount(&bad, 0); err == nil {
			t.Fatal("accepted invalid account", bad)
		}
	}
	if _, err := identityForAccount(&account, 501); err == nil {
		t.Fatal("non-root service cannot clear supplementary groups")
	}
	if _, err := resolveExecutionIdentity("veynode-account-that-does-not-exist"); err == nil {
		t.Fatal("unknown account accepted")
	}
}
func TestExecutionIdentityAppliesHomeUserAndDirectory(t *testing.T) {
	manager := testManager(t)
	directory := t.TempDir()
	manager.identity.Home = directory
	manager.identity.CWD = directory
	manager.identity.User = "test-fixed-user"
	reply := manager.execute(request{ID: "1", Argv: []string{"/bin/sh", "-c", "printf '%s\\n%s\\n%s' \"$HOME\" \"$USER\" \"$PWD\""}, TimeoutMS: 1000})
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	canonical, err := filepath.EvalSymlinks(directory)
	if err != nil {
		t.Fatal(err)
	}
	if reply.Result.(map[string]any)["stdout"] != directory+"\ntest-fixed-user\n"+canonical {
		t.Fatal(reply)
	}
	info := handleWithManager(manager, request{ID: "info", Method: "info"}, true).Result.(map[string]any)
	data, err := json.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(data), `"execution_identity"`) || strings.Contains(string(data), `"credential"`) {
		t.Fatal(string(data))
	}
}
func TestBareExecutableDoesNotUseServicePATH(t *testing.T) {
	directory := t.TempDir()
	fake := filepath.Join(directory, "true")
	if err := os.WriteFile(fake, []byte("#!/bin/sh\nprintf COMPROMISED"), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", directory)
	manager := testManager(t)
	reply := manager.execute(request{ID: "1", Argv: []string{"true"}, TimeoutMS: 1000})
	if reply.Error != nil || reply.Result.(map[string]any)["stdout"] != "" {
		t.Fatal(reply)
	}
}
func TestCurrentIdentityReportsActualPrivilege(t *testing.T) {
	identity := currentExecutionIdentity()
	if identity.UID != uint32(os.Geteuid()) || identity.GID != uint32(os.Getegid()) || identity.Rootless != (os.Geteuid() != 0) {
		t.Fatal(identity)
	}
	if identity.User == "" || identity.Home == "" {
		t.Fatal(identity)
	}
}
func TestActualCredentialDropWhenRoot(t *testing.T) {
	if os.Geteuid() != 0 {
		t.Skip("actual UID/GID drop requires a privileged Linux Guest test; host is unprivileged")
	}
	account, err := user.Lookup("nobody")
	if err != nil {
		t.Skip("nobody account unavailable")
	}
	// /tmp is accessible for this test even if the system nobody home does not exist.
	account.HomeDir = "/tmp"
	identity, err := identityForAccount(account, 0)
	if err != nil {
		t.Fatal(err)
	}
	manager := testManager(t)
	manager.identity = identity
	reply := manager.execute(request{ID: "drop", Argv: []string{"/bin/sh", "-c", "id -u; id -g; grep '^Groups:' /proc/self/status"}, TimeoutMS: 1000})
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	lines := strings.Split(reply.Result.(map[string]any)["stdout"].(string), "\n")
	if lines[0] != strconv.FormatUint(uint64(identity.UID), 10) || lines[1] != strconv.FormatUint(uint64(identity.GID), 10) || strings.TrimSpace(strings.TrimPrefix(lines[2], "Groups:")) != "" {
		t.Fatal(lines)
	}
}
func TestServiceStopCancelsActiveSyncAndAsyncAndRejectsNewCommands(t *testing.T) {
	manager := testManager(t)
	directory := t.TempDir()
	asyncReady := filepath.Join(directory, "async-ready")
	syncReady := filepath.Join(directory, "sync-ready")
	leak := filepath.Join(directory, "leak")
	script := "(sleep 0.4; printf leaked >> \"$2\") & sleep 5 & printf ready > \"$1\"; wait"
	id := startTestJob(t, manager, []string{"/bin/sh", "-c", script, "test", asyncReady, leak}, 5000)
	syncDone := make(chan response, 1)
	go func() {
		syncDone <- manager.execute(request{ID: "sync", Argv: []string{"/bin/sh", "-c", script, "test", syncReady, leak}, TimeoutMS: 5000})
	}()
	deadline := time.Now().Add(time.Second)
	for {
		_, a := os.Stat(asyncReady)
		_, b := os.Stat(syncReady)
		if a == nil && b == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("commands not ready")
		}
		time.Sleep(time.Millisecond)
	}
	if !manager.stopAll(time.Second) {
		t.Fatal("bounded service cleanup did not finish")
	}
	if result := waitTestJob(t, manager, id); result["cancelled"] != true {
		t.Fatal(result)
	}
	select {
	case reply := <-syncDone:
		if reply.Error != nil || reply.Result.(map[string]any)["cancelled"] != true {
			t.Fatal(reply)
		}
	case <-time.After(time.Second):
		t.Fatal("sync command survived cleanup")
	}
	reply := manager.execute(request{ID: "new", Argv: []string{"/bin/true"}, TimeoutMS: 1000})
	if reply.Error == nil || reply.Error.Code != "busy" {
		t.Fatal(reply)
	}
	time.Sleep(500 * time.Millisecond)
	if _, err := os.Stat(leak); !os.IsNotExist(err) {
		t.Fatal("cleanup left descendant writing", err)
	}
}
