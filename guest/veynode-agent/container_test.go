//go:build linux

package main

import (
	"fmt"
	"os"
	"path/filepath"
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
