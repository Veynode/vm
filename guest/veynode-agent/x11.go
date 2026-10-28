package main

// Minimal X11 core-protocol client for the desktop tools (desktop_x11.go):
// connection setup, atoms, properties, geometry, pointer, focus and EWMH
// client messages. Synchronous, little-endian, no extensions; enough to read
// and drive a window manager without any library in the guest image.

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"time"
)

const (
	x11InternAtom           = 16
	x11GetProperty          = 20
	x11GetGeometry          = 14
	x11SendEvent            = 25
	x11QueryPointer         = 38
	x11TranslateCoordinates = 40
	x11GetInputFocus        = 43
	x11ClientMessage        = 33
	x11AnyPropertyType      = 0
	// SubstructureNotify | SubstructureRedirect: how EWMH requests reach the WM.
	x11RootEventMask = 1<<19 | 1<<20
	x11MaxReply      = 1 << 20
)

type x11Conn struct {
	conn          net.Conn
	seq           uint16
	root          uint32
	width, height int
	atoms         map[string]uint32
	// Resource ids this client may allocate (a window of its own).
	idBase, idMask uint32
}

type x11Error struct{ code, request byte }

func (e *x11Error) Error() string {
	names := map[byte]string{3: "BadWindow", 5: "BadAtom", 8: "BadMatch", 9: "BadDrawable", 2: "BadValue"}
	name := names[e.code]
	if name == "" {
		name = fmt.Sprintf("error %d", e.code)
	}
	return fmt.Sprintf("X11 %s (request %d)", name, e.request)
}

func pad4(n int) int { return (n + 3) &^ 3 }

// x11Handshake runs connection setup on an open stream and returns the
// client for the first screen.
func x11Handshake(conn net.Conn, authName string, authData []byte) (*x11Conn, error) {
	head := make([]byte, 12, 12+pad4(len(authName))+pad4(len(authData)))
	head[0] = 'l'
	binary.LittleEndian.PutUint16(head[2:], 11)
	binary.LittleEndian.PutUint16(head[6:], uint16(len(authName)))
	binary.LittleEndian.PutUint16(head[8:], uint16(len(authData)))
	head = append(head, authName...)
	head = append(head, make([]byte, pad4(len(authName))-len(authName))...)
	head = append(head, authData...)
	head = append(head, make([]byte, pad4(len(authData))-len(authData))...)
	if _, err := conn.Write(head); err != nil {
		return nil, err
	}
	var prefix [8]byte
	if _, err := io.ReadFull(conn, prefix[:]); err != nil {
		return nil, err
	}
	extra := int(binary.LittleEndian.Uint16(prefix[6:])) * 4
	if extra > x11MaxReply {
		return nil, errors.New("X11 setup reply too large")
	}
	body := make([]byte, extra)
	if _, err := io.ReadFull(conn, body); err != nil {
		return nil, err
	}
	if prefix[0] != 1 {
		reason := body
		if n := int(prefix[1]); prefix[0] == 0 && n <= len(body) {
			reason = body[:n]
		}
		return nil, fmt.Errorf("X11 connection refused: %s", reason)
	}
	if len(body) < 32 {
		return nil, errors.New("short X11 setup reply")
	}
	vendor, formats := int(binary.LittleEndian.Uint16(body[16:])), int(body[21])
	screen := 32 + pad4(vendor) + 8*formats
	if body[20] == 0 || screen+24 > len(body) {
		return nil, errors.New("X11 server reports no screen")
	}
	return &x11Conn{conn: conn, root: binary.LittleEndian.Uint32(body[screen:]),
		width: int(binary.LittleEndian.Uint16(body[screen+20:])), height: int(binary.LittleEndian.Uint16(body[screen+22:])),
		atoms: map[string]uint32{}, idBase: binary.LittleEndian.Uint32(body[4:]), idMask: binary.LittleEndian.Uint32(body[8:])}, nil
}

func (x *x11Conn) close() { x.conn.Close() }

