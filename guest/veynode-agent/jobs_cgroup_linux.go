//go:build linux

package main

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

const commandCgroupRoot = "/sys/fs/cgroup/veynode-exec"
const cgroup2Magic = 0x63677270

// Held for the service lifetime: a second service must not reap the first one's jobs.
var commandCgroupLock *os.File

type cgroupBoundary struct {
	path string
	fd   *os.File
}

func (b *cgroupBoundary) started() {
	if b.fd != nil {
		_ = b.fd.Close()
		b.fd = nil
	}
}
func (b *cgroupBoundary) terminate() error {
	if err := os.WriteFile(filepath.Join(b.path, "cgroup.kill"), []byte("1"), 0600); err != nil {
		return fmt.Errorf("cgroup kill failed: %w", err)
	}
	deadline := time.Now().Add(time.Second)
	for {
		data, err := os.ReadFile(filepath.Join(b.path, "cgroup.events"))
		if err != nil {
			return err
		}
		if strings.Contains(string(data), "populated 0\n") {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("cgroup remained populated after bounded cancellation")
		}
		time.Sleep(time.Millisecond)
	}
}
func (b *cgroupBoundary) close() error { b.started(); return os.Remove(b.path) }
func secureRootDirectory(path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok || !info.IsDir() || info.Mode().Perm() != 0700 || stat.Uid != 0 {
		return fmt.Errorf("cgroup execution subtree must be root-owned directory mode0700")
	}
	return nil
}
func configureCommandContainment(identity executionIdentity, legacy bool) (boundaryFactory, string, error) {
	if legacy {
		return legacyBoundary, "legacy-process-group-not-contained", nil
	}
	if os.Geteuid() != 0 || identity.credential == nil || identity.UID == 0 || identity.GID == 0 {
		return nil, "unavailable", fmt.Errorf("cgroup containment requires root service and fixed non-root --exec-user")
	}
	var fs syscall.Statfs_t
	if err := syscall.Statfs("/sys/fs/cgroup", &fs); err != nil {
		return nil, "unavailable", err
	}
	if fs.Type != cgroup2Magic {
		if fs.Type != 0x62656572 {
			return nil, "unavailable", fmt.Errorf("existing cgroup mount is not unified cgroup v2")
		}
		if err := syscall.Mount("cgroup2", "/sys/fs/cgroup", "cgroup2", syscall.MS_NOSUID|syscall.MS_NODEV|syscall.MS_NOEXEC, ""); err != nil {
			return nil, "unavailable", fmt.Errorf("mount cgroup v2: %w", err)
		}
	}
	if err := syscall.Statfs("/sys/fs/cgroup", &fs); err != nil || fs.Type != cgroup2Magic {
		return nil, "unavailable", fmt.Errorf("cgroup v2 filesystem verification failed")
	}
	lock, err := os.OpenFile("/run/veynode-exec.lock", os.O_CREATE|os.O_RDWR|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return nil, "unavailable", err
	}
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		lock.Close()
		return nil, "unavailable", fmt.Errorf("another execution service owns cgroup subtree: %w", err)
	}
	success := false
	defer func() {
		if !success {
			lock.Close()
		}
	}()
	if err = os.Mkdir(commandCgroupRoot, 0700); err != nil && !errors.Is(err, os.ErrExist) {
		return nil, "unavailable", err
	}
	if err = secureRootDirectory(commandCgroupRoot); err != nil {
		return nil, "unavailable", err
	}
	// After a service crash, reap only our own root-protected prior job subtrees.
	entries, err := os.ReadDir(commandCgroupRoot)
	if err != nil {
		return nil, "unavailable", err
	}
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		if !strings.HasPrefix(entry.Name(), "job-") {
			return nil, "unavailable", fmt.Errorf("unexpected directory in execution subtree")
		}
		stale := &cgroupBoundary{path: filepath.Join(commandCgroupRoot, entry.Name())}
		if err = secureRootDirectory(stale.path); err != nil {
			return nil, "unavailable", err
		}
		if err = stale.terminate(); err != nil {
			return nil, "unavailable", err
		}
		if err = stale.close(); err != nil {
			return nil, "unavailable", err
		}
	}
	factory := func(cmd *exec.Cmd) (jobBoundary, error) {
		path, err := os.MkdirTemp(commandCgroupRoot, "job-")
		if err != nil {
			return nil, err
		}
		dir, err := os.Open(path)
		if err != nil {
			os.Remove(path)
			return nil, err
		}
		b := &cgroupBoundary{path: path, fd: dir}
		if _, err = os.Stat(filepath.Join(path, "cgroup.kill")); err != nil {
			b.close()
			return nil, fmt.Errorf("kernel lacks cgroup.kill: %w", err)
		}
		cmd.SysProcAttr.UseCgroupFD = true
		cmd.SysProcAttr.CgroupFD = int(dir.Fd())
		return b, nil
	}
	// Real clone3/credential/cgroup.kill probe; no fallback after unsupported kernels.
	probe := exec.Command("/bin/true")
	identity.configure(probe)
	boundary, err := factory(probe)
	if err != nil {
		return nil, "unavailable", err
	}
	err = probe.Start()
	boundary.started()
	if err == nil {
		err = probe.Wait()
	}
	killErr := boundary.terminate()
	closeErr := boundary.close()
	if err != nil || killErr != nil || closeErr != nil {
		return nil, "unavailable", fmt.Errorf("atomic cgroup execution probe failed: %v / %v / %v", err, killErr, closeErr)
	}
	commandCgroupLock = lock
	success = true
	return factory, "cgroup-v2-clone3-kill", nil
}
