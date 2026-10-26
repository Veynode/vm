//go:build linux

package main

// "Full system" mode (--init systemd): container-init prepares the root as
// usual and then makes the image's init PID 1. Before handing over it writes
// runtime-only configuration under the new root's /run (a tmpfs, so nothing
// leaks into the writable layer) that keeps the micro-VM integration working:
// the guest service as a systemd unit, an exit marker on shutdown, and DNS
// managed by VeynodeVM rather than the image's network stack.

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

const systemdAgentPath = "/run/veynodevm/agent"
const systemdEnvPath = "/run/veynodevm/env.json"

// systemdAgentArgs are the guest service flags for the transport; commands run
// as root in "/" because the service itself lives inside the container root.
func systemdAgentArgs(transport string) []string {
	args := []string{"--allow-execute", "--container-root", "/", "--container-env", systemdEnvPath}
	if transport == "virtio-serial-exec" {
		return append([]string{"--virtio-serial-exec"}, args...)
	}
	// On Apple Virtualization `veynodevm stop` asks the service to shut down;
	// /sbin/poweroff in a systemd image is `systemctl poweroff`.
	return append(args, "--allow-shutdown")
}

func systemdAgentUnit(transport string) string {
	return `[Unit]
Description=VeynodeVM guest service (exec, stop)
DefaultDependencies=no
Conflicts=shutdown.target
Before=shutdown.target

[Service]
ExecStart=` + systemdAgentPath + " " + strings.Join(systemdAgentArgs(transport), " ") + `
Restart=always
RestartSec=1
`
}

// The exit marker lets `veynodevm ps` report "Exited (0)" after a clean
// poweroff, like a container whose entrypoint returned. A reboot restarts the
// micro-VM in place and reports nothing.
const systemdExitUnit = `[Unit]
Description=Report VeynodeVM container exit
DefaultDependencies=no
Before=systemd-poweroff.service systemd-halt.service

[Service]
Type=oneshot
ExecStart=` + systemdAgentPath + ` marker exit 0
StandardOutput=file:/dev/console
`

// DNS comes from --dns/runtime.json and is bind-mounted on /etc/resolv.conf;
// NetworkManager may still configure eth0 (Apple NAT and QEMU user networking
// hand out the same lease again) but must not rewrite the resolver.
const networkManagerConf = `[main]
dns=none
rc-manager=unmanaged
`

// The image's udev would autoload drivers that container-init never loads.
// With both of these loaded, GNOME guests on Apple Virtualization hit memory
// corruption (kernel oopses, squashfs and ext4 errors) within 25-100 s of a
// first boot in 3 of 3 runs; with them blacklisted the guests stayed clean
// (docs/known-issues.md). Neither device is needed by a micro-VM: the kernel
// seeds its RNG without virtio-rng, and VeynodeVM never inflates the balloon.
const modprobeConf = `blacklist virtio_balloon
blacklist virtio_rng
`

// Units that block or prompt on a first boot without a console user, and
// login prompts on the console, which is an output-only log here.
var systemdMasks = []string{"systemd-firstboot.service", "systemd-networkd-wait-online.service", "serial-getty@hvc0.service", "serial-getty@ttyAMA0.service"}

func writeFileUnder(root, path string, data []byte, mode os.FileMode) error {
	target, err := insideRoot(root, path)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return err
	}
	return os.WriteFile(target, data, mode)
}

func symlinkUnder(root, path, link string) error {
	target, err := insideRoot(root, path)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return err
	}
	_ = os.Remove(target)
	return os.Symlink(link, target)
}

func copyExecutable(source, target string) error {
	in, err := os.Open(source)
	if err != nil {
		return err
	}
	defer in.Close()
	if err = os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return err
	}
	out, err := os.OpenFile(target, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o755)
	if err != nil {
		return err
	}
	if _, err = io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

