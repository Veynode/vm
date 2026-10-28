package main

import (
	"bytes"
	"encoding/binary"
	"io"
	"net"
	"testing"
	"time"
)

// fakeClipboardX is the server end of a clipboard connection: it numbers requests
// like an X server and answers the setup the client needs.
type fakeClipboardX struct {
	t     *testing.T
	conn  net.Conn
	seq   uint16
	atoms map[string]uint32
}

type fakeRequest struct {
	opcode, detail byte
	seq            uint16
	data           []byte
}

func (f *fakeClipboardX) next() fakeRequest {
	f.t.Helper()
	_ = f.conn.SetReadDeadline(time.Now().Add(3 * time.Second))
	var head [4]byte
	if _, err := io.ReadFull(f.conn, head[:]); err != nil {
		f.t.Fatalf("read request: %v", err)
	}
	data := make([]byte, int(binary.LittleEndian.Uint16(head[2:]))*4-4)
	if _, err := io.ReadFull(f.conn, data); err != nil {
		f.t.Fatalf("read request body: %v", err)
	}
	f.seq++
	return fakeRequest{head[0], head[1], f.seq, data}
}

func (f *fakeClipboardX) expect(opcode byte) fakeRequest {
	f.t.Helper()
	r := f.next()
	if r.opcode != opcode {
		f.t.Fatalf("request %d, want %d", r.opcode, opcode)
	}
	return r
}

func (f *fakeClipboardX) reply(seq uint16, head []byte, extra []byte) {
	packet := make([]byte, 32, 32+len(extra))
	packet[0] = 1
	copy(packet[1:], head)
	binary.LittleEndian.PutUint16(packet[2:], seq)
	binary.LittleEndian.PutUint32(packet[4:], uint32(len(extra)/4))
	f.write(append(packet, extra...))
}

func (f *fakeClipboardX) event(code byte, fields ...uint32) {
	packet := make([]byte, 32)
	packet[0] = code
	for i, v := range fields {
		binary.LittleEndian.PutUint32(packet[4+4*i:], v)
	}
	f.write(packet)
}

func (f *fakeClipboardX) write(packet []byte) {
	f.t.Helper()
	_ = f.conn.SetWriteDeadline(time.Now().Add(3 * time.Second))
	if _, err := f.conn.Write(packet); err != nil {
		f.t.Fatalf("write: %v", err)
	}
}

const fakeClipboardXFixesEvent = 87

// startClipboard runs the client's setup against the fake server.
func startClipboard(t *testing.T) (*clipboardX11, *fakeClipboardX, chan clipboardEvent) {
	client, server := net.Pipe()
	f := &fakeClipboardX{t: t, conn: server, atoms: map[string]uint32{}}
	events := make(chan clipboardEvent, 4)
	c := &clipboardX11{x: &x11Conn{conn: client, root: 0x100, atoms: map[string]uint32{}, idBase: 0x400000, idMask: 0x1fffff},
		atoms: map[string]uint32{}, packets: make(chan x11Packet, 64), commands: make(chan func(), 16), events: events, done: make(chan struct{})}
	setup := make(chan error, 1)
	go func() { setup <- c.setup() }()
	for i := 0; i < 8; i++ {
		r := f.expect(x11InternAtom)
		name := string(r.data[4 : 4+binary.LittleEndian.Uint16(r.data)])
		f.atoms[name] = uint32(300 + i)
		head := make([]byte, 11)
		binary.LittleEndian.PutUint32(head[7:], f.atoms[name])
		f.reply(r.seq, head, nil)
	}
	r := f.expect(x11QueryExtension)
	if !bytes.Contains(r.data, []byte("XFIXES")) {
		t.Fatalf("extension query %q", r.data)
	}
	f.reply(r.seq, []byte{0, 0, 0, 0, 0, 0, 0, 1, 140, fakeClipboardXFixesEvent}, nil)
	r = f.expect(140)
	f.reply(r.seq, nil, nil)
	create := f.expect(x11CreateWindow)
	if window := binary.LittleEndian.Uint32(create.data); window != 0x400001 {
		t.Fatalf("window id %#x", window)
	}
	selection := f.expect(140)
	if selection.detail != xfixesSelectSelectionInput || binary.LittleEndian.Uint32(selection.data[4:]) != f.atoms["CLIPBOARD"] {
		t.Fatalf("selection input %+v", selection)
	}
	if err := <-setup; err != nil {
		t.Fatal(err)
	}
	go c.read()
	go c.run()
	t.Cleanup(func() { c.close(); server.Close() })
	return c, f, events
}

func receive(t *testing.T, events chan clipboardEvent) clipboardEvent {
	t.Helper()
	select {
	case event := <-events:
		return event
	case <-time.After(3 * time.Second):
		t.Fatal("no clipboard event")
		return clipboardEvent{}
	}
}

