package main

import (
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"
)

// fakeDisplay models what the injector depends on: the server's keycode table
// with its history, a virtual clock, and a client that resolves each key press
// against the table as it is when the client gets to that event. clientCost is
// the client's processing time per key; a positive value builds a backlog.
type fakeDisplay struct {
	clock      time.Time
	focus      uint32
	mask       uint16
	rows       [248][4]uint32 // keycodes 8..255
	history    []fakeChange
	presses    []fakePress
	down       map[byte]bool
	clientCost time.Duration
	echoGroups bool // report a binding in the second group's columns too, as XKB servers do
	ignoreMap  func(keycode byte) bool
	maps       int
	dials      int
	dialErr    error
	failPlan   []int // failAfter for successive connections
	slept      time.Duration
	armed      []time.Duration
}
type fakeChange struct {
	at      time.Time
	keycode byte
	keysym  uint32
}
type fakePress struct {
	at      time.Time
	keycode byte
}
type fakeKeyboard struct {
	shared    *fakeDisplay
	failAfter int // fail the Nth operation on this connection (1-based); 0 = never
	ops       int
	closed    bool
}

// newFakeDisplay leaves `spare` keycodes free, highest first, with bound keys between them.
func newFakeDisplay(spare int) *fakeDisplay {
	d := &fakeDisplay{clock: time.Unix(1000, 0), focus: 0x400001, down: map[byte]bool{}}
	for i := range d.rows {
		d.rows[i] = [4]uint32{0x61, 0x41, 0, 0}
	}
	for i := 0; i < spare; i++ {
		d.rows[247-2*i] = [4]uint32{}
	}
	return d
}
func (d *fakeDisplay) service() *textService {
	return &textService{
		diagnostic: true,
		sleep:      func(t time.Duration) { d.slept += t; d.clock = d.clock.Add(t) },
		now:        func() time.Time { return d.clock },
		schedule:   func(after time.Duration) { d.armed = append(d.armed, after) },
		dial: func(time.Duration) (xKeyboard, error) {
			if d.dialErr != nil {
				return nil, d.dialErr
			}
			k := &fakeKeyboard{shared: d}
			if d.dials < len(d.failPlan) {
				k.failAfter = d.failPlan[d.dials]
			}
			d.dials++
			return k, nil
		}}
}

// text is what the modelled client decodes, each press resolved against the
// table at the moment the client processes it.
func (d *fakeDisplay) text() string {
	var out []rune
	var free time.Time
	for _, press := range d.presses {
		at := press.at
		if free.After(at) {
			at = free
		}
		at = at.Add(d.clientCost)
		free = at
		keysym := uint32(0x61) // initial table content of a non-spare key
		known := false
		for _, change := range d.history {
			if change.keycode == press.keycode && !change.at.After(at) {
				keysym, known = change.keysym, true
			}
		}
		if !known {
			keysym = 0
		}
		out = append(out, keysymRune(keysym))
	}
	return string(out)
}
func (d *fakeDisplay) spareBound() int {
	bound := 0
	for i := 0; i < 124; i++ {
		if d.rows[247-2*i] != [4]uint32{} && d.rows[247-2*i] != [4]uint32{0x61, 0x41, 0, 0} {
			bound++
		}
	}
	return bound
}
func (k *fakeKeyboard) step() error {
	k.ops++
	if k.closed {
		return errors.New("closed")
	}
	if k.failAfter != 0 && k.ops >= k.failAfter {
		return errors.New("broken connection")
	}
	return nil
}
func (k *fakeKeyboard) State() (uint32, uint16, error) {
	return k.shared.focus, k.shared.mask, k.step()
}
func (k *fakeKeyboard) Keymap() (byte, [][]uint32, error) {
	if err := k.step(); err != nil {
		return 0, nil, err
	}
	rows := make([][]uint32, len(k.shared.rows))
	for i := range rows {
		rows[i] = append([]uint32{}, k.shared.rows[i][:]...)
	}
	return 8, rows, nil
}
func (k *fakeKeyboard) Map(keycode byte, keysym uint32) error {
	if err := k.step(); err != nil {
		return err
	}
	d := k.shared
	d.maps++
	if d.ignoreMap != nil && d.ignoreMap(keycode) {
		return nil
	}
	d.rows[keycode-8] = [4]uint32{keysym, keysym, 0, 0}
	if d.echoGroups {
		d.rows[keycode-8] = [4]uint32{keysym, keysym, keysym, keysym}
	}
	d.history = append(d.history, fakeChange{d.clock, keycode, keysym})
	return nil
}
func (k *fakeKeyboard) Key(keycode byte, down bool) error {
	if err := k.step(); err != nil {
		return err
	}
	d := k.shared
	if down {
		if d.down[keycode] {
			return errors.New("double press")
		}
		d.down[keycode] = true
		d.presses = append(d.presses, fakePress{d.clock, keycode})
	} else {
		delete(d.down, keycode)
	}
	return nil
}
func (k *fakeKeyboard) Sync() error  { return k.step() }
func (k *fakeKeyboard) Close() error { k.closed = true; return nil }

