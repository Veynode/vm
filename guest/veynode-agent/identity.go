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
	credential          *syscall.Credential
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
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Credential: identity.credential}
	cmd.Env = []string{"PATH=" + executionPath, "LANG=C.UTF-8", "HOME=" + identity.Home, "USER=" + identity.User, "LOGNAME=" + identity.User}
	cmd.Dir = identity.CWD
}

// exec.Command normally resolves bare names using the service process PATH,
// before cmd.Env is applied. Resolve against our fixed command PATH instead.
func commandExecutable(name string) (string, error) {
	if strings.ContainsRune(name, '/') {
		return name, nil
	}
	for _, directory := range strings.Split(executionPath, ":") {
		candidate := filepath.Join(directory, name)
		if info, err := os.Stat(candidate); err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("executable not found in fixed PATH")
}
