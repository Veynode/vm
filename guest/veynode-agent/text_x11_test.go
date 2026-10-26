package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// fakeXServer is an in-process X11 server speaking the wire subset the injector
// uses. It is this repository's reading of the X11/XTEST encodings, so it
// proves framing and sequencing consistency, not acceptance by a real Xorg.
type fakeXServer struct {
	path     string
	cookie   []byte
	xtest    bool
	focus    uint32
	mask     uint16
	rejectAt int // ChangeKeyboardMapping number that answers BadValue (1-based)

	// clientCost models the focused client: it handles one key press per
	// clientCost and resolves it against the table as it is at that moment.
	clientCost time.Duration

	mu       sync.Mutex
	table    [248][4]uint32 // keycodes 8..255
	initial  [248][4]uint32
	history  []fakeChange
	presses  []fakePress
	down     map[byte]bool
	changes  int
	problems []string
}

func startFakeX(t *testing.T, configure func(*fakeXServer)) *fakeXServer {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "vn-x-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	s := &fakeXServer{path: filepath.Join(dir, "X0"), xtest: true, focus: 0x200001, down: map[byte]bool{}}
	// A realistic table: low keycodes are bound, 20 high keycodes are free.
	for i := range s.table {
		if i < len(s.table)-20 {
			s.table[i] = [4]uint32{0x61, 0x41, 0, 0}
		}
	}
	if configure != nil {
		configure(s)
	}
	s.initial = s.table
	listener, err := net.Listen("unix", s.path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	go func() {
		for {
			c, err := listener.Accept()
			if err != nil {
				return
			}
			go s.serve(c)
		}
	}()
	return s
}

// decoded is what the modelled client has resolved by `until`; presses it has
// not reached yet are not included.
func (s *fakeXServer) decoded(until time.Time) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []rune
	var free time.Time
	for _, press := range s.presses {
		at := press.at
		if free.After(at) {
			at = free
		}
		at = at.Add(s.clientCost)
		free = at
		if at.After(until) {
			break
		}
		keysym := s.initial[press.keycode-8][0]
		for _, change := range s.history {
			if change.keycode == press.keycode && !change.at.After(at) {
				keysym = change.keysym
			}
		}
		out = append(out, keysymRune(keysym))
	}
	return string(out)
}