// keysymRune is the inverse an X client applies (Xlib keysym to UCS rule).
func keysymRune(sym uint32) rune {
	switch {
	case sym == keysymReturn:
		return '\n'
	case sym == keysymTab:
		return '\t'
	case sym&0xff000000 == keysymUnicodeFlag:
		return rune(sym & 0x00ffffff)
	case (sym >= 0x20 && sym <= 0x7e) || (sym >= 0xa0 && sym <= 0xff):
		return rune(sym)
	}
	return -1
}
func textRequest(s string) request {
	return request{ID: "t", Method: "text_input", TextHex: hex.EncodeToString([]byte(s))}
}

const mixedText = "\u4f60\u597de\u0301\U0001f600" // CJK, combining acute, emoji outside the BMP
// Sixteen distinct scalars from eight scripts; the string of the first real-Xorg failure.
const sixteenText = "aZ9 \u00e9\u00bf\u00df\u0416\u044f\u3042\u30ab\u03a9\u2192\u8a9e\ud55c\U0001f600"

func TestTextValidationIsStrict(t *testing.T) {
	good := []string{"a", mixedText, sixteenText, "line\n\ttab", "\u200d\ufe0f", strings.Repeat("\U0001f600", maxTextScalars), "\ufffd", "\U0010fffd"}
	for _, s := range good {
		runes, size, err := decodeText(hex.EncodeToString([]byte(s)))
		if err != nil || string(runes) != s || size != len(s) {
			t.Fatalf("%q rejected: %v", s, err)
		}
	}
	bad := map[string]string{
		"empty":            "",
		"odd":              "abc",
		"uppercase":        "E4BDA0",
		"not hex":          "zz",
		"NUL":              hex.EncodeToString([]byte{0}),
		"CR":               hex.EncodeToString([]byte("\r")),
		"ESC":              hex.EncodeToString([]byte("\x1b")),
		"DEL":              hex.EncodeToString([]byte("\x7f")),
		"C1":               hex.EncodeToString([]byte("\u0085")),
		"noncharacter":     hex.EncodeToString([]byte("\ufffe")),
		"noncharacter2":    hex.EncodeToString([]byte("\ufdd0")),
		"plane nonchar":    hex.EncodeToString([]byte("\U0001ffff")),
		"lone surrogate":   "eda080",
		"overlong":         "c080",
		"truncated":        "e4bd",
		"above U+10FFFF":   "f4908080",
		"too many scalars": hex.EncodeToString([]byte(strings.Repeat("a", maxTextScalars+1))),
		"too many bytes":   strings.Repeat("61", maxTextBytes+1),
	}
	for name, value := range bad {
		if _, _, err := decodeText(value); err == nil {
			t.Fatalf("%s accepted", name)
		}
		d := newFakeDisplay(16)
		reply := d.service().handle(request{ID: "t", TextHex: value})
		if reply.Error == nil || reply.Error.Code != codeInvalid || d.dials != 0 {
			t.Fatalf("%s reached the display: %+v", name, reply)
		}
	}
}

