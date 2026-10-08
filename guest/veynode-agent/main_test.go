package main

import (
	"encoding/json"
	"io"
	"net"
	"strings"
	"testing"
	"time"
)

func TestDeniedAndUnknown(t *testing.T) {
	if handle(request{ID: "1", Method: "exec", Argv: []string{"echo"}, TimeoutMS: 1000}, false).Error.Code != "forbidden" {
		t.Fatal("exec enabled by default")
	}
	if handle(request{ID: "1", Method: "screenshot"}, true).Error.Code != "unsupported" {
		t.Fatal("unsupported hidden")
	}
}
func TestExecUnicodeAndNonzero(t *testing.T) {
	r := handle(request{ID: "1", Method: "exec", Argv: []string{"/bin/sh", "-c", "printf '你好'; printf error >&2; exit 7"}, TimeoutMS: 1000}, true)
	if r.Error != nil {
		t.Fatal(r.Error)
	}
	v := r.Result.(map[string]any)
	if v["stdout"] != "你好" || v["stderr"] != "error" || v["exit_code"] != 7 {
		t.Fatal(v)
	}
}
func TestTimeout(t *testing.T) {
	start := time.Now()
	r := handle(request{ID: "1", Method: "exec", Argv: []string{"/bin/sh", "-c", "sleep 10 & wait"}, TimeoutMS: 50}, true)
	if r.Error != nil || r.Result.(map[string]any)["timed_out"] != true || time.Since(start) > time.Second {
		t.Fatal(r)
	}
}
func TestOutputBound(t *testing.T) {
	r := handle(request{ID: "1", Method: "exec", Argv: []string{"/bin/sh", "-c", "yes x | head -c 100000"}, TimeoutMS: 1000}, true)
	if r.Error != nil {
		t.Fatal(r.Error)
	}
	v := r.Result.(map[string]any)
	if len(v["stdout"].(string)) != maxOutput || v["stdout_truncated"] != true {
		t.Fatal(v)
	}
}
func TestProtocolRejectsTrailingAndOversize(t *testing.T) {
	for _, msg := range []string{"{\"id\":\"1\",\"method\":\"info\"} {}\n", strings.Repeat("a", maxRequest+1) + "\n"} {
		a, b := net.Pipe()
		go serve(a, false)
		go func() { _, _ = io.WriteString(b, msg) }()
		var r response
		if err := json.NewDecoder(b).Decode(&r); err != nil {
			t.Fatal(err)
		}
		b.Close()
		if r.Error == nil {
			t.Fatal("accepted invalid frame")
		}
	}
}

func TestShutdownRequiresIndependentScope(t *testing.T) {
	r := handle(request{ID: "1", Method: "shutdown"}, true)
	if r.Error == nil || r.Error.Code != "forbidden" {
		t.Fatal("execute permission must not implicitly grant shutdown")
	}
}

func TestProtocolExactRequestLineBoundary(t *testing.T) {
	for _, length := range []int{maxRequest, maxRequest + 1} {
		value := request{ID: "boundary", Method: "info", Argv: []string{""}}
		empty, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		value.Argv[0] = strings.Repeat("x", length-len(empty)-1)
		encoded, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		encoded = append(encoded, '\n')
		if len(encoded) != length {
			t.Fatal("incorrect fixture length")
		}
		a, b := net.Pipe()
		go serve(a, false)
		go func() { _, _ = b.Write(encoded) }()
		var reply response
		if err := json.NewDecoder(b).Decode(&reply); err != nil {
			t.Fatal(err)
		}
		b.Close()
		if length == maxRequest && reply.Error != nil {
			t.Fatal("exact 64 KiB line rejected", reply.Error)
		}
		if length > maxRequest && (reply.Error == nil || reply.Error.Code != "invalid_request") {
			t.Fatal("oversize line accepted", reply)
		}
	}
}
