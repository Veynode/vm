package main

import (
	"errors"
	"os/exec"
	"syscall"
)

type jobBoundary interface {
	started()
	terminate() error
	close() error
}
type boundaryFactory func(*exec.Cmd) (jobBoundary, error)

type processGroupBoundary struct{ cmd *exec.Cmd }

func legacyBoundary(cmd *exec.Cmd) (jobBoundary, error) { return &processGroupBoundary{cmd}, nil }
func (*processGroupBoundary) started()                  {}
func (b *processGroupBoundary) terminate() error {
	err := syscall.Kill(-b.cmd.Process.Pid, syscall.SIGKILL)
	if errors.Is(err, syscall.ESRCH) {
		return nil
	}
	return err
}
func (*processGroupBoundary) close() error { return nil }