func TestTextKeysyms(t *testing.T) {
	for r, want := range map[rune]uint32{'a': 0x61, '~': 0x7e, '\u00e9': 0xe9, '\n': 0xff0d, '\t': 0xff09, '\u0100': 0x01000100, '\u4f60': 0x01004f60, '\u0301': 0x01000301, '\U0001f600': 0x0101f600} {
		if got := textKeysym(r); got != want || keysymRune(got) != r {
			t.Fatalf("U+%04X: %#x", r, got)
		}
	}
}

func TestTextInjectionDeliversExactScalarsAndVerifiesBindings(t *testing.T) {
	for _, spare := range []int{minSpareKeycodes, 11, 16, 100} {
		for _, groups := range []bool{false, true} {
			for _, text := range []string{mixedText, sixteenText, "aaaa", "abcdefghijklmnop", "a\nb\tc", "\U0001f600\U0001f600e\u0301e\u0301"} {
				d := newFakeDisplay(spare)
				d.echoGroups = groups
				s := d.service()
				reply := s.handle(textRequest(text))
				if reply.Error != nil {
					t.Fatal(text, spare, reply.Error)
				}
				result := reply.Result.(map[string]any)
				if d.text() != text || result["injected_scalars"] != len([]rune(text)) || result["injected_bytes"] != len(text) || result["keymap_restore"] != "deferred" || result["guest_delivery_confirmed"] != false || result["backend"] != textBackend {
					t.Fatalf("%q with %d spare: typed %q result %v", text, spare, d.text(), result)
				}
				if len(d.down) != 0 || d.dials != 1 || d.slept+textWaitMargin > textRequestBudget {
					t.Fatalf("%q with %d spare: keys down %v, slept %v", text, spare, d.down, d.slept)
				}
				// Bindings stay for reuse; the deferred restore is armed and later clears exactly them.
				if d.spareBound() == 0 || len(d.armed) != 1 || d.armed[0] != textRestoreIdle {
					t.Fatal("bindings must outlive the request with a restore armed", d.spareBound(), d.armed)
				}
				if restored, remaining, err := s.restoreAll(); err != nil || remaining != 0 || restored == 0 || d.spareBound() != 0 {
					t.Fatal(restored, remaining, err, d.spareBound())
				}
			}
		}
	}
}

// The first real-Xorg run: sixteen distinct characters, a client that needs
// about 15 ms per key, and a restore 60 ms after the last key. The client was
// still nine keys behind and resolved the rest against the restored mapping.
func TestSlowClientLosesCharactersOnlyIfBindingsAreRestoredEarly(t *testing.T) {
	d := newFakeDisplay(16)
	d.clientCost = 15 * time.Millisecond
	s := d.service()
	if reply := s.handle(textRequest(sixteenText)); reply.Error != nil {
		t.Fatal(reply.Error)
	}
	// The retired behaviour, forced: restore shortly after the last key event.
	early := *d
	early.history = append([]fakeChange{}, d.history...)
	for keycode := byte(8); keycode != 0; keycode++ {
		if row := d.rows[keycode-8]; row != [4]uint32{} && row != [4]uint32{0x61, 0x41, 0, 0} {
			early.history = append(early.history, fakeChange{d.clock.Add(60 * time.Millisecond), keycode, 0})
		}
	}
	lost := early.text()
	if lost == sixteenText || !strings.HasPrefix(sixteenText, strings.TrimRight(lost, "\ufffd")) || strings.Count(lost, "\ufffd") < 5 {
		t.Fatalf("early restore should lose the tail for a slow client, decoded %q", lost)
	}
	// The implemented behaviour: nothing is restored before textRestoreIdle.
	d.clock = d.clock.Add(textRestoreIdle - time.Millisecond)
	s.restoreIdle()
	if d.spareBound() != 16 {
		t.Fatal("restored before the idle period", d.spareBound())
	}
	d.clock = d.clock.Add(time.Millisecond)
	s.restoreIdle()
	if d.spareBound() != 0 || d.text() != sixteenText {
		t.Fatalf("after deferred restore: bound %d decoded %q", d.spareBound(), d.text())
	}
	// A client slower than the quiet period is beyond what this mechanism can serve; say where the line is.
	if textQuietPeriod < 16*4*15*time.Millisecond || textRestoreIdle < textQuietPeriod {
		t.Fatal("quiet period must cover a 16-key backlog of a 60 ms/key client")
	}
}