// drained is when the modelled client has handled every press so far.
func (s *fakeXServer) drained() time.Time {
	s.mu.Lock()
	defer s.mu.Unlock()
	var free time.Time
	for _, press := range s.presses {
		at := press.at
		if free.After(at) {
			at = free
		}
		free = at.Add(s.clientCost)
	}
	return free
}
func (s *fakeXServer) problem(text string) {
	s.mu.Lock()
	s.problems = append(s.problems, text)
	s.mu.Unlock()
}
func (s *fakeXServer) serve(c net.Conn) {
	defer c.Close()
	le := binary.LittleEndian
	head := make([]byte, 12)
	if _, err := io.ReadFull(c, head); err != nil {
		return
	}
	nameLen, dataLen := int(le.Uint16(head[6:])), int(le.Uint16(head[8:]))
	auth := make([]byte, nameLen+xPad(nameLen)+dataLen+xPad(dataLen))
	if _, err := io.ReadFull(c, auth); err != nil {
		return
	}
	if head[0] != 'l' || le.Uint16(head[2:]) != 11 || le.Uint16(head[4:]) != 0 {
		s.problem("setup header")
	}
	authorized := len(s.cookie) == 0 || (string(auth[:nameLen]) == "MIT-MAGIC-COOKIE-1" && bytes.Equal(auth[nameLen+xPad(nameLen):][:dataLen], s.cookie))
	if !authorized {
		reason := []byte("no auth ")
		refuse := []byte{0, byte(len(reason)), 11, 0, 0, 0, byte(len(reason) / 4), 0}
		c.Write(append(refuse, reason...))
		return
	}
	vendor := []byte("fake")
	extra := make([]byte, 32, 96)
	le.PutUint16(extra[16:], uint16(len(vendor)))
	le.PutUint16(extra[18:], 65535)
	extra[20], extra[21] = 1, 1
	extra[26], extra[27] = 8, 255
	extra = append(extra, vendor...)
	extra = append(extra, make([]byte, 8)...)  // one pixmap format
	extra = append(extra, 0x9a, 0x02, 0, 0)    // root window 0x29a
	extra = append(extra, make([]byte, 36)...) // rest of the screen
	setup := []byte{1, 0, 11, 0, 0, 0, 0, 0}
	le.PutUint16(setup[6:], uint16(len(extra)/4))
	if _, err := c.Write(append(setup, extra...)); err != nil {
		return
	}
	reply := func(seq uint16, first byte, body []byte, extraUnits []byte) {
		packet := make([]byte, 32)
		packet[0], packet[1] = 1, first
		le.PutUint16(packet[2:], seq)
		le.PutUint32(packet[4:], uint32(len(extraUnits)/4))
		copy(packet[8:], body)
		c.Write(append(packet, extraUnits...))
	}
	var seq uint16
	for {
		header := make([]byte, 4)
		if _, err := io.ReadFull(c, header); err != nil {
			return
		}
		body := make([]byte, 4*int(le.Uint16(header[2:]))-4)
		if _, err := io.ReadFull(c, body); err != nil {
			return
		}
		seq++
		switch header[0] {
		case xOpQueryExtension:
			if string(body[4:4+le.Uint16(body)]) != "XTEST" {
				s.problem("extension name")
			}
			present := []byte{0, 0, 0, 0}
			if s.xtest {
				present = []byte{1, 132, 0, 0}
			}
			reply(seq, 0, present, nil)
		case xOpGetInputFocus:
			focus := make([]byte, 4)
			le.PutUint32(focus, s.focus)
			reply(seq, 1, focus, nil)
		case xOpQueryPointer:
			if le.Uint32(body) != 0x29a {
				s.problem("pointer query window")
			}
			pointer := make([]byte, 24)
			le.PutUint32(pointer, 0x29a)
			le.PutUint16(pointer[16:], s.mask)
			reply(seq, 1, pointer, nil)
		case xOpGetKeyboardMapping:
			first, count := body[0], int(body[1])
			if first != 8 || count != 248 {
				s.problem("keyboard mapping range")
			}
			s.mu.Lock()
			syms := make([]byte, 0, 4*4*count)
			for _, row := range s.table {
				for _, sym := range row {
					syms = le.AppendUint32(syms, sym)
				}
			}
			s.mu.Unlock()
			reply(seq, 4, nil, syms)
		case xOpChangeKeyboardMapping:
			s.mu.Lock()
			s.changes++
			rejected := s.changes == s.rejectAt
			if !rejected {
				if header[1] != 1 || body[1] != 2 || len(body) != 12 || body[0] < 8 {
					s.problems = append(s.problems, "change mapping layout")
				} else {
					s.table[body[0]-8] = [4]uint32{le.Uint32(body[4:]), le.Uint32(body[8:]), 0, 0}
					s.history = append(s.history, fakeChange{time.Now(), body[0], le.Uint32(body[4:])})
				}
			}
			s.mu.Unlock()
			packet := make([]byte, 32)
			if rejected {
				packet[1] = 2 // BadValue
				le.PutUint16(packet[2:], seq)
				packet[10] = xOpChangeKeyboardMapping
			} else {
				packet[0] = 34 // MappingNotify goes to every client
				le.PutUint16(packet[2:], seq)
				packet[4], packet[5], packet[6] = 1, body[0], 1
			}
			c.Write(packet)
		case 132:
			s.mu.Lock()
			keycode := body[1]
			zero := true
			for _, b := range append(append([]byte{}, body[2:20]...), body[24:]...) {
				zero = zero && b == 0
			}
			if header[1] != xtestFakeInput || len(body) != 32 || !zero || keycode < 8 {
				s.problems = append(s.problems, "fake input layout")
			} else if body[0] == xKeyPress {
				if s.down[keycode] {
					s.problems = append(s.problems, "press while down")
				}
				s.down[keycode] = true
				s.presses = append(s.presses, fakePress{time.Now(), keycode})
			} else if body[0] == xKeyRelease {
				if !s.down[keycode] {
					s.problems = append(s.problems, "release while up")
				}
				delete(s.down, keycode)
			} else {
				s.problems = append(s.problems, "fake input type")
			}
			s.mu.Unlock()
		default:
			s.problem("unexpected opcode")
			return
		}
	}
}

