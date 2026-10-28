package main

// Desktop clipboard for the browser desktop (Apple Virtualization Linux
// desktops; QEMU uses SPICE's vdagent clipboard instead). The agent is a
// clipboard client of the container's X display like any application: it
// owns CLIPBOARD to paste the viewer's text, and watches the owner (XFixes)
// to copy the Guest's text back. Text only, UTF-8, at most clipboardMax bytes.
//
// One clipboard per desktop stream connection (desktop_linux.go):
//   host -> guest {"clipboard":"text"}      the Guest's clipboard becomes text
//                 {"clipboard_watch":bool}  send every Guest copy as an event
//                 {"clipboard_get":true}    send the Guest's clipboard now
//   guest -> host {"event":"clipboard","text":"..."} (or "error":"...")

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"sync"
	"time"
	"unicode/utf8"
)

const (
	// Under the core protocol's request limit (262140 bytes), so a paste never needs INCR.
	clipboardMax = 200 * 1024

	x11CreateWindow       = 1
	x11DestroyWindow      = 4
	x11ChangeProperty     = 18
	x11SetSelectionOwner  = 22
	x11ConvertSelection   = 24
	x11QueryExtension     = 98
	x11PropertyChangeMask = 1 << 22

	x11EventPropertyNotify     = 28
	x11EventSelectionClear     = 29
	x11EventSelectionRequest   = 30
	x11EventSelectionNotify    = 31
	xfixesQueryVersion         = 0
	xfixesSelectSelectionInput = 2
)

// clipboardEvent is what the stream reports: the text of a copy, or why there is none.
type clipboardEvent struct {
	Text  string
	Error string
}

type x11Packet struct {
	head  [32]byte
	extra []byte
}

type clipboardX11 struct {
	x      *x11Conn
	window uint32
	atoms  map[string]uint32
	xfixes struct{ opcode, event byte }
	// The text this client serves while it owns CLIPBOARD.
	owned []byte
	// A copy in flight: the requested target, and INCR parts when the owner sends them in pieces.
	fetching  uint32
	incr      *bytes.Buffer
	waitReply uint16
	watch     bool
	wanted    bool
	packets   chan x11Packet
	commands  chan func()
	events    chan<- clipboardEvent
	done      chan struct{}
	closeOnce sync.Once
}

func (c *clipboardX11) atom(name string) uint32 { return c.atoms[name] }

// openClipboard connects to the desktop's display and starts serving; events
// go to `events` until close.
func openClipboard(env desktopEnv, events chan<- clipboardEvent) (*clipboardX11, error) {
	x, err := env.dial()
	if err != nil {
		return nil, err
	}
	c := &clipboardX11{x: x, atoms: map[string]uint32{}, packets: make(chan x11Packet, 64), commands: make(chan func(), 16), events: events, done: make(chan struct{})}
	if err := c.setup(); err != nil {
		x.close()
		return nil, err
	}
	// Requests keep a write deadline; reads wait for events indefinitely.
	_ = x.conn.SetReadDeadline(time.Time{})
	go c.read()
	go c.run()
	return c, nil
}

func (c *clipboardX11) setup() error {
	for _, name := range []string{"CLIPBOARD", "UTF8_STRING", "TARGETS", "TEXT", "STRING", "INCR", "ATOM", "VEYNODE_CLIPBOARD"} {
		atom, err := c.x.atom(name)
		if err != nil {
			return err
		}
		c.atoms[name] = atom
	}
	name := "XFIXES"
	data := make([]byte, 4, 4+len(name))
	binary.LittleEndian.PutUint16(data, uint16(len(name)))
	reply, err := c.x.call(x11QueryExtension, 0, append(data, name...))
	if err != nil {
		return err
	}
	if reply[8] == 0 {
		return errors.New("the X server has no XFIXES extension")
	}
	c.xfixes.opcode, c.xfixes.event = reply[9], reply[10]
	version := make([]byte, 8)
	binary.LittleEndian.PutUint32(version, 5)
	if _, err := c.x.call(c.xfixes.opcode, xfixesQueryVersion, version); err != nil {
		return err
	}
	// An unmapped input-only window: the requestor and owner of transfers.
	c.window = c.x.idBase | (1 & c.x.idMask)
	window := make([]byte, 28)
	binary.LittleEndian.PutUint32(window[0:], c.window)
	binary.LittleEndian.PutUint32(window[4:], c.x.root)
	binary.LittleEndian.PutUint16(window[12:], 1)
	binary.LittleEndian.PutUint16(window[14:], 1)
	binary.LittleEndian.PutUint16(window[18:], 2) // InputOnly
	binary.LittleEndian.PutUint32(window[24:], 1<<11)
	window = binary.LittleEndian.AppendUint32(window, x11PropertyChangeMask)
	if _, err := c.x.send(x11CreateWindow, 0, window); err != nil {
		return err
	}
	selection := make([]byte, 12)
	binary.LittleEndian.PutUint32(selection[0:], c.window)
	binary.LittleEndian.PutUint32(selection[4:], c.atom("CLIPBOARD"))
	binary.LittleEndian.PutUint32(selection[8:], 1) // SetSelectionOwnerNotify
	_, err = c.x.send(c.xfixes.opcode, xfixesSelectSelectionInput, selection)
	return err
}