func TestRebindWaitsForTheQuietPeriodInsideAndAcrossRequests(t *testing.T) {
	d := newFakeDisplay(minSpareKeycodes)
	d.clientCost = 50 * time.Millisecond // 16-key backlog of 0.8 s
	s := d.service()
	reply := s.handle(textRequest(sixteenText))
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	result := reply.Result.(map[string]any)
	if result["batches"] != 2 || result["waited_ms"].(int64) < 900 || result["waited_ms"].(int64) > 1000 || result["pool_keycodes"] != minSpareKeycodes {
		t.Fatal(result)
	}
	// A different set immediately afterwards must wait again rather than rebind hot keycodes.
	second := "\u0431\u0432\u0433\u0434\u0435\u0436\u0437\u0438"
	before := d.slept
	if reply = s.handle(textRequest(second)); reply.Error != nil || d.slept-before < 900*time.Millisecond {
		t.Fatal(reply.Error, d.slept-before)
	}
	// The same characters again need no mapping change and no wait.
	maps, before := d.maps, d.slept
	reply = s.handle(textRequest(second))
	if reply.Error != nil || d.maps != maps || d.slept-before > 100*time.Millisecond {
		t.Fatal(reply.Error, d.maps-maps, d.slept-before)
	}
	for _, binding := range fmtBindings(reply) {
		if !strings.Contains(binding, "reused:true") {
			t.Fatal("expected reuse", binding)
		}
	}
	if d.text() != sixteenText+second+second {
		t.Fatalf("slow client decoded %q", d.text())
	}
}

func fmtBindings(reply response) []string {
	var out []string
	for _, line := range strings.Split(strings.Trim(fmt.Sprintf("%+v", reply.Result.(map[string]any)["bindings"]), "[]"), "} {") {
		out = append(out, strings.ToLower(strings.ReplaceAll(line, " ", "")))
	}
	return out
}

// Hypothesis "the keycode is not really free": the server accepts the mapping
// change but does not apply it. The read-back refuses to type on such a keycode.
func TestBindingThatDoesNotReadBackIsNeverTyped(t *testing.T) {
	d := newFakeDisplay(16)
	d.ignoreMap = func(keycode byte) bool { return keycode < 240 }
	reply := d.service().handle(textRequest(sixteenText))
	if reply.Error == nil || reply.Error.Code != codeInjectFailed || !strings.Contains(reply.Error.Message, "reads back") || !strings.Contains(reply.Error.Message, "nothing was injected") || len(d.presses) != 0 {
		t.Fatalf("%+v presses %d", reply, len(d.presses))
	}
	// Five characters fit the keycodes that do apply.
	d = newFakeDisplay(16)
	d.ignoreMap = func(keycode byte) bool { return keycode < 240 }
	if reply = d.service().handle(textRequest(mixedText)); reply.Error != nil || d.text() != mixedText {
		t.Fatal(reply)
	}
	// A keycode somebody else rebinds between requests is dropped and never cleared by us.
	d = newFakeDisplay(16)
	s := d.service()
	if reply = s.handle(textRequest("ab")); reply.Error != nil {
		t.Fatal(reply)
	}
	d.rows[255-8] = [4]uint32{0x7a, 0x5a, 0, 0}
	d.history = append(d.history, fakeChange{d.clock, 255, 0x7a})
	if reply = s.handle(textRequest("ab")); reply.Error != nil || d.text() != "abab" {
		t.Fatal(reply, d.text())
	}
	if _, _, err := s.restoreAll(); err != nil || d.rows[255-8] != [4]uint32{0x7a, 0x5a, 0, 0} || d.spareBound() != 1 {
		t.Fatal("foreign binding touched", err, d.rows[255-8], d.spareBound())
	}
}