// service has no background restore: tests restore explicitly.
func (s *fakeXServer) service(cookie []byte) *textService {
	return &textService{diagnostic: true, sleep: time.Sleep, now: time.Now, schedule: func(time.Duration) {}, dial: func(budget time.Duration) (xKeyboard, error) {
		return dialText(s.path, cookie, budget)
	}}
}

// check waits for the modelled client to drain and compares what it decoded.
func (s *fakeXServer) check(t *testing.T, typed string) {
	t.Helper()
	time.Sleep(time.Until(s.drained()))
	got := s.decoded(time.Now())
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.problems) != 0 {
		t.Fatal(s.problems)
	}
	if got != typed || !bytes.Equal([]byte(got), []byte(typed)) {
		t.Fatalf("client decoded %q (% x), want %q (% x)", got, got, typed, typed)
	}
	if len(s.down) != 0 {
		t.Fatal("keys left down", s.down)
	}
}

// restored runs the deferred restore now and requires the original table back.
func (s *fakeXServer) restored(t *testing.T, service *textService, want int) {
	t.Helper()
	restored, remaining, err := service.restoreAll()
	s.mu.Lock()
	defer s.mu.Unlock()
	if err != nil || remaining != 0 || restored != want || s.table != s.initial {
		t.Fatalf("restore: %d restored (want %d), %d remaining, %v, table restored %v", restored, want, remaining, err, s.table == s.initial)
	}
}

func TestX11WireInjectionIsByteExactAndRestoresKeycodes(t *testing.T) {
	cookie := bytes.Repeat([]byte{0x5a}, 16)
	s := startFakeX(t, func(s *fakeXServer) { s.cookie = cookie })
	service := s.service(cookie)
	status := service.status()
	if status["available"] != true || status["spare_keycodes"] != 20 || status["bound_keycodes"] != 0 {
		t.Fatal(status)
	}
	text := mixedText + "\nq\t"
	started := time.Now()
	reply := service.handle(textRequest(text))
	if reply.Error != nil || time.Since(started) > 900*time.Millisecond {
		t.Fatal(reply.Error, time.Since(started))
	}
	s.check(t, text)
	// One mapping write per distinct character, each read back from the server before any key.
	line, _ := json.Marshal(reply.Result)
	var result struct {
		Bindings []struct {
			Keycode  byte
			Keysym   uint32
			Readback []uint32
			Reused   bool
		}
		Batches int
	}
	if err := json.Unmarshal(line, &result); err != nil || len(result.Bindings) != 8 || result.Batches != 1 || s.changes != 8 {
		t.Fatal(err, string(line), s.changes)
	}
	seen := map[uint32]bool{}
	for _, binding := range result.Bindings {
		if binding.Keycode < 236 || binding.Reused || len(binding.Readback) != 4 || binding.Readback[0] != binding.Keysym || binding.Readback[1] != binding.Keysym || seen[binding.Keysym] {
			t.Fatal(string(line))
		}
		seen[binding.Keysym] = true
	}
	if status = service.status(); status["spare_keycodes"] != 12 || status["bound_keycodes"] != 8 || status["available"] != true {
		t.Fatal(status)
	}
	// The same text again reuses every binding: no mapping write at all.
	if reply = service.handle(textRequest(text)); reply.Error != nil || s.changes != 8 {
		t.Fatal(reply.Error, s.changes)
	}
	s.check(t, text+text)
	s.restored(t, service, 8)
	if s.changes != 16 {
		t.Fatal("unexpected mapping writes", s.changes)
	}
}

// The failure of the first real-Xorg run at wire level: the client needs 15 ms
// per key, so it is still working through the events when the request returns.
// Restoring at that moment (the retired behaviour) makes it resolve the rest
// against empty keycodes; restoring after the quiet period loses nothing.
func TestX11SlowClientNeedsBindingsToOutliveTheRequest(t *testing.T) {
	early := startFakeX(t, func(s *fakeXServer) { s.clientCost = 15 * time.Millisecond })
	service := early.service(nil)
	if reply := service.handle(textRequest(sixteenText)); reply.Error != nil {
		t.Fatal(reply.Error)
	}
	early.restored(t, service, 16)
	time.Sleep(time.Until(early.drained()))
	lost := early.decoded(time.Now())
	kept := strings.TrimRight(lost, "\ufffd")
	if lost == sixteenText || len([]rune(lost)) != 16 || kept == "" || !strings.HasPrefix(sixteenText, kept) {
		t.Fatalf("immediate restore should lose a contiguous tail, client decoded %q", lost)
	}

	kept16 := startFakeX(t, func(s *fakeXServer) { s.clientCost = 15 * time.Millisecond })
	service = kept16.service(nil)
	reply := service.handle(textRequest(sixteenText))
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	sent := time.Now()
	kept16.check(t, sixteenText)
	if backlog := kept16.drained().Sub(sent); backlog <= 0 || backlog > textQuietPeriod {
		t.Fatal("client backlog outside the modelled range", backlog)
	}
	time.Sleep(time.Until(sent.Add(textQuietPeriod)))
	kept16.restored(t, service, 16)
	kept16.check(t, sixteenText)
}

