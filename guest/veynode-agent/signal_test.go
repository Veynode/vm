package main

import (
	"bufio"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestSignalCleanupWithActualServiceProcess(t *testing.T) {
	if os.Getenv("VEYNODE_SIGNAL_TEST_CHILD") == "1" {
		directory := os.Getenv("VEYNODE_SIGNAL_TEST_DIR")
		manager := newJobManager(time.Now)
		installSignalCleanup(manager)
		script := "(sleep 0.5; printf leaked >> \"$2\") & sleep 5 & printf ready > \"$1\"; wait"
		requestFor := func(name string) request {
			return request{ID: name, Method: "exec_start", Argv: []string{"/bin/sh", "-c", script, "test", filepath.Join(directory, name), filepath.Join(directory, "leak")}, TimeoutMS: 5000}
		}
		if reply := manager.start(requestFor("async-ready")); reply.Error != nil {
			panic(reply.Error)
		}
		go manager.execute(requestFor("sync-ready"))
		deadline := time.Now().Add(time.Second)
		for {
			_, a := os.Stat(filepath.Join(directory, "async-ready"))
			_, b := os.Stat(filepath.Join(directory, "sync-ready"))
			if a == nil && b == nil {
				break
			}
			if time.Now().After(deadline) {
				panic("children not ready")
			}
			time.Sleep(time.Millisecond)
		}
		fmt.Println("READY")
		select {}
	}
	directory := t.TempDir()
	child := exec.Command(os.Args[0], "-test.run=^TestSignalCleanupWithActualServiceProcess$")
	child.Env = append(os.Environ(), "VEYNODE_SIGNAL_TEST_CHILD=1", "VEYNODE_SIGNAL_TEST_DIR="+directory, "GORACE=atexit_sleep_ms=0")
	stdout, err := child.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	child.Stderr = os.Stderr
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = child.Process.Kill() }()
	ready := make(chan bool, 1)
	go func() {
		scanner := bufio.NewScanner(stdout)
		for scanner.Scan() {
			if scanner.Text() == "READY" {
				ready <- true
				return
			}
		}
		ready <- false
	}()
	select {
	case ok := <-ready:
		if !ok {
			t.Fatal("service child never ready")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("readiness timeout")
	}
	start := time.Now()
	if err := child.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- child.Wait() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal("SIGTERM cleanup failed", err)
		}
	case <-time.After(time.Second):
		t.Fatal("SIGTERM did not exit within 1 second")
	}
	if time.Since(start) >= time.Second {
		t.Fatal("cleanup too slow")
	}
	time.Sleep(600 * time.Millisecond)
	if _, err := os.Stat(filepath.Join(directory, "leak")); !os.IsNotExist(err) {
		t.Fatal("descendant survived service SIGTERM", err)
	}
}