// An RPC reply must not restate the text: no bindings, no keysym or code point in any message.
func TestRPCRepliesNeverEchoTheText(t *testing.T) {
	secret := "\u4f60\u597d\u00e9Zq7"
	leaks := func(reply response) bool {
		line, _ := json.Marshal(reply)
		for _, r := range secret {
			for _, form := range []string{fmt.Sprintf("%d", textKeysym(r)), fmt.Sprintf("%x", textKeysym(r)), fmt.Sprintf("%04X", r), fmt.Sprintf("%d", r)} {
				if len(form) >= 3 && strings.Contains(strings.ToLower(string(line)), strings.ToLower(form)) {
					return true
				}
			}
		}
		return strings.Contains(string(line), "bindings") || strings.Contains(string(line), hex.EncodeToString([]byte(secret)))
	}
	d := newFakeDisplay(16)
	s := d.service()
	s.diagnostic = false
	reply := s.handle(textRequest(secret))
	if reply.Error != nil || d.text() != secret || leaks(reply) {
		t.Fatalf("%+v", reply)
	}
	result := reply.Result.(map[string]any)
	if result["batches"] != 1 || result["pool_keycodes"] != 16 || result["injected_scalars"] != 6 {
		t.Fatal(result)
	}
	d = newFakeDisplay(16)
	d.ignoreMap = func(byte) bool { return true }
	s = d.service()
	s.diagnostic = false
	if reply = s.handle(textRequest(secret)); reply.Error == nil || reply.Error.Code != codeInjectFailed || leaks(reply) {
		t.Fatalf("%+v", reply)
	}
	for _, failAt := range []int{1, 2, 3, 9, 10, 11, 14, 22} {
		d = newFakeDisplay(16)
		d.failPlan = []int{failAt}
		s = d.service()
		s.diagnostic = false
		if reply = s.handle(textRequest(secret)); reply.Error == nil || leaks(reply) {
			t.Fatalf("%d: %+v", failAt, reply)
		}
	}
	// A rejected character is named by position only.
	bad := s.handle(request{ID: "t", TextHex: hex.EncodeToString([]byte("pass\u009bword"))})
	if bad.Error == nil || bad.Error.Message != "character 4 is not allowed" {
		t.Fatalf("%+v", bad)
	}
	if status, _ := json.Marshal(s.status()); strings.Contains(string(status), "keysym") {
		t.Fatal(string(status))
	}
}

func TestTextUnsupportedIsExplicitAndInjectsNothing(t *testing.T) {
	saved := textInput
	defer func() { textInput = saved }()
	textInput = nil
	reply := handle(textRequest(mixedText), true)
	if reply.Error == nil || reply.Error.Code != codeUnsupported || !strings.Contains(reply.Error.Message, "--allow-text-input") {
		t.Fatal(reply)
	}
	info := handle(request{ID: "i", Method: "info"}, false).Result.(map[string]any)
	if info["capabilities"].(map[string]bool)["text_input"] || info["text_input"].(map[string]any)["enabled"] != false {
		t.Fatal(info)
	}

	for name, prepare := range map[string]func(*fakeDisplay){
		"no display":    func(d *fakeDisplay) { d.dialErr = fmt.Errorf("%w: X display socket not reachable", errXUnavailable) },
		"no XTEST":      func(d *fakeDisplay) { d.dialErr = fmt.Errorf("%w: XTEST extension absent", errXUnavailable) },
		"setup failure": func(d *fakeDisplay) { d.dialErr = errors.New("timeout") },
		"too few spares": func(d *fakeDisplay) {
			*d = *newFakeDisplay(minSpareKeycodes - 1)
		},
	} {
		d := newFakeDisplay(16)
		prepare(d)
		textInput = d.service()
		reply := handle(textRequest(mixedText), false)
		if reply.Error == nil || reply.Error.Code != codeUnsupported || len(d.presses) != 0 || d.maps != 0 {
			t.Fatalf("%s: %+v", name, reply)
		}
		info := handle(request{ID: "i", Method: "info"}, false).Result.(map[string]any)
		status := info["text_input"].(map[string]any)
		if info["capabilities"].(map[string]bool)["text_input"] || status["available"] != false || status["reason"] == "" || status["enabled"] != true {
			t.Fatalf("%s: info %v", name, status)
		}
	}

	d := newFakeDisplay(16)
	textInput = d.service()
	info = handle(request{ID: "i", Method: "info"}, false).Result.(map[string]any)
	status := info["text_input"].(map[string]any)
	if !info["capabilities"].(map[string]bool)["text_input"] || status["available"] != true || status["max_scalars_per_request"] != maxTextScalars || status["backend"] != textBackend || status["spare_keycodes"] != 16 || status["bound_keycodes"] != 0 || d.maps != 0 || len(d.presses) != 0 {
		t.Fatal(status)
	}
	if info["capabilities"].(map[string]bool)["desktop"] {
		t.Fatal("text input must not advertise a desktop agent")
	}
}

