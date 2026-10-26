package main

import (
	"bytes"
	"io"
	"net"
	"strings"
	"testing"
	"time"
)

// writerConn is a net.Conn whose writes go to a buffer.
type writerConn struct{ *bytes.Buffer }

func (writerConn) Read([]byte) (int, error)         { return 0, io.EOF }
func (writerConn) Close() error                     { return nil }
func (writerConn) LocalAddr() net.Addr              { return nil }
func (writerConn) RemoteAddr() net.Addr             { return nil }
func (writerConn) SetDeadline(time.Time) error      { return nil }
func (writerConn) SetReadDeadline(time.Time) error  { return nil }
func (writerConn) SetWriteDeadline(time.Time) error { return nil }

func TestSessionRequiresToken(t *testing.T) {
	token := strings.Repeat("a", 64)
	if _, ok := authorizeSession([]byte(`{"token":"`+token+`","op":"exec"}`), token); !ok {
		t.Fatal("matching token refused")
	}
	for _, line := range []string{`{"token":"wrong","op":"exec"}`, `{"op":"exec"}`, `not json`} {
		if _, ok := authorizeSession([]byte(line), token); ok {
			t.Fatalf("accepted %s", line)
		}
	}
	// A missing server token never matches an empty request token.
	if _, ok := authorizeSession([]byte(`{"token":""}`), ""); ok {
		t.Fatal("empty token accepted")
	}
}

func TestSessionFrames(t *testing.T) {
	var buffer bytes.Buffer
	client := &sessionConn{conn: writerConn{&buffer}}
	if err := client.send('o', []byte("hello")); err != nil {
		t.Fatal(err)
	}
	kind, payload, err := readFrame(&buffer)
	if err != nil || kind != 'o' || string(payload) != "hello" {
		t.Fatalf("got %q %q %v", kind, payload, err)
	}
	if _, _, err = readFrame(bytes.NewReader([]byte{'i', 0xff, 0xff, 0xff, 0xff})); err == nil {
		t.Fatal("oversized frame accepted")
	}
}