func TestClipboardReportsAGuestCopy(t *testing.T) {
	c, f, events := startClipboard(t)
	c.setWatch(true)
	// Commands run in order on the clipboard's goroutine: once this one ran, so did setWatch.
	applied := make(chan struct{})
	c.do(func() { close(applied) })
	<-applied
	// Another application takes CLIPBOARD; our own ownership would not be news.
	f.event(fakeClipboardXFixesEvent, 0x400001, 0x777, f.atoms["CLIPBOARD"])
	convert := f.expect(x11ConvertSelection)
	if binary.LittleEndian.Uint32(convert.data[8:]) != f.atoms["UTF8_STRING"] {
		t.Fatalf("convert target %v", convert.data)
	}
	// It offers no UTF8_STRING; STRING (Latin-1) is asked next.
	f.event(x11EventSelectionNotify, 0, 0x400001, f.atoms["CLIPBOARD"], f.atoms["UTF8_STRING"], 0)
	convert = f.expect(x11ConvertSelection)
	if binary.LittleEndian.Uint32(convert.data[8:]) != f.atoms["STRING"] {
		t.Fatalf("fallback target %v", convert.data)
	}
	f.event(x11EventSelectionNotify, 0, 0x400001, f.atoms["CLIPBOARD"], f.atoms["STRING"], f.atoms["VEYNODE_CLIPBOARD"])
	get := f.expect(x11GetProperty)
	if get.detail != 1 {
		t.Fatal("the transfer property is not deleted")
	}
	head := make([]byte, 19)
	head[0] = 8
	binary.LittleEndian.PutUint32(head[7:], f.atoms["STRING"])
	binary.LittleEndian.PutUint32(head[15:], 4)
	f.reply(get.seq, head, []byte("caf\xe9"))
	if event := receive(t, events); event.Text != "café" {
		t.Fatalf("event %+v", event)
	}
}

func TestClipboardServesItsPasteAndReadsAnIncrementalCopy(t *testing.T) {
	c, f, events := startClipboard(t)
	c.set("你好, world")
	owner := f.expect(x11SetSelectionOwner)
	if binary.LittleEndian.Uint32(owner.data) != 0x400001 {
		t.Fatalf("owner %v", owner.data)
	}
	// The XFixes echo of our own ownership is ignored: no request follows it.
	f.event(fakeClipboardXFixesEvent, 0x400001, 0x400001, f.atoms["CLIPBOARD"])
	f.event(x11EventSelectionRequest, 5, 0x400001, 0x888, f.atoms["CLIPBOARD"], f.atoms["TARGETS"], 0x55)
	targets := f.expect(x11ChangeProperty)
	if binary.LittleEndian.Uint32(targets.data[8:]) != f.atoms["ATOM"] || targets.data[12] != 32 || binary.LittleEndian.Uint32(targets.data[16:]) != 4 {
		t.Fatalf("targets %v", targets.data[:20])
	}
	f.expect(x11SendEvent)
	f.event(x11EventSelectionRequest, 6, 0x400001, 0x888, f.atoms["CLIPBOARD"], f.atoms["UTF8_STRING"], 0x56)
	text := f.expect(x11ChangeProperty)
	if got := string(text.data[20 : 20+binary.LittleEndian.Uint32(text.data[16:])]); got != "你好, world" {
		t.Fatalf("pasted %q", got)
	}
	notify := f.expect(x11SendEvent)
	if binary.LittleEndian.Uint32(notify.data) != 0x888 || notify.data[8] != x11EventSelectionNotify || binary.LittleEndian.Uint32(notify.data[28:]) != 0x56 {
		t.Fatalf("notify %v", notify.data)
	}
	// A target we do not offer is refused with property None.
	f.event(x11EventSelectionRequest, 7, 0x400001, 0x888, f.atoms["CLIPBOARD"], 999, 0x57)
	refused := f.expect(x11SendEvent)
	if binary.LittleEndian.Uint32(refused.data[28:]) != 0 {
		t.Fatal("unknown target was not refused")
	}

	// Another client copies a large text: INCR parts until an empty one.
	f.event(x11EventSelectionClear, 0, 0x400001, f.atoms["CLIPBOARD"])
	// Events and commands reach the clipboard on separate channels: wait until the clear is seen.
	for cleared := false; !cleared; time.Sleep(time.Millisecond) {
		seen := make(chan bool)
		c.do(func() { seen <- c.owned == nil })
		cleared = <-seen
	}
	c.get()
	f.expect(x11ConvertSelection)
	f.event(x11EventSelectionNotify, 0, 0x400001, f.atoms["CLIPBOARD"], f.atoms["UTF8_STRING"], f.atoms["VEYNODE_CLIPBOARD"])
	property := func(kind uint32, value string) {
		get := f.expect(x11GetProperty)
		head := make([]byte, 19)
		head[0] = 8
		binary.LittleEndian.PutUint32(head[7:], kind)
		binary.LittleEndian.PutUint32(head[15:], uint32(len(value)))
		f.reply(get.seq, head, append([]byte(value), make([]byte, pad4(len(value))-len(value))...))
	}
	property(f.atoms["INCR"], "\x00\x00\x01\x00")
	for _, part := range []string{"part one, ", "part two", ""} {
		f.event(x11EventPropertyNotify, 0x400001, f.atoms["VEYNODE_CLIPBOARD"], 0, 0)
		property(f.atoms["UTF8_STRING"], part)
	}
	if event := receive(t, events); event.Text != "part one, part two" {
		t.Fatalf("event %+v", event)
	}
}