func TestTextNotReadyWithoutFocusOrWithModifiers(t *testing.T) {
	for name, prepare := range map[string]func(*fakeDisplay){
		"no focus": func(d *fakeDisplay) { d.focus = xFocusNone },
		"shift":    func(d *fakeDisplay) { d.mask = 0x01 },
		"capslock": func(d *fakeDisplay) { d.mask = 0x02 },
		"control":  func(d *fakeDisplay) { d.mask = 0x04 },
		"alt":      func(d *fakeDisplay) { d.mask = 0x08 },
		"super":    func(d *fakeDisplay) { d.mask = 0x40 },
	} {
		d := newFakeDisplay(16)
		prepare(d)
		reply := d.service().handle(textRequest(mixedText))
		if reply.Error == nil || reply.Error.Code != codeNotReady || len(d.presses) != 0 || d.maps != 0 {
			t.Fatalf("%s: %+v", name, reply)
		}
	}
	// NumLock and pointer buttons do not change the two bound levels.
	d := newFakeDisplay(16)
	d.mask = 0x10 | 0x100
	if reply := d.service().handle(textRequest(mixedText)); reply.Error != nil || d.text() != mixedText {
		t.Fatal(reply)
	}
}

func TestTextFailureNeverReportsSuccess(t *testing.T) {
	// Operation order on a connection for "abcdefgh" with at least eight free keycodes:
	// 1 State, 2 Keymap, 3-10 Map, 11 Sync, 12 Keymap (read-back), 13-28 Key pairs, 29 Sync.
	cases := []struct {
		name     string
		text     string
		failAt   int
		code     string
		typed    string
		contains string
	}{
		{"state", "abcdefgh", 1, codeInjectFailed, "", "nothing was injected"},
		{"keymap read", "abcdefgh", 2, codeInjectFailed, "", "nothing was injected"},
		{"first map", "abcdefgh", 3, codeInjectFailed, "", "nothing was injected"},
		{"map sync", "abcdefgh", 11, codeInjectFailed, "", "nothing was injected"},
		{"read-back", "abcdefgh", 12, codeInjectFailed, "", "read-back failed; nothing was injected"},
		{"first press", "abcdefgh", 13, codePartial, "", "injected 0 of 8 characters, 1 more unconfirmed"},
		{"second release", "abcdefgh", 16, codePartial, "ab", "injected 0 of 8 characters, 2 more unconfirmed"},
		{"final sync", "abcdefgh", 29, codePartial, "abcdefgh", "injected 0 of 8 characters, 8 more unconfirmed"},
		// Nine distinct characters on eight keycodes: the ninth needs a second batch (30 Map, 31 Sync, 32 Keymap, 33 press).
		{"second batch map", "abcdefghi", 30, codePartial, "abcdefgh", "injected 8 of 9 characters, 0 more unconfirmed"},
		{"second batch press", "abcdefghi", 33, codePartial, "abcdefgh", "injected 8 of 9 characters, 1 more unconfirmed"},
	}
	for _, c := range cases {
		d := newFakeDisplay(minSpareKeycodes)
		d.failPlan = []int{c.failAt}
		s := d.service()
		reply := s.handle(textRequest(c.text))
		if reply.Error == nil || reply.Result != nil || reply.Error.Code != c.code || !strings.Contains(reply.Error.Message, c.contains) {
			t.Fatalf("%s: %+v", c.name, reply.Error)
		}
		if d.text() != c.typed || strings.Contains(reply.Error.Message, "abcd") {
			t.Fatalf("%s: typed %q, message %q", c.name, d.text(), reply.Error.Message)
		}
		// A key left down is released at once on a second connection; bindings go through the deferred restore.
		if len(d.down) != 0 || strings.Contains(reply.Error.Message, "may still be held") {
			t.Fatalf("%s: keys down %v %q", c.name, d.down, reply.Error.Message)
		}
		if _, _, err := s.restoreAll(); err != nil || d.spareBound() != 0 {
			t.Fatalf("%s: restore %v, %d left bound", c.name, err, d.spareBound())
		}
	}
	// The release of a held key fails as well: the reply says so.
	d := newFakeDisplay(minSpareKeycodes)
	d.failPlan = []int{16, 1}
	reply := d.service().handle(textRequest("abcdefgh"))
	if reply.Error == nil || reply.Error.Code != codePartial || !strings.Contains(reply.Error.Message, "may still be held") || len(d.down) != 1 {
		t.Fatalf("%+v", reply.Error)
	}
}

