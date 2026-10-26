// Interactive sessions for OCI micro-VMs (`veynodevm exec -it`, `run -it`,
// `attach`, the web UI terminal); the server is in session_linux.go.
//
// The host connects over TCP to sessionPort: the guest address on Apple
// Virtualization NAT, a loopback hostfwd on QEMU. A connection starts with one
// JSON line (sessionRequest, carrying the container's random session token)
// and one JSON line reply; then both sides send frames of u8 type, u32
// big-endian length, payload.
//
//	host -> guest: 'i' stdin bytes, 'e' stdin EOF, 'r' resize (u16 rows, u16 cols)
//	guest -> host: 'o' stdout, 'x' stderr, 'q' exit status (decimal)
package main

import (
	"crypto/subtle"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"sync"
)

const sessionPort = 52022
const maxSessionFrame = 1 << 20

type sessionRequest struct {
	Token   string   `json:"token"`
	Op      string   `json:"op"`
	Argv    []string `json:"argv,omitempty"`
	TTY     bool     `json:"tty,omitempty"`
	Stdin   bool     `json:"stdin,omitempty"`
	Rows    uint16   `json:"rows,omitempty"`
	Cols    uint16   `json:"cols,omitempty"`
	Env     []string `json:"env,omitempty"`
	Workdir string   `json:"workdir,omitempty"`
	User    string   `json:"user,omitempty"`
}

// authorizeSession parses the request line and checks its token.
func authorizeSession(line []byte, token string) (sessionRequest, bool) {
	var req sessionRequest
	if json.Unmarshal(line, &req) != nil || len(token) < 32 {
		return req, false
	}
	return req, subtle.ConstantTimeCompare([]byte(req.Token), []byte(token)) == 1
}

type sessionConn struct {
	conn net.Conn
	mu   sync.Mutex
}

func (c *sessionConn) send(kind byte, payload []byte) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	header := [5]byte{kind}
	binary.BigEndian.PutUint32(header[1:], uint32(len(payload)))
	if _, err := c.conn.Write(header[:]); err != nil {
		return err
	}
	_, err := c.conn.Write(payload)
	return err
}

func readFrame(r io.Reader) (byte, []byte, error) {
	var header [5]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return 0, nil, err
	}
	size := binary.BigEndian.Uint32(header[1:])
	if size > maxSessionFrame {
		return 0, nil, errors.New("session frame too large")
	}
	payload := make([]byte, size)
	_, err := io.ReadFull(r, payload)
	return header[0], payload, err
}
