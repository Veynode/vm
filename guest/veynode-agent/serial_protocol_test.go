package main

import (
	"encoding/binary"
	"encoding/json"
	"flag"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func serialPair(t *testing.T) (net.Conn, <-chan error) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "vn-serial-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	path := filepath.Join(dir, "agent.sock")
	l, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		defer l.Close()
		c, e := l.Accept()
		if e != nil {
			done <- e
			return
		}
		done <- serveSerial(c.(deadlineStream))
	}()
	c, e := net.Dial("unix", path)
	if e != nil {
		t.Fatal(e)
	}
	t.Cleanup(func() { c.Close() })
	return c, done
}
func serialHost(t *testing.T, c net.Conn) serialEnvelope {
	t.Helper()
	challenge, e := serialRead(c, false)
	if e != nil {
		t.Fatal(e)
	}
	if challenge.Type != "challenge" || !validSerialNonce(challenge.GuestNonce) {
		t.Fatal(challenge)
	}
	challenge.Type = "hello"
	challenge.HostNonce = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	if e = serialWrite(c, challenge); e != nil {
		t.Fatal(e)
	}
	ready, e := serialRead(c, false)
	if e != nil || ready.Type != "ready" {
		t.Fatalf("handshake %v %v", ready, e)
	}
	return ready
}
func serialEnded(t *testing.T, done <-chan error) {
	t.Helper()
	select {
	case e := <-done:
		if e == nil {
			t.Fatal("fence must fail")
		}
	case <-time.After(4 * time.Second):
		t.Fatal("unbounded serial shutdown")
	}
}
func TestSerialObserverUnixAndEOF(t *testing.T) {
	c, done := serialPair(t)
	r := serialHost(t, c)
	r.Type = "request"
	r.Sequence = 1
	r.Request = &request{ID: "one", Method: "info"}
	if e := serialWrite(c, r); e != nil {
		t.Fatal(e)
	}
	reply, e := serialRead(c, false)
	if e != nil {
		t.Fatal(e)
	}
	b, _ := json.Marshal(reply.Response.Result)
	var v map[string]any
	json.Unmarshal(b, &v)
	if v["transport"] != "virtio-serial-private-observer-v1" {
		t.Fatal(v)
	}
	caps := v["capabilities"].(map[string]any)
	for _, k := range []string{"exec", "async_exec", "shutdown", "files", "desktop", "descendant_containment"} {
		if caps[k] != false {
			t.Fatal(k)
		}
	}
	c.Close()
	serialEnded(t, done)
}
func TestSerialRejectsReplayMutationAndForeignBinding(t *testing.T) {
	for _, kind := range []string{"sequence", "nonce", "exec", "files", "shutdown"} {
		t.Run(kind, func(t *testing.T) {
			c, done := serialPair(t)
			r := serialHost(t, c)
			r.Type = "request"
			r.Sequence = 1
			r.Request = &request{ID: "one", Method: "info"}
			switch kind {
			case "sequence":
				r.Sequence = 2
			case "nonce":
				r.HostNonce = "b" + r.HostNonce[1:]
			case "exec":
				r.Request.Method = "exec_start"
				r.Request.Argv = []string{"/bin/sh", "-c", "exit 7"}
			case "files":
				r.Request.Method = "file_upload_begin"
			case "shutdown":
				r.Request.Method = "shutdown"
			}
			if e := serialWrite(c, r); e != nil {
				t.Fatal(e)
			}
			serialEnded(t, done)
		})
	}
}
func TestSerialRejectsPartialAndOversizedFrames(t *testing.T) {
	for _, kind := range []string{"partial", "oversize", "unknown"} {
		t.Run(kind, func(t *testing.T) {
			c, done := serialPair(t)
			serialHost(t, c)
			if kind == "partial" {
				c.Write([]byte{0, 0})
				c.Close()
			} else if kind == "oversize" {
				var h [4]byte
				binary.BigEndian.PutUint32(h[:], 65537)
				c.Write(h[:])
			} else {
				b := []byte(`{"version":1,"type":"request","unknown":true}`)
				var h [4]byte
				binary.BigEndian.PutUint32(h[:], uint32(len(b)))
				c.Write(h[:])
				c.Write(b)
			}
			serialEnded(t, done)
		})
	}
}
func TestSerialPartialFrameDeadline(t *testing.T) {
	if testing.Short() {
		t.Skip("3-second protocol deadline")
	}
	c, done := serialPair(t)
	serialHost(t, c)
	c.Write([]byte{0})
	start := time.Now()
	serialEnded(t, done)
	if time.Since(start) < 2*time.Second {
		t.Fatal("fixture did not exercise deadline")
	}
}
func TestSerialDifferentServiceChallenge(t *testing.T) {
	a, da := serialPair(t)
	old, e := serialRead(a, false)
	if e != nil {
		t.Fatal(e)
	}
	a.Close()
	serialEnded(t, da)
	b, db := serialPair(t)
	fresh, e := serialRead(b, false)
	if e != nil {
		t.Fatal(e)
	}
	if old.GuestNonce == fresh.GuestNonce {
		t.Fatal("nonce reused")
	}
	old.Type = "hello"
	old.HostNonce = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	serialWrite(b, old)
	serialEnded(t, db)
}

func TestSerialCLIRejectsEveryPrivilegeBeforeDeviceOpen(t *testing.T) {
	if value := os.Getenv("VEYNODE_SERIAL_CLI_CHILD"); value != "" {
		flag.CommandLine = flag.NewFlagSet("serial-cli", flag.ExitOnError)
		os.Args = []string{"serial-cli", "--virtio-serial", value}
		main()
		return
	}
	for _, arg := range []string{"--allow-execute", "--allow-files", "--allow-shutdown", "--exec-user=nobody", "--files-user=nobody", "--files-root=/tmp", "--legacy-exec-process-groups", "--file-worker"} {
		c := exec.Command(os.Args[0], "-test.run=^TestSerialCLIRejectsEveryPrivilegeBeforeDeviceOpen$")
		c.Env = append(os.Environ(), "VEYNODE_SERIAL_CLI_CHILD="+arg, "GORACE=atexit_sleep_ms=0")
		out, err := c.CombinedOutput()
		if err == nil || !strings.Contains(string(out), "observer-only; privilege options rejected") {
			t.Fatalf("%s: error=%v output=%s", arg, err, out)
		}
	}
}

// Test-only interoperability endpoint; never part of the shipped service CLI.
func TestSerialInteropService(t *testing.T) {
	path := os.Getenv("VEYNODE_SERIAL_INTEROP_SOCKET")
	if path == "" {
		return
	}
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	conn, err := listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	if err = serveSerial(conn.(deadlineStream)); err == nil {
		t.Fatal("observer incarnation must end on EOF")
	}
}