func TestDeferredRestoreReportsFailureAndRetries(t *testing.T) {
	d := newFakeDisplay(16)
	s := d.service()
	if reply := s.handle(textRequest("ab")); reply.Error != nil {
		t.Fatal(reply)
	}
	d.clock = d.clock.Add(textRestoreIdle)
	d.failPlan = []int{0, 2} // the restore connection fails at its first Map
	armed := len(d.armed)
	s.restoreIdle()
	status := s.status()
	if d.spareBound() != 2 || status["last_restore_error"] != "X keymap restore request failed" || status["bound_keycodes"] != 2 || len(d.armed) != armed+1 {
		t.Fatal(d.spareBound(), status, d.armed)
	}
	s.restoreIdle()
	if status = s.status(); d.spareBound() != 0 || status["last_restore_error"] != nil || status["bound_keycodes"] != 0 || status["spare_keycodes"] != 16 {
		t.Fatal(d.spareBound(), status)
	}
	// Only bindings idle for the whole period are restored; the rest keep the timer armed.
	if reply := s.handle(textRequest("a")); reply.Error != nil {
		t.Fatal(reply)
	}
	d.clock = d.clock.Add(textRestoreIdle - time.Second)
	if reply := s.handle(textRequest("b")); reply.Error != nil {
		t.Fatal(reply)
	}
	d.clock = d.clock.Add(time.Second)
	armed = len(d.armed)
	s.restoreIdle()
	if d.spareBound() != 1 || len(d.armed) != armed+1 {
		t.Fatal(d.spareBound(), d.armed)
	}
}

func TestTextSingleFlight(t *testing.T) {
	d := newFakeDisplay(16)
	s := d.service()
	s.mu.Lock()
	reply := s.handle(textRequest("a"))
	status := s.status()
	s.restoreIdle()
	s.mu.Unlock()
	if reply.Error == nil || reply.Error.Code != codeBusy || d.dials != 0 || status["available"] != false || status["reason"] != "busy" {
		t.Fatal(reply, status)
	}
}

func TestSerialV2TextInputAnswersContentErrorsAndFencesExtraFields(t *testing.T) {
	saved := textInput
	defer func() { textInput = saved }()
	d := newFakeDisplay(8)
	textInput = d.service()
	m := testManager(t)
	c, done, base := serialExecPair(t, m)
	send := func(seq uint64, r request) response {
		v2Send(t, c, base, seq, r)
		return *v2Read(t, c).Response
	}
	info := send(1, request{ID: "info", Method: "info"}).Result.(map[string]any)
	if info["capabilities"].(map[string]any)["text_input"] != true || info["capabilities"].(map[string]any)["desktop"] != false {
		t.Fatal(info)
	}
	reply := send(2, textRequest(mixedText))
	if reply.Error != nil || d.text() != mixedText || reply.Result.(map[string]any)["injected_bytes"] != float64(len(mixedText)) {
		t.Fatal(reply)
	}
	// Content errors are replies, not transport fences.
	if reply = send(3, request{ID: "bad", Method: "text_input", TextHex: "00"}); reply.Error == nil || reply.Error.Code != codeInvalid {
		t.Fatal(reply)
	}
	// A request with extra fields fences the incarnation like any malformed v2 request.
	v2Send(t, c, base, 4, request{ID: "extra", Method: "text_input", TextHex: "61", JobID: strings.Repeat("b", 32)})
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("text input with extra fields accepted")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("text input with extra fields did not fence")
	}
	c.Close()
}