func TestX11UnavailableDisplayIsUnsupported(t *testing.T) {
	missing := &textService{sleep: time.Sleep, now: time.Now, dial: func(budget time.Duration) (xKeyboard, error) {
		return dialText("/tmp/vn-x-absent/X0", nil, budget)
	}}
	for name, service := range map[string]*textService{
		"no socket":    missing,
		"wrong cookie": startFakeX(t, func(s *fakeXServer) { s.cookie = bytes.Repeat([]byte{1}, 16) }).service(bytes.Repeat([]byte{2}, 16)),
		"no cookie":    startFakeX(t, func(s *fakeXServer) { s.cookie = bytes.Repeat([]byte{1}, 16) }).service(nil),
		"no XTEST":     startFakeX(t, func(s *fakeXServer) { s.xtest = false }).service(nil),
		"few spares": startFakeX(t, func(s *fakeXServer) {
			for i := range s.table {
				if i < len(s.table)-(minSpareKeycodes-1) {
					s.table[i] = [4]uint32{0, 0, 0, 0x62}
				}
			}
		}).service(nil),
	} {
		reply := service.handle(textRequest(mixedText))
		if reply.Error == nil || reply.Error.Code != codeUnsupported {
			t.Fatalf("%s: %+v", name, reply)
		}
		if status := service.status(); status["available"] != false || status["reason"] == "" {
			t.Fatalf("%s: %v", name, status)
		}
	}
}

func TestX11ServerErrorAndStateSurface(t *testing.T) {
	s := startFakeX(t, func(s *fakeXServer) { s.rejectAt = 2 })
	service := s.service(nil)
	reply := service.handle(textRequest("ab"))
	if reply.Error == nil || reply.Error.Code != codeInjectFailed || !strings.Contains(reply.Error.Message, "nothing was injected") {
		t.Fatal(reply)
	}
	s.check(t, "")
	s.restored(t, service, 1) // the one binding the server applied
	s = startFakeX(t, func(s *fakeXServer) { s.focus = 0 })
	if reply = s.service(nil).handle(textRequest("ab")); reply.Error == nil || reply.Error.Code != codeNotReady {
		t.Fatal(reply)
	}
	s.check(t, "")
	s = startFakeX(t, func(s *fakeXServer) { s.mask = 0x04 })
	if reply = s.service(nil).handle(textRequest("ab")); reply.Error == nil || reply.Error.Code != codeNotReady {
		t.Fatal(reply)
	}
	s.check(t, "")
}

func TestX11HungServerEndsInsideBudget(t *testing.T) {
	dir, err := os.MkdirTemp("/tmp", "vn-x-")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(dir)
	listener, err := net.Listen("unix", filepath.Join(dir, "X0"))
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	go func() {
		for {
			c, err := listener.Accept()
			if err != nil {
				return
			}
			defer c.Close() // never answers setup
		}
	}()
	service := &textService{sleep: time.Sleep, now: time.Now, dial: func(budget time.Duration) (xKeyboard, error) {
		return dialText(filepath.Join(dir, "X0"), nil, budget)
	}}
	started := time.Now()
	reply := service.handle(textRequest("a"))
	if reply.Error == nil || reply.Error.Code != codeUnsupported || time.Since(started) > 900*time.Millisecond {
		t.Fatal(reply, time.Since(started))
	}
}

