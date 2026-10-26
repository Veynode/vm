//go:build linux

package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseContainerConfig(t *testing.T) {
	raw := []byte(containerConfigMagic + `{"version":1,"id":"c1","argv":["sh","-c","echo hi"],"env":["A=B"],"transport":"vsock","volumes":[{"tag":"v0","target":"/data","fs":"virtiofs"}]}` + "\n\x00\x00")
	config, err := parseContainerConfig(raw)
	if err != nil || config.Argv[2] != "echo hi" || config.Volumes[0].Target != "/data" {
		t.Fatalf("parse: %v %+v", err, config)
	}
	for _, bad := range []string{
		`{"version":1,"argv":[],"transport":"vsock"}`,
		`{"version":2,"argv":["x"],"transport":"vsock"}`,
		`{"version":1,"argv":["x"],"transport":"tcp"}`,
		`{"version":1,"argv":["x"],"transport":"vsock","workdir":"relative"}`,
		`{"version":1,"argv":["x"],"transport":"vsock","unknown":true}`,
		`{"version":1,"argv":["x"],"transport":"vsock","volumes":[{"tag":"v","target":"/x","fs":"nfs"}]}`,
	} {
		if _, err := parseContainerConfig([]byte(containerConfigMagic + bad)); err == nil {
			t.Fatalf("accepted %s", bad)
		}
	}
	if _, err := parseContainerConfig([]byte(`{"version":1}`)); err == nil {
		t.Fatal("accepted config without magic")
	}
}

func TestInsideRoot(t *testing.T) {
	for path, want := range map[string]string{"/data": "/newroot/data", "../../etc": "/newroot/etc", "/": "/newroot"} {
		got, err := insideRoot("/newroot", path)
		if err != nil || got != want {
			t.Fatalf("%s: %s %v", path, got, err)
		}
	}
}

func TestLookupUser(t *testing.T) {
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "etc"), 0o755)
	os.WriteFile(filepath.Join(root, "etc/passwd"), []byte("root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000::/home/node:/bin/sh\n"), 0o644)
	os.WriteFile(filepath.Join(root, "etc/group"), []byte("root:x:0:\nstaff:x:50:\n"), 0o644)
	cases := []struct {
		spec     string
		uid, gid uint32
		home     string
	}{{"", 0, 0, "/root"}, {"node", 1000, 1000, "/home/node"}, {"1000", 1000, 1000, "/home/node"}, {"node:staff", 1000, 50, "/home/node"}, {"4242:7", 4242, 7, "/"}}
	for _, c := range cases {
		uid, gid, home, err := lookupUser(root, c.spec)
		if err != nil || uid != c.uid || gid != c.gid || home != c.home {
			t.Fatalf("%q: %d %d %s %v", c.spec, uid, gid, home, err)
		}
	}
	if _, _, _, err := lookupUser(root, "ghost"); err == nil {
		t.Fatal("unknown user accepted")
	}
}

func TestContainerIdentityResolvesInsideRoot(t *testing.T) {
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "opt/app/bin"), 0o755)
	os.WriteFile(filepath.Join(root, "opt/app/bin/tool"), []byte("#!/bin/sh\n"), 0o755)
	identity := containerIdentity(root, withDefaultEnv([]string{"PATH=/opt/app/bin:/bin"}, "/root"), "")
	path, err := identity.executable("tool")
	if err != nil || path != "/opt/app/bin/tool" {
		t.Fatalf("%s %v", path, err)
	}
	if _, err := identity.executable("missing"); err == nil {
		t.Fatal("missing executable resolved")
	}
	if identity.CWD != "/" || identity.Mode != "container-root" {
		t.Fatalf("%+v", identity)
	}
}

func TestContainerConfigDNS(t *testing.T) {
	base := `{"version":1,"id":"x","argv":["/bin/true"],"transport":"vsock","dns":%s}`
	if _, err := parseContainerConfig([]byte(containerConfigMagic + fmt.Sprintf(base, `["1.1.1.1","2606:4700:4700::1111"]`))); err != nil {
		t.Fatal(err)
	}
	if _, err := parseContainerConfig([]byte(containerConfigMagic + fmt.Sprintf(base, `["not-an-ip"]`))); err == nil {
		t.Fatal("invalid DNS server accepted")
	}
}

func TestContainerConfigInit(t *testing.T) {
	base := `{"version":1,"id":"x","argv":[%q],"transport":"vsock"%s}`
	if config, err := parseContainerConfig([]byte(containerConfigMagic + fmt.Sprintf(base, "/sbin/init", `,"init":"systemd"`))); err != nil || config.Init != "systemd" {
		t.Fatalf("%+v %v", config, err)
	}
	if config, err := parseContainerConfig([]byte(containerConfigMagic + fmt.Sprintf(base, "sh", ""))); err != nil || config.Init != "" {
		t.Fatalf("default init: %+v %v", config, err)
	}
	for _, bad := range []string{fmt.Sprintf(base, "/sbin/init", `,"init":"openrc"`), fmt.Sprintf(base, "init", `,"init":"systemd"`)} {
		if _, err := parseContainerConfig([]byte(containerConfigMagic + bad)); err == nil {
			t.Fatalf("accepted %s", bad)
		}
	}
}