// read hands every packet from the server to run.
func (c *clipboardX11) read() {
	defer close(c.packets)
	for {
		var p x11Packet
		if _, err := io.ReadFull(c.x.conn, p.head[:]); err != nil {
			return
		}
		if p.head[0] == 1 {
			extra := int(binary.LittleEndian.Uint32(p.head[4:])) * 4
			if extra > x11MaxReply {
				return
			}
			p.extra = make([]byte, extra)
			if _, err := io.ReadFull(c.x.conn, p.extra); err != nil {
				return
			}
		}
		select {
		case c.packets <- p:
		case <-c.done:
			return
		}
	}
}

func (c *clipboardX11) run() {
	defer c.x.close()
	// A lost display ends the clipboard; the stream opens a new one on next use.
	defer c.close()
	for {
		select {
		case <-c.done:
			return
		case command := <-c.commands:
			command()
		case p, ok := <-c.packets:
			if !ok {
				c.report(clipboardEvent{Error: "the desktop's display closed"})
				return
			}
			if err := c.handle(p); err != nil {
				c.report(clipboardEvent{Error: err.Error()})
				return
			}
		}
	}
}

func (c *clipboardX11) report(event clipboardEvent) {
	select {
	case c.events <- event:
	case <-c.done:
	}
}

func (c *clipboardX11) do(command func()) {
	select {
	case c.commands <- command:
	case <-c.done:
	}
}

func (c *clipboardX11) close() { c.closeOnce.Do(func() { close(c.done) }) }

func (c *clipboardX11) closed() bool {
	select {
	case <-c.done:
		return true
	default:
		return false
	}
}

// set makes text the Guest's clipboard.
func (c *clipboardX11) set(text string) {
	c.do(func() {
		if len(text) > clipboardMax || !utf8.ValidString(text) {
			c.report(clipboardEvent{Error: fmt.Sprintf("clipboard text must be UTF-8 of at most %d bytes", clipboardMax)})
			return
		}
		c.owned = []byte(text)
		data := make([]byte, 12)
		binary.LittleEndian.PutUint32(data[0:], c.window)
		binary.LittleEndian.PutUint32(data[4:], c.atom("CLIPBOARD"))
		if _, err := c.send(x11SetSelectionOwner, 0, data); err != nil {
			c.report(clipboardEvent{Error: err.Error()})
		}
	})
}

// setWatch reports each later copy in the Guest when on.
func (c *clipboardX11) setWatch(on bool) { c.do(func() { c.watch = on }) }

// get reports the Guest's clipboard now.
func (c *clipboardX11) get() {
	c.do(func() {
		if c.owned != nil {
			c.report(clipboardEvent{Text: string(c.owned)})
			return
		}
		c.wanted = true
		c.convert(c.atom("UTF8_STRING"))
	})
}

func (c *clipboardX11) convert(target uint32) {
	c.fetching, c.incr = target, nil
	data := make([]byte, 20)
	binary.LittleEndian.PutUint32(data[0:], c.window)
	binary.LittleEndian.PutUint32(data[4:], c.atom("CLIPBOARD"))
	binary.LittleEndian.PutUint32(data[8:], target)
	binary.LittleEndian.PutUint32(data[12:], c.atom("VEYNODE_CLIPBOARD"))
	_, _ = c.send(x11ConvertSelection, 0, data)
}

// readProperty asks for (and deletes) the transfer property; the reply comes to handle.
func (c *clipboardX11) readProperty() {
	data := make([]byte, 20)
	binary.LittleEndian.PutUint32(data[0:], c.window)
	binary.LittleEndian.PutUint32(data[4:], c.atom("VEYNODE_CLIPBOARD"))
	binary.LittleEndian.PutUint32(data[16:], (clipboardMax+4)/4)
	c.waitReply, _ = c.send(x11GetProperty, 1, data)
}

func (c *clipboardX11) finish(text []byte, err error) {
	c.fetching, c.incr = 0, nil
	if !c.watch && !c.wanted {
		return
	}
	c.wanted = false
	switch {
	case err != nil:
		c.report(clipboardEvent{Error: err.Error()})
	case len(text) > clipboardMax:
		c.report(clipboardEvent{Error: fmt.Sprintf("the Guest's clipboard is over %d bytes", clipboardMax)})
	default:
		c.report(clipboardEvent{Text: string(text)})
	}
}