func TestDisplayAndAuthorityParsing(t *testing.T) {
	for display, want := range map[string]int{":0": 0, ":1": 1, ":99": 99, ":0.0": 0, ":7.1": 7} {
		if got, err := parseDisplay(display); err != nil || got != want {
			t.Fatal(display, got, err)
		}
	}
	for _, display := range []string{"", "0", "localhost:0", "host/unix:0", ":", ":-1", ":100", ":01", ":0.", ":0.x", ":a", "tcp/:0"} {
		if _, err := parseDisplay(display); err == nil {
			t.Fatal("accepted", display)
		}
	}
	if _, err := newX11TextService("remote:0", ""); err == nil {
		t.Fatal("remote display accepted")
	}
	entry := func(family uint16, address, number, name string, data []byte) []byte {
		out := binary.BigEndian.AppendUint16(nil, family)
		for _, field := range [][]byte{[]byte(address), []byte(number), []byte(name), data} {
			out = binary.BigEndian.AppendUint16(out, uint16(len(field)))
			out = append(out, field...)
		}
		return out
	}
	one, two := bytes.Repeat([]byte{1}, 16), bytes.Repeat([]byte{2}, 16)
	file := append(entry(256, "guest", "1", "MIT-MAGIC-COOKIE-1", one), entry(256, "guest", "0", "MIT-MAGIC-COOKIE-1", two)...)
	if cookie, ok := parseXauthority(file, 0); !ok || !bytes.Equal(cookie, two) {
		t.Fatal(cookie, ok)
	}
	if cookie, ok := parseXauthority(file, 1); !ok || !bytes.Equal(cookie, one) {
		t.Fatal(cookie, ok)
	}
	for name, data := range map[string][]byte{
		"other display": entry(256, "guest", "2", "MIT-MAGIC-COOKIE-1", one),
		"other scheme":  entry(256, "guest", "0", "XDM-AUTHORIZATION-1", one),
		"internet":      entry(0, "\x7f\x00\x00\x01", "0", "MIT-MAGIC-COOKIE-1", one),
		"short cookie":  entry(256, "guest", "0", "MIT-MAGIC-COOKIE-1", one[:8]),
		"truncated":     file[:len(file)-3],
		"empty":         nil,
	} {
		if _, ok := parseXauthority(data, 0); ok {
			t.Fatal("accepted", name)
		}
	}
	if _, ok := parseXauthority(entry(256, "guest", "0", "MIT-MAGIC-COOKIE-1", one)[:20], 0); ok {
		t.Fatal("accepted truncated entry")
	}
	dir := t.TempDir()
	path := filepath.Join(dir, "Xauthority")
	if err := os.WriteFile(path, file, 0600); err != nil {
		t.Fatal(err)
	}
	if cookie, err := readXauthority(path, 0); err != nil || !bytes.Equal(cookie, two) {
		t.Fatal(cookie, err)
	}
	if _, err := readXauthority(filepath.Join(dir, "absent"), 0); err == nil {
		t.Fatal("absent authority accepted")
	}
}

// Test executable only: the real serial v2 service with text input backed by the
// in-process X server, for the Rust host planner to drive over a Unix socket.
// It proves host/Guest wire agreement, not delivery into a real Guest desktop.
func TestSerialTextInteropProcess(t *testing.T) {
	path := os.Getenv("VEYNODE_SERIAL_TEXT_SOCKET")
	if path == "" {
		return
	}
	want, err := hex.DecodeString(os.Getenv("VEYNODE_SERIAL_TEXT_EXPECT_HEX"))
	if err != nil || !filepath.IsAbs(path) {
		t.Fatal("absolute test socket and expected hex required")
	}
	x := startFakeX(t, nil)
	textInput = x.service(nil)
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: path, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	if err = listener.SetDeadline(time.Now().Add(20 * time.Second)); err != nil {
		t.Fatal(err)
	}
	c, err := listener.AcceptUnix()
	if err != nil {
		t.Fatal(err)
	}
	m := testManager(t)
	if m.identity.UID == 0 {
		c.Close()
		t.Fatal("interop fixture requires nonroot host user")
	}
	m.identity.Rootless = true
	m.containmentMode = "cgroup-v2-clone3-kill"
	done := make(chan error, 1)
	go func() { done <- serveSerialExec(c, m) }()
	select {
	case err = <-done:
		if err == nil {
			t.Fatal("EOF must terminate incarnation")
		}
	case <-time.After(20 * time.Second):
		c.Close()
		t.Fatal("interop exceeded 20s")
	}
	x.check(t, string(want))
	if _, remaining, err := textInput.restoreAll(); err != nil || remaining != 0 || x.table != x.initial {
		t.Fatal("keymap not restored", remaining, err)
	}
}