func TestStatInRootFollowsLinksInsideRoot(t *testing.T) {
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "usr/lib/systemd"), 0o755)
	os.MkdirAll(filepath.Join(root, "usr/sbin"), 0o755)
	os.WriteFile(filepath.Join(root, "usr/lib/systemd/systemd"), []byte("#!/bin/sh\n"), 0o755)
	os.Symlink("usr/sbin", filepath.Join(root, "sbin"))
	os.Symlink("usr/lib", filepath.Join(root, "lib"))
	// Absolute links resolve inside the root, not on the host.
	os.Symlink("/lib/systemd/systemd", filepath.Join(root, "usr/sbin/init"))
	info, err := statInRoot(root, "/sbin/init")
	if err != nil || info.IsDir() || info.Mode()&0o111 == 0 {
		t.Fatalf("%v %v", info, err)
	}
	os.Symlink("../../../../../../../../bin/sh", filepath.Join(root, "usr/sbin/escape"))
	if _, err := statInRoot(root, "/sbin/escape"); err == nil {
		t.Fatal("followed a link out of the root")
	}
	os.Symlink("loop", filepath.Join(root, "loop"))
	if _, err := statInRoot(root, "/loop"); err == nil {
		t.Fatal("symlink loop resolved")
	}
}

func TestPrepareSystemdRuntime(t *testing.T) {
	root := t.TempDir()
	for _, dir := range []string{"run", "etc", "sbin"} {
		os.MkdirAll(filepath.Join(root, dir), 0o755)
	}
	agent := filepath.Join(t.TempDir(), "agent")
	os.WriteFile(agent, []byte("agent-binary"), 0o755)
	config := containerConfig{Argv: []string{"/sbin/init"}, Transport: "virtio-serial-exec", Workdir: "/srv"}
	if err := prepareSystemdRuntime(root, agent, config, []string{"PATH=/bin"}); err == nil {
		t.Fatal("accepted an image without init")
	}
	os.WriteFile(filepath.Join(root, "sbin/init"), []byte("#!/bin/sh\n"), 0o755)
	if err := prepareSystemdRuntime(root, agent, config, []string{"PATH=/bin"}); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(filepath.Join(root, "run/veynodevm/agent")); string(data) != "agent-binary" {
		t.Fatalf("agent copy: %q", data)
	}
	unit, _ := os.ReadFile(filepath.Join(root, "run/systemd/system/veynodevm-agent.service"))
	if !strings.Contains(string(unit), "ExecStart=/run/veynodevm/agent --virtio-serial-exec --allow-execute --container-root / --container-env /run/veynodevm/env.json\n") || strings.Contains(string(unit), "--allow-shutdown") {
		t.Fatalf("unit:\n%s", unit)
	}
	for _, link := range []string{"sysinit.target.wants/veynodevm-agent.service", "poweroff.target.wants/veynodevm-exit.service", "halt.target.wants/veynodevm-exit.service"} {
		if target, err := os.Readlink(filepath.Join(root, "run/systemd/system", link)); err != nil || !strings.HasPrefix(target, "../veynodevm-") {
			t.Fatalf("%s: %s %v", link, target, err)
		}
	}
	if blacklist, _ := os.ReadFile(filepath.Join(root, "run/modprobe.d/veynodevm.conf")); !strings.Contains(string(blacklist), "blacklist virtio_balloon\n") {
		t.Fatalf("modprobe.d: %q", blacklist)
	}
	if target, _ := os.Readlink(filepath.Join(root, "run/systemd/system/systemd-firstboot.service")); target != "/dev/null" {
		t.Fatalf("firstboot not masked: %q", target)
	}
	var env containerEnvFile
	raw, _ := os.ReadFile(filepath.Join(root, "run/veynodevm/env.json"))
	if json.Unmarshal(raw, &env) != nil || env.Workdir != "/srv" || env.Env[0] != "PATH=/bin" {
		t.Fatalf("env: %s", raw)
	}
	id, _ := os.ReadFile(filepath.Join(root, "etc/machine-id"))
	if len(strings.TrimSpace(string(id))) != 32 {
		t.Fatalf("machine-id %q", id)
	}
	// A second boot keeps the identity.
	if err := prepareSystemdRuntime(root, agent, config, nil); err != nil {
		t.Fatal(err)
	}
	if again, _ := os.ReadFile(filepath.Join(root, "etc/machine-id")); string(again) != string(id) {
		t.Fatal("machine-id changed")
	}
	if !strings.Contains(systemdAgentUnit("vsock"), "--allow-shutdown") {
		t.Fatal("vsock unit cannot shut down")
	}
}