// send writes one request; data excludes the 4-byte header.
func (x *x11Conn) send(opcode, detail byte, data []byte) (uint16, error) {
	request := make([]byte, 4+pad4(len(data)))
	request[0], request[1] = opcode, detail
	binary.LittleEndian.PutUint16(request[2:], uint16(len(request)/4))
	copy(request[4:], data)
	_ = x.conn.SetDeadline(time.Now().Add(3 * time.Second))
	if _, err := x.conn.Write(request); err != nil {
		return 0, err
	}
	x.seq++
	return x.seq, nil
}

// reply reads until the reply or error for seq; events are skipped.
func (x *x11Conn) reply(seq uint16) ([]byte, error) {
	for {
		var head [32]byte
		if _, err := io.ReadFull(x.conn, head[:]); err != nil {
			return nil, err
		}
		got := binary.LittleEndian.Uint16(head[2:])
		switch head[0] {
		case 0:
			if got == seq {
				return nil, &x11Error{code: head[1], request: head[10]}
			}
		case 1:
			extra := int(binary.LittleEndian.Uint32(head[4:])) * 4
			if extra > x11MaxReply {
				return nil, errors.New("X11 reply too large")
			}
			out := make([]byte, 32+extra)
			copy(out, head[:])
			if _, err := io.ReadFull(x.conn, out[32:]); err != nil {
				return nil, err
			}
			if got == seq {
				return out, nil
			}
		}
	}
}

func (x *x11Conn) call(opcode, detail byte, data []byte) ([]byte, error) {
	seq, err := x.send(opcode, detail, data)
	if err != nil {
		return nil, err
	}
	return x.reply(seq)
}

func (x *x11Conn) atom(name string) (uint32, error) {
	if atom, ok := x.atoms[name]; ok {
		return atom, nil
	}
	data := make([]byte, 4, 4+len(name))
	binary.LittleEndian.PutUint16(data, uint16(len(name)))
	reply, err := x.call(x11InternAtom, 0, append(data, name...))
	if err != nil {
		return 0, err
	}
	atom := binary.LittleEndian.Uint32(reply[8:])
	x.atoms[name] = atom
	return atom, nil
}

type x11Property struct {
	format int
	kind   uint32
	data   []byte
}

func (x *x11Conn) property(window uint32, name string) (x11Property, error) {
	atom, err := x.atom(name)
	if err != nil {
		return x11Property{}, err
	}
	data := make([]byte, 20)
	binary.LittleEndian.PutUint32(data[0:], window)
	binary.LittleEndian.PutUint32(data[4:], atom)
	binary.LittleEndian.PutUint32(data[8:], x11AnyPropertyType)
	binary.LittleEndian.PutUint32(data[16:], x11MaxReply/4)
	reply, err := x.call(x11GetProperty, 0, data)
	if err != nil {
		return x11Property{}, err
	}
	format := int(reply[1])
	size := int(binary.LittleEndian.Uint32(reply[16:])) * max(format/8, 1)
	if 32+size > len(reply) {
		return x11Property{}, errors.New("truncated X11 property")
	}
	return x11Property{format: format, kind: binary.LittleEndian.Uint32(reply[8:]), data: reply[32 : 32+size]}, nil
}

func (p x11Property) cardinals() []uint32 {
	if p.format != 32 {
		return nil
	}
	out := make([]uint32, len(p.data)/4)
	for i := range out {
		out[i] = binary.LittleEndian.Uint32(p.data[i*4:])
	}
	return out
}

func (x *x11Conn) cardinals(window uint32, name string) []uint32 {
	p, err := x.property(window, name)
	if err != nil {
		return nil
	}
	return p.cardinals()
}

func (x *x11Conn) cardinal(window uint32, name string) (uint32, bool) {
	values := x.cardinals(window, name)
	if len(values) == 0 {
		return 0, false
	}
	return values[0], true
}

// strings splits a NUL-separated text property.
func (x *x11Conn) strings(window uint32, name string) []string {
	p, err := x.property(window, name)
	if err != nil || p.format != 8 || len(p.data) == 0 {
		return nil
	}
	data := p.data
	if data[len(data)-1] == 0 {
		data = data[:len(data)-1]
	}
	var out []string
	for _, part := range splitNUL(data) {
		out = append(out, latin1(part, p.kind, x))
	}
	return out
}

