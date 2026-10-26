package main

import (
	"bytes"
	"crypto/rand"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"time"
)

const serialPort = "/dev/virtio-ports/org.veynode.agent.0"
const serialTimeout = 3 * time.Second

type serialEnvelope struct {
	wire       []byte
	Version    int       `json:"version"`
	Type       string    `json:"type"`
	GuestNonce string    `json:"guest_nonce"`
	HostNonce  string    `json:"host_nonce,omitempty"`
	Sequence   uint64    `json:"sequence,omitempty"`
	Request    *request  `json:"request,omitempty"`
	Response   *response `json:"response,omitempty"`
}

func serialNonce() (string, error) {
	var b [32]byte
	_, err := rand.Read(b[:])
	return hex.EncodeToString(b[:]), err
}
func validSerialNonce(s string) bool {
	if len(s) != 64 {
		return false
	}
	b, e := hex.DecodeString(s)
	return e == nil && hex.EncodeToString(b) == s
}

type deadlineStream interface {
	io.ReadWriteCloser
	SetReadDeadline(time.Time) error
	SetWriteDeadline(time.Time) error
}

func serialRead(s deadlineStream, idle bool) (serialEnvelope, error) {
	var e serialEnvelope
	var h [4]byte
	deadline := time.Now().Add(serialTimeout)
	if idle {
		deadline = time.Time{}
	}
	if err := s.SetReadDeadline(deadline); err != nil {
		return e, err
	}
	if _, err := io.ReadFull(s, h[:1]); err != nil {
		return e, err
	}
	if idle {
		deadline = time.Now().Add(serialTimeout)
	}
	if err := s.SetReadDeadline(deadline); err != nil {
		return e, err
	}
	if _, err := io.ReadFull(s, h[1:]); err != nil {
		return e, err
	}
	n := binary.BigEndian.Uint32(h[:])
	if n == 0 || n > 65536 {
		return e, fmt.Errorf("serial request size invalid")
	}
	b := make([]byte, n)
	if _, err := io.ReadFull(s, b); err != nil {
		return e, err
	}
	d := json.NewDecoder(bytes.NewReader(b))
	d.DisallowUnknownFields()
	if err := d.Decode(&e); err != nil {
		return e, fmt.Errorf("invalid serial envelope")
	}
	if d.Decode(new(any)) != io.EOF {
		return e, fmt.Errorf("trailing serial JSON")
	}
	e.wire = b
	return e, nil
}
func serialWrite(s deadlineStream, e serialEnvelope) error {
	b, err := json.Marshal(e)
	if err != nil {
		return err
	}
	if len(b) > 1048576 {
		return fmt.Errorf("serial response size exceeded")
	}
	var h [4]byte
	binary.BigEndian.PutUint32(h[:], uint32(len(b)))
	if err = s.SetWriteDeadline(time.Now().Add(serialTimeout)); err != nil {
		return err
	}
	for _, part := range [][]byte{h[:], b} {
		for len(part) > 0 {
			n, e := s.Write(part)
			if e != nil {
				return e
			}
			if n == 0 {
				return io.ErrShortWrite
			}
			part = part[n:]
		}
	}
	return nil
}

// A service incarnation accepts one host connection only. Any framing/EOF/timeout
// error ends the incarnation. It never invokes a mutating handler or resynchronizes.
func serveSerial(s deadlineStream) error {
	defer s.Close()
	nonce, err := serialNonce()
	if err != nil {
		return err
	}
	if err = serialWrite(s, serialEnvelope{Version: 1, Type: "challenge", GuestNonce: nonce}); err != nil {
		return err
	}
	hello, err := serialRead(s, false)
	if err != nil {
		return err
	}
	if hello.Version != 1 || hello.Type != "hello" || hello.GuestNonce != nonce || !validSerialNonce(hello.HostNonce) || hello.Sequence != 0 || hello.Request != nil || hello.Response != nil {
		return fmt.Errorf("serial handshake rejected")
	}
	ready := serialEnvelope{Version: 1, Type: "ready", GuestNonce: nonce, HostNonce: hello.HostNonce}
	if err = serialWrite(s, ready); err != nil {
		return err
	}
	for seq := uint64(1); seq > 0; seq++ {
		e, err := serialRead(s, true)
		if err != nil {
			return err
		}
		if e.Version != 1 || e.Type != "request" || e.GuestNonce != nonce || e.HostNonce != hello.HostNonce || e.Sequence != seq || e.Request == nil || e.Response != nil {
			return fmt.Errorf("serial request binding rejected")
		}
		r := e.Request
		// Exact observer request; never route other methods to handle().
		if r.Method != "info" || r.ID == "" || len(r.ID) > 128 || len(r.Argv) != 0 || r.TimeoutMS != 0 || r.JobID != "" || r.Owner != "" || r.Path != "" || r.Token != "" || r.Data != "" || r.Size != 0 || r.Offset != 0 || r.SHA256 != "" {
			return fmt.Errorf("serial observer request rejected")
		}
		reply := handle(*r, false, false)
		result, ok := reply.Result.(map[string]any)
		if !ok {
			return fmt.Errorf("serial info unavailable")
		}
		result["transport"] = "virtio-serial-private-observer-v1"
		result["transport_reconnect"] = false
		if err = serialWrite(s, serialEnvelope{Version: 1, Type: "response", GuestNonce: nonce, HostNonce: hello.HostNonce, Sequence: seq, Response: &reply}); err != nil {
			return err
		}
	}
	return fmt.Errorf("serial sequence exhausted")
}
