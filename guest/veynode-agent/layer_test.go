package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestAdoptWritableLayer(t *testing.T) {
	disk := t.TempDir()
	upper := filepath.Join(disk, "upper")
	write := func(name, body string) {
		path := filepath.Join(upper, name)
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(body), 0o444); err != nil {
			t.Fatal(err)
		}
	}
	exists := func(name string) bool {
		_, err := os.Lstat(filepath.Join(upper, name))
		return err == nil
	}
	write("etc/machine-id", "0123456789abcdef0123456789abcdef\n")
	write("var/lib/dbus/machine-id", "0123456789abcdef0123456789abcdef\n")
	write("etc/hostname", "keep\n")

	// A layer without an owner (first boot, or written by an older agent) is claimed as is.
	if forked, err := adoptWritableLayer(disk, upper, "aaaaaa"); err != nil || forked {
		t.Fatalf("first boot: forked=%v err=%v", forked, err)
	}
	if !exists("etc/machine-id") {
		t.Fatal("first boot removed machine-id")
	}
	// The same container booting again keeps its identity.
	if forked, err := adoptWritableLayer(disk, upper, "aaaaaa"); err != nil || forked {
		t.Fatalf("reboot: forked=%v err=%v", forked, err)
	}
	if !exists("etc/machine-id") {
		t.Fatal("reboot removed machine-id")
	}
	// A clone booted by another container drops the machine identity only.
	if forked, err := adoptWritableLayer(disk, upper, "bbbbbb"); err != nil || !forked {
		t.Fatalf("fork: forked=%v err=%v", forked, err)
	}
	if exists("etc/machine-id") || exists("var/lib/dbus/machine-id") || !exists("etc/hostname") {
		t.Fatal("fork did not reset exactly the identity files")
	}
	owner, _ := os.ReadFile(filepath.Join(disk, layerOwnerFile))
	if string(owner) != "bbbbbb\n" {
		t.Fatalf("owner %q", owner)
	}
	// No id (older host) changes nothing.
	if forked, err := adoptWritableLayer(disk, upper, ""); err != nil || forked {
		t.Fatalf("empty id: forked=%v err=%v", forked, err)
	}
}