func splitNUL(data []byte) [][]byte {
	var out [][]byte
	start := 0
	for i, b := range data {
		if b == 0 {
			out = append(out, data[start:i])
			start = i + 1
		}
	}
	return append(out, data[start:])
}

// latin1 decodes STRING (ISO 8859-1) properties; UTF8_STRING is kept.
func latin1(data []byte, kind uint32, x *x11Conn) string {
	if utf8, err := x.atom("UTF8_STRING"); err == nil && kind == utf8 {
		return string(data)
	}
	runes := make([]rune, len(data))
	for i, b := range data {
		runes[i] = rune(b)
	}
	return string(runes)
}

// text reads a window title-like property, preferring the UTF-8 EWMH one.
func (x *x11Conn) text(window uint32, names ...string) string {
	for _, name := range names {
		if values := x.strings(window, name); len(values) > 0 && values[0] != "" {
			return values[0]
		}
	}
	return ""
}

type x11Rect struct{ X, Y, W, H int }

// geometry returns a window's area in root coordinates.
func (x *x11Conn) geometry(window uint32) (x11Rect, error) {
	data := make([]byte, 4)
	binary.LittleEndian.PutUint32(data, window)
	reply, err := x.call(x11GetGeometry, 0, data)
	if err != nil {
		return x11Rect{}, err
	}
	w, h := int(binary.LittleEndian.Uint16(reply[16:])), int(binary.LittleEndian.Uint16(reply[18:]))
	data = make([]byte, 12)
	binary.LittleEndian.PutUint32(data[0:], window)
	binary.LittleEndian.PutUint32(data[4:], x.root)
	reply, err = x.call(x11TranslateCoordinates, 0, data)
	if err != nil {
		return x11Rect{}, err
	}
	return x11Rect{int(int16(binary.LittleEndian.Uint16(reply[12:]))), int(int16(binary.LittleEndian.Uint16(reply[14:]))), w, h}, nil
}

func (x *x11Conn) pointer() (int, int, error) {
	data := make([]byte, 4)
	binary.LittleEndian.PutUint32(data, x.root)
	reply, err := x.call(x11QueryPointer, 0, data)
	if err != nil {
		return 0, 0, err
	}
	return int(int16(binary.LittleEndian.Uint16(reply[16:]))), int(int16(binary.LittleEndian.Uint16(reply[18:]))), nil
}

func (x *x11Conn) focus() (uint32, error) {
	reply, err := x.call(x11GetInputFocus, 0, nil)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint32(reply[8:]), nil
}

// message sends an EWMH client message to the root window and waits for the
// server to process it (a round trip reports a BadWindow or BadAtom).
func (x *x11Conn) message(window uint32, kind string, data ...uint32) error {
	atom, err := x.atom(kind)
	if err != nil {
		return err
	}
	request := make([]byte, 40)
	binary.LittleEndian.PutUint32(request[0:], x.root)
	binary.LittleEndian.PutUint32(request[4:], x11RootEventMask)
	event := request[8:]
	event[0], event[1] = x11ClientMessage, 32
	binary.LittleEndian.PutUint32(event[4:], window)
	binary.LittleEndian.PutUint32(event[8:], atom)
	for i, value := range data[:min(len(data), 5)] {
		binary.LittleEndian.PutUint32(event[12+4*i:], value)
	}
	seq, err := x.send(x11SendEvent, 0, request)
	if err != nil {
		return err
	}
	// The next reply orders after any error for the event request.
	next, err := x.send(x11GetInputFocus, 0, nil)
	if err != nil {
		return err
	}
	for {
		var head [32]byte
		if _, err := io.ReadFull(x.conn, head[:]); err != nil {
			return err
		}
		got := binary.LittleEndian.Uint16(head[2:])
		if head[0] == 0 && got == seq {
			return &x11Error{code: head[1], request: head[10]}
		}
		if head[0] == 1 {
			extra := int(binary.LittleEndian.Uint32(head[4:])) * 4
			if _, err := io.CopyN(io.Discard, x.conn, int64(extra)); err != nil {
				return err
			}
			if got == next {
				return nil
			}
		}
	}
}