func (c *clipboardX11) handle(p x11Packet) error {
	head := p.head[:]
	u32 := func(at int) uint32 { return binary.LittleEndian.Uint32(head[at:]) }
	switch code := head[0] & 0x7f; {
	case code == 0:
		// An error for a transfer request ends that transfer; others are not ours to act on.
		if c.fetching != 0 {
			c.finish(nil, &x11Error{code: head[1], request: head[10]})
		}
	case code == 1:
		if binary.LittleEndian.Uint16(head[2:]) != c.waitReply || c.fetching == 0 {
			return nil
		}
		format, kind := head[1], u32(8)
		size := int(u32(16)) * max(int(format)/8, 1)
		if size > len(p.extra) {
			return errors.New("truncated X11 property")
		}
		value := p.extra[:size]
		switch {
		case kind == c.atom("INCR"):
			c.incr = &bytes.Buffer{}
		case c.incr != nil && size == 0:
			c.finish(c.incr.Bytes(), nil)
		case c.incr != nil:
			if c.incr.Len()+size > clipboardMax {
				c.incr = nil
				c.finish(make([]byte, clipboardMax+1), nil)
				return nil
			}
			c.incr.Write(value)
		case u32(12) != 0:
			c.finish(make([]byte, clipboardMax+1), nil)
		default:
			c.finish(textOf(value, kind == c.atom("STRING")), nil)
		}
	case code == c.xfixes.event:
		// The owner changed: a copy in the Guest (or our own paste, which is not news).
		if owner := u32(8); owner != c.window {
			c.owned = nil
			if c.watch {
				c.convert(c.atom("UTF8_STRING"))
			}
		}
	case code == x11EventSelectionNotify:
		if u32(8) != c.window || c.fetching == 0 {
			return nil
		}
		if u32(20) == 0 {
			// Refused: older applications offer only STRING.
			if c.fetching == c.atom("UTF8_STRING") {
				c.convert(c.atom("STRING"))
			} else {
				c.finish(nil, errors.New("the Guest's clipboard holds no text"))
			}
			return nil
		}
		c.readProperty()
	case code == x11EventPropertyNotify:
		// INCR: each new part is announced by the owner writing the property again.
		if c.incr != nil && u32(4) == c.window && u32(8) == c.atom("VEYNODE_CLIPBOARD") && head[16] == 0 {
			c.readProperty()
		}
	case code == x11EventSelectionClear:
		if u32(8) == c.window {
			c.owned = nil
		}
	case code == x11EventSelectionRequest:
		return c.serve(u32(4), u32(12), u32(16), u32(20), u32(24))
	}
	return nil
}

// textOf is a transfer's text as UTF-8 (STRING is Latin-1).
func textOf(value []byte, latin bool) []byte {
	if !latin {
		return value
	}
	out := make([]byte, 0, len(value))
	for _, b := range value {
		out = utf8.AppendRune(out, rune(b))
	}
	return out
}

// send is x11Conn.send for a connection whose reads wait for events: the
// request's deadline must not end the reader's wait.
func (c *clipboardX11) send(opcode, detail byte, data []byte) (uint16, error) {
	seq, err := c.x.send(opcode, detail, data)
	_ = c.x.conn.SetReadDeadline(time.Time{})
	return seq, err
}

// serve answers another client's paste of our text (ICCCM: write the property, then notify).
func (c *clipboardX11) serve(stamp, requestor, selection, target, property uint32) error {
	if property == 0 {
		property = target
	}
	answer := uint32(0)
	if c.owned != nil && selection == c.atom("CLIPBOARD") {
		var kind uint32
		var format byte = 8
		var value []byte
		switch target {
		case c.atom("TARGETS"):
			kind, format = c.atom("ATOM"), 32
			for _, name := range []string{"TARGETS", "UTF8_STRING", "TEXT", "STRING"} {
				value = binary.LittleEndian.AppendUint32(value, c.atom(name))
			}
		case c.atom("UTF8_STRING"), c.atom("TEXT"):
			kind, value = c.atom("UTF8_STRING"), c.owned
		case c.atom("STRING"):
			kind, value = c.atom("STRING"), latin1Of(c.owned)
		}
		if kind != 0 {
			data := make([]byte, 20, 20+len(value))
			binary.LittleEndian.PutUint32(data[0:], requestor)
			binary.LittleEndian.PutUint32(data[4:], property)
			binary.LittleEndian.PutUint32(data[8:], kind)
			data[12] = format
			binary.LittleEndian.PutUint32(data[16:], uint32(len(value)/(int(format)/8)))
			if _, err := c.send(x11ChangeProperty, 0, append(data, value...)); err != nil {
				return err
			}
			answer = property
		}
	}
	notify := make([]byte, 40)
	binary.LittleEndian.PutUint32(notify[0:], requestor)
	event := notify[8:]
	event[0] = x11EventSelectionNotify
	binary.LittleEndian.PutUint32(event[4:], stamp)
	binary.LittleEndian.PutUint32(event[8:], requestor)
	binary.LittleEndian.PutUint32(event[12:], selection)
	binary.LittleEndian.PutUint32(event[16:], target)
	binary.LittleEndian.PutUint32(event[20:], answer)
	_, err := c.send(x11SendEvent, 0, notify)
	return err
}

// latin1Of is text as Latin-1, with '?' for what Latin-1 cannot hold.
func latin1Of(text []byte) []byte {
	out := make([]byte, 0, len(text))
	for _, r := range string(text) {
		if r > 0xff {
			r = '?'
		}
		out = append(out, byte(r))
	}
	return out
}