func TestObserverSerialRejectsTextFields(t *testing.T) {
	saved := textInput
	defer func() { textInput = saved }()
	d := newFakeDisplay(8)
	textInput = d.service()
	b, a := serialPair(t)
	base := serialHost(t, b)
	var err error
	r := textRequest("a")
	if err = serialWrite(b, serialEnvelope{Version: 1, Type: "request", GuestNonce: base.GuestNonce, HostNonce: base.HostNonce, Sequence: 1, Request: &r}); err != nil {
		t.Fatal(err)
	}
	select {
	case err = <-a:
		if err == nil || len(d.presses) != 0 {
			t.Fatal("observer transport injected text", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("observer did not reject text input")
	}
}

// The diagnostic one-shot and the flag rules, through the real main().
func TestTextCLIOneShotAndFlagRules(t *testing.T) {
	if raw := os.Getenv("VEYNODE_TEXT_CLI_ARGS"); raw != "" {
		flag.CommandLine = flag.NewFlagSet("text-cli", flag.ExitOnError)
		os.Args = append([]string{"text-cli"}, strings.Split(raw, " ")...)
		main()
		return
	}
	run := func(args string) (string, int) {
		cmd := exec.Command(os.Args[0], "-test.run=^TestTextCLIOneShotAndFlagRules$")
		cmd.Env = append(os.Environ(), "VEYNODE_TEXT_CLI_ARGS="+args, "GORACE=atexit_sleep_ms=0")
		out, err := cmd.CombinedOutput()
		code := 0
		var exit *exec.ExitError
		if errors.As(err, &exit) {
			code = exit.ExitCode()
		} else if err != nil {
			t.Fatal(err)
		}
		return string(out), code
	}
	// Display :97 does not exist on a test host: explicit unsupported, exit 3, nothing listened on.
	out, code := run("--allow-text-input --text-input-display=:97 --text-input-once-hex=e4bda0")
	if code != 3 || !strings.Contains(out, `TEXT_INPUT_STATUS {"allowed_controls":["U+000A","U+0009"],"available":false,"backend":"x11-xtest-keymap","enabled":true`) || !strings.Contains(out, `TEXT_INPUT_ONCE {"id":"once","error":{"code":"unsupported","message":"X display socket not reachable"}}`) || !strings.Contains(out, `TEXT_INPUT_RESTORE {"error":"","remaining":0,"restored":0,"waited_ms":0}`) {
		t.Fatal(code, out)
	}
	out, code = run("--text-input-once-hex=e4bda0")
	if code != 3 || !strings.Contains(out, `"enabled":false`) || !strings.Contains(out, `"code":"unsupported","message":"text input disabled at guest service launch (--allow-text-input)"`) {
		t.Fatal(code, out)
	}
	out, code = run("--allow-text-input --text-input-display=:97 --text-input-once-hex=1b")
	if code != 3 || !strings.Contains(out, `"code":"invalid_request"`) {
		t.Fatal(code, out)
	}
	for args, want := range map[string]string{
		"--allow-text-input --text-input-display=host:0 --text-input-once-hex=61": "text input display rejected",
		"--text-input-display=:1":                         "text input settings require --allow-text-input",
		"--text-input-xauthority=/tmp/x":                  "text input settings require --allow-text-input",
		"--virtio-serial --allow-text-input":              "observer-only; privilege options rejected",
		"--virtio-serial --text-input-once-hex=61":        "observer-only; privilege options rejected",
		"--virtio-serial --text-input-display=:1":         "observer-only; privilege options rejected",
		"--file-worker --allow-text-input --files-root=/": "text input is not available to the files worker",
	} {
		if out, code = run(args); code != 1 || !strings.Contains(out, want) {
			t.Fatalf("%s: %d %s", args, code, out)
		}
	}
}
