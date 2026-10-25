package main

import (
	"fmt"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

const executionPath = "/usr/sbin:/usr/bin:/sbin:/bin"

type executionIdentity struct {
	User                string   `json:"user"`
	UID                 uint32   `json:"uid"`
	GID                 uint32   `json:"gid"`
	Home                string   `json:"home"`
	CWD                 string   `json:"cwd"`
	Rootless            bool     `json:"rootless"`
	SupplementaryGroups []uint32 `json:"supplementary_groups"`
	Mode                string   `json:"mode"`
	// Chroot is the container root for OCI micro-VMs; commands run as the
	// container's root inside it, contained by the same cgroup boundary.
	Chroot     string `json:"chroot,omitempty"`
	env        []string
	credential *syscall.Credential
}

func currentExecutionIdentity() executionIdentity {
	home, _ := os.UserHomeDir()
	if home == "" {
		home = "/"
	}
	name := strconv.Itoa(os.Geteuid())
	if account, err := user.Current(); err == nil {
		name = account.Username
	}
	groups, _ := os.Getgroups()
	supplementary := make([]uint32, 0, len(groups))
	for _, group := range groups {
		supplementary = append(supplementary, uint32(group))
	}
	mode := "service-user"
	if os.Geteuid() == 0 {
		mode = "legacy-root"
	}
	return executionIdentity{User: name, UID: uint32(os.Geteuid()), GID: uint32(os.Getegid()), Home: home, CWD: "/", Rootless: os.Geteuid() != 0, SupplementaryGroups: supplementary, Mode: mode}
}
func identityForAccount(account *user.User, serviceUID int) (executionIdentity, error) {
	if serviceUID != 0 {
		return executionIdentity{}, fmt.Errorf("--exec-user requires root service privilege to set credentials and clear groups")
	}
	uid, err := strconv.ParseUint(account.Uid, 10, 32)
	if err != nil || uid == 0 {
		return executionIdentity{}, fmt.Errorf("--exec-user must resolve to a non-root UID")
	}
	gid, err := strconv.ParseUint(account.Gid, 10, 32)
	if err != nil || gid == 0 {
		return executionIdentity{}, fmt.Errorf("--exec-user must have a non-root primary GID")
	}
	if !filepath.IsAbs(account.HomeDir) {
		return executionIdentity{}, fmt.Errorf("execution account requires an absolute home directory")
	}
	stat, err := os.Stat(account.HomeDir)
	if err != nil || !stat.IsDir() {
		return executionIdentity{}, fmt.Errorf("execution account home directory is unavailable")
	}
	return executionIdentity{User: account.Username, UID: uint32(uid), GID: uint32(gid), Home: account.HomeDir, CWD: account.HomeDir, Rootless: true, SupplementaryGroups: []uint32{}, Mode: "fixed-user", credential: &syscall.Credential{Uid: uint32(uid), Gid: uint32(gid), Groups: []uint32{}}}, nil
}
func resolveExecutionIdentity(name string) (executionIdentity, error) {
	if name == "" {
		return currentExecutionIdentity(), nil
	}
	account, err := user.Lookup(name)
	if err != nil {
		return executionIdentity{}, fmt.Errorf("invalid --exec-user account: %w", err)
	}
	identity, err := identityForAccount(account, os.Geteuid())
	if err != nil {
		return executionIdentity{}, err
	}
	// Verify the configured identity can actually be applied before accepting RPCs.
	// No fallback to privileged service credentials is permitted.
	probe := exec.Command("/bin/true")
	identity.configure(probe)
	if err := probe.Run(); err != nil {
		return executionIdentity{}, fmt.Errorf("cannot apply --exec-user credentials or home directory: %w", err)
	}
	return identity, nil
}
func (identity executionIdentity) configure(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Credential: identity.credential, Chroot: identity.Chroot}
	cmd.Env = []string{"PATH=" + executionPath, "LANG=C.UTF-8", "HOME=" + identity.Home, "USER=" + identity.User, "LOGNAME=" + identity.User}
	if identity.env != nil {
		cmd.Env = append([]string(nil), identity.env...)
	}
	cmd.Dir = identity.CWD
}

// containerIdentity runs commands as root inside an OCI container root with
// the image environment, like `docker exec`.
func containerIdentity(root string, env []string, workdir string) executionIdentity {
	if workdir == "" {
		workdir = "/"
	}
	return executionIdentity{User: "root", UID: 0, GID: 0, Home: "/root", CWD: workdir, Rootless: false, SupplementaryGroups: []uint32{}, Mode: "container-root", Chroot: root, env: env}
}

func (identity executionIdentity) searchPath() string {
	for _, entry := range identity.env {
		if strings.HasPrefix(entry, "PATH=") {
			return strings.TrimPrefix(entry, "PATH=")
		}
	}
	return executionPath
}

// exec.Command normally resolves bare names using the service process PATH,
// before cmd.Env is applied. Resolve against the identity PATH (inside its
// chroot, if any) instead. Returned paths are as seen by the child.
func (identity executionIdentity) executable(name string) (string, error) {
	if strings.ContainsRune(name, '/') {
		return name, nil
	}
	for _, directory := range strings.Split(identity.searchPath(), ":") {
		if !filepath.IsAbs(directory) {
			continue
		}
		candidate := filepath.Join(directory, name)
		if info, err := os.Stat(filepath.Join(identity.Chroot, candidate)); err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("executable not found in PATH")
}

func commandExecutable(name string) (string, error) {
	return executionIdentity{}.executable(name)
}