// ensureMachineID gives an image without /etc/machine-id (Docker images ship
// none or an empty one) a persistent identity, so systemd does not treat
// every boot as a first boot.
func ensureMachineID(root string) error {
	path := filepath.Join(root, "etc/machine-id")
	if info, err := os.Lstat(path); err == nil {
		if info.Mode()&os.ModeSymlink != 0 {
			return nil
		}
		if existing, err := os.ReadFile(path); err == nil && len(strings.TrimSpace(string(existing))) == 32 {
			return nil
		}
	}
	id := make([]byte, 16)
	if _, err := rand.Read(id); err != nil {
		return err
	}
	return writeFileUnder(root, "/etc/machine-id", []byte(hex.EncodeToString(id)+"\n"), 0o444)
}

// statInRoot stats path as seen from inside root, following symlinks
// (absolute ones relative to root), e.g. /sbin/init -> /lib/systemd/systemd.
func statInRoot(root, path string) (os.FileInfo, error) {
	pending := strings.Split(filepath.Clean("/"+path), "/")
	resolved := "/"
	hops := 0
	for len(pending) > 0 {
		part := pending[0]
		pending = pending[1:]
		switch part {
		case "", ".":
			continue
		case "..":
			resolved = filepath.Dir(resolved)
			continue
		}
		next := filepath.Join(resolved, part)
		info, err := os.Lstat(filepath.Join(root, next))
		if err != nil {
			return nil, err
		}
		if info.Mode()&os.ModeSymlink == 0 {
			resolved = next
			continue
		}
		if hops++; hops > 40 {
			return nil, errors.New("too many symlinks")
		}
		link, err := os.Readlink(filepath.Join(root, next))
		if err != nil {
			return nil, err
		}
		if filepath.IsAbs(link) {
			resolved = "/"
		}
		pending = append(strings.Split(link, "/"), pending...)
	}
	return os.Lstat(filepath.Join(root, resolved))
}

// prepareSystemdRuntime writes the integration files into the new root.
func prepareSystemdRuntime(root, agentSource string, config containerConfig, env []string) error {
	if info, err := statInRoot(root, config.Argv[0]); err != nil || info.IsDir() || info.Mode()&0o111 == 0 {
		return fmt.Errorf("init %s not found in the image (install systemd, or pass the init path as COMMAND)", config.Argv[0])
	}
	if _, err := os.Stat(filepath.Join(root, "run")); err != nil {
		return errors.New("container root has no /run")
	}
	if err := copyExecutable(agentSource, filepath.Join(root, systemdAgentPath)); err != nil {
		return fmt.Errorf("install guest service: %w", err)
	}
	envJSON, err := json.Marshal(containerEnvFile{Env: env, Workdir: config.Workdir})
	if err != nil {
		return err
	}
	if err = writeFileUnder(root, systemdEnvPath, envJSON, 0o600); err != nil {
		return err
	}
	units := "/run/systemd/system/"
	for _, unit := range []struct{ name, body, wantedBy string }{
		{"veynodevm-agent.service", systemdAgentUnit(config.Transport), "sysinit.target"},
		{"veynodevm-exit.service", systemdExitUnit, "poweroff.target"},
		{"veynodevm-exit.service", "", "halt.target"},
	} {
		if unit.body != "" {
			if err = writeFileUnder(root, units+unit.name, []byte(unit.body), 0o644); err != nil {
				return err
			}
		}
		if err = symlinkUnder(root, units+unit.wantedBy+".wants/"+unit.name, "../"+unit.name); err != nil {
			return err
		}
	}
	for _, name := range systemdMasks {
		if err = symlinkUnder(root, units+name, "/dev/null"); err != nil {
			return err
		}
	}
	if err = writeFileUnder(root, "/run/modprobe.d/veynodevm.conf", []byte(modprobeConf), 0o644); err != nil {
		return err
	}
	if err = writeFileUnder(root, "/run/NetworkManager/conf.d/00-veynodevm.conf", []byte(networkManagerConf), 0o644); err != nil {
		return err
	}
	return ensureMachineID(root)
}
