// Unicode text input for the focused Guest application.
//
// Mechanism ("x11-xtest-keymap"): bind spare keycodes of the running X server
// to the keysyms of the requested characters, read the mapping back, and send
// XTEST key press/release pairs. This needs only the X server the desktop
// already runs; no xdotool, IBus, uinput or clipboard is involved.
//
// A client resolves a key event against the server mapping at the time it
// processes the event, not when the event was sent. A binding must therefore
// outlive the slowest client's backlog: a keycode is never rebound or restored
// within textQuietPeriod of its last key event, bindings are kept and reused
// across requests, and they are restored only after textRestoreIdle without use.
//
// Disabled unless the service is launched with --allow-text-input. A Guest
// without a reachable display, XTEST or enough spare keycodes answers
// `unsupported`; characters are never dropped or substituted silently.
package main

import (
	"encoding/hex"
	"errors"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

const (
	textBackend        = "x11-xtest-keymap"
	maxTextScalars     = 16 // per request; the host chunks longer text
	maxTextBytes       = 4 * maxTextScalars
	minSpareKeycodes   = 8 // 16 distinct characters then need at most one in-request rebind wait
	maxPoolKeycodes    = 64
	textKeyDelay       = 2 * time.Millisecond
	textMapDelay       = 10 * time.Millisecond
	textQuietPeriod    = time.Second     // no rebind or restore of a keycode sooner after its last key event
	textRestoreIdle    = 3 * time.Second // bindings unused this long are restored in the background
	textRequestBudget  = 3 * time.Second // one request, including rebind waits; the host allows 4 s
	textWaitMargin     = 300 * time.Millisecond
	textRestoreBudget  = 300 * time.Millisecond
	textProbeBudget    = 300 * time.Millisecond
	textRestoreRetries = 5
	textBlockingMask   = 0x00ef // Shift, Lock, Control, Mod1, Mod3, Mod4, Mod5; NumLock (Mod2) is tolerated
	keysymReturn       = 0xff0d
	keysymTab          = 0xff09
	keysymUnicodeFlag  = 0x01000000
	xFocusNone         = 0
	codeUnsupported    = "unsupported"
	codeInvalid        = "invalid_request"
	codeBusy           = "busy"
	codeNotReady       = "not_ready"
	codeInjectFailed   = "injection_failed"
	codePartial        = "partial_delivery"
	textDisabledReason = "text input disabled at guest service launch (--allow-text-input)"
)

// xKeyboard is the injection seam. The production implementation is *xtestConn.
type xKeyboard interface {
	State() (focus uint32, mask uint16, err error)
	Keymap() (first byte, rows [][]uint32, err error)
	Map(keycode byte, keysym uint32) error
	Key(keycode byte, down bool) error
	Sync() error
	Close() error
}

type textBinding struct {
	keysym uint32
	prior  uint32 // what the keycode held before a rebind the server has not yet shown; 0 if none
	used   time.Time
}

type textService struct {
	mu       sync.Mutex
	dial     func(budget time.Duration) (xKeyboard, error)
	sleep    func(time.Duration)
	now      func() time.Time
	schedule func(time.Duration) // arms the deferred restore; nil uses a real timer
	// diagnostic adds per-keycode bindings (which restate the text) to a result.
	// Only the local --text-input-once-hex mode sets it; RPC replies never echo text.
	diagnostic bool
	// Guarded by mu: temporary bindings this service created and has not restored.
	bound        map[byte]*textBinding
	restoreError string
	restoreFails int
	timer        *time.Timer
}

// textInput is nil unless --allow-text-input was given.
var textInput *textService

func newX11TextService(display, authority string) (*textService, error) {
	number, err := parseDisplay(display)
	if err != nil {
		return nil, err
	}
	socket := fmt.Sprintf("/tmp/.X11-unix/X%d", number)
	return &textService{sleep: time.Sleep, now: time.Now, dial: func(budget time.Duration) (xKeyboard, error) {
		var cookie []byte
		if authority != "" {
			var err error
			if cookie, err = readXauthority(authority, number); err != nil {
				return nil, fmt.Errorf("%w: X authority unavailable", errXUnavailable)
			}
		}
		return dialText(socket, cookie, budget)
	}}, nil
}

// allowedTextRune is the complete character policy. LF and TAB are the only
// control characters; they become the Return and Tab keys.
func allowedTextRune(r rune) bool {
	switch {
	case r == '\n' || r == '\t':
		return true
	case r < 0x20 || (r >= 0x7f && r <= 0x9f):
		return false
	case r >= 0xd800 && r <= 0xdfff:
		return false
	case r >= 0xfdd0 && r <= 0xfdef, r&0xfffe == 0xfffe, r > 0x10ffff:
		return false
	}
	return true
}

func textKeysym(r rune) uint32 {
	switch {
	case r == '\n':
		return keysymReturn
	case r == '\t':
		return keysymTab
	case (r >= 0x20 && r <= 0x7e) || (r >= 0xa0 && r <= 0xff):
		return uint32(r) // Latin-1 keysyms equal their code points
	}
	return keysymUnicodeFlag | uint32(r)
}

// decodeText validates the wire form: lowercase hex of 1..64 UTF-8 bytes that
// decode to 1..16 allowed Unicode scalar values. Hex keeps the bytes exact;
// encoding/json would replace invalid sequences with U+FFFD.
func decodeText(textHex string) ([]rune, int, error) {
	if textHex == "" || len(textHex)%2 != 0 || len(textHex) > 2*maxTextBytes || textHex != strings.ToLower(textHex) {
		return nil, 0, fmt.Errorf("text_hex must be lowercase hex of 1..%d bytes", maxTextBytes)
	}
	raw, err := hex.DecodeString(textHex)
	if err != nil {
		return nil, 0, fmt.Errorf("text_hex is not hexadecimal")
	}
	if !utf8.Valid(raw) {
		return nil, 0, fmt.Errorf("text is not valid UTF-8")
	}
	runes := []rune(string(raw))
	if len(runes) > maxTextScalars {
		return nil, 0, fmt.Errorf("text exceeds %d Unicode scalar values", maxTextScalars)
	}
	for index, r := range runes {
		if !allowedTextRune(r) { // the message names the position, never the character
			return nil, 0, fmt.Errorf("character %d is not allowed", index)
		}
	}
	return runes, len(raw), nil
}

type textFailure struct {
	code, message string
}

func (f *textFailure) Error() string { return f.code + ": " + f.message }

func unavailable(err error) *textFailure {
	if errors.Is(err, errXUnavailable) {
		return &textFailure{codeUnsupported, strings.TrimPrefix(err.Error(), errXUnavailable.Error()+": ")}
	}
	return &textFailure{codeUnsupported, "X display did not complete setup"}
}

// dialText gives connection setup the short probe budget, so a hung display
// fails fast, and the established connection the caller's whole budget.
func dialText(socket string, cookie []byte, budget time.Duration) (xKeyboard, error) {
	setup := budget
	if setup > textProbeBudget {
		setup = textProbeBudget
	}
	started := time.Now()
	x, err := dialX11(socket, cookie, started.Add(setup))
	if err != nil {
		return nil, err
	}
	if err = x.c.SetDeadline(started.Add(budget)); err != nil {
		x.Close()
		return nil, fmt.Errorf("%w: deadline unsupported", errXUnavailable)
	}
	return x, nil
}

func rowEmpty(row []uint32) bool {
	for _, sym := range row {
		if sym != 0 {
			return false
		}
	}
	return true
}

// rowBound accepts what a server reports for a keycode bound to keysym in both
// shift levels: column 0 is the keysym and every other column is the keysym or
// empty (XKB servers repeat a one-group key in the second group's columns).
func rowBound(row []uint32, keysym uint32) bool {
	if len(row) == 0 || row[0] != keysym || keysym == 0 {
		return false
	}
	for _, sym := range row {
		if sym != 0 && sym != keysym {
			return false
		}
	}
	return true
}

func rowAt(first byte, rows [][]uint32, keycode byte) []uint32 {
	if keycode < first || int(keycode-first) >= len(rows) {
		return nil
	}
	return rows[keycode-first]
}

func readback(row []uint32) []uint32 {
	if len(row) > 4 {
		row = row[:4]
	}
	return append([]uint32{}, row...)
}

// reconcile makes the binding table agree with the server: a rebind that never
// took effect falls back to the binding it was to replace, and a keycode that
// shows neither is no longer ours and is never touched again. It then lists
// free keycodes, highest first, so that kept bindings plus free keycodes are at
// most maxPoolKeycodes.
func (s *textService) reconcile(first byte, rows [][]uint32) []byte {
	for keycode, binding := range s.bound {
		row := rowAt(first, rows, keycode)
		switch {
		case rowBound(row, binding.keysym):
			binding.prior = 0
		case binding.prior != 0 && rowBound(row, binding.prior):
			binding.keysym, binding.prior = binding.prior, 0
		default:
			delete(s.bound, keycode)
		}
	}
	var free []byte
	for index := len(rows) - 1; index >= 0 && len(s.bound)+len(free) < maxPoolKeycodes; index-- {
		keycode := first + byte(index)
		if _, ours := s.bound[keycode]; !ours && rowEmpty(rows[index]) {
			free = append(free, keycode)
		}
	}
	return free
}

// status probes the live display. It injects nothing and changes no mapping.
func (s *textService) status() map[string]any {
	result := map[string]any{"enabled": true, "backend": textBackend, "available": false, "max_scalars_per_request": maxTextScalars, "max_bytes_per_request": maxTextBytes, "allowed_controls": []string{"U+000A", "U+0009"}, "guest_delivery_confirmed": false}
	if !s.mu.TryLock() {
		result["reason"] = "busy"
		return result
	}
	defer s.mu.Unlock()
	if s.restoreError != "" {
		result["last_restore_error"] = s.restoreError
	}
	x, err := s.dial(textProbeBudget)
	if err != nil {
		result["reason"] = unavailable(err).message
		return result
	}
	defer x.Close()
	first, rows, err := x.Keymap()
	if err != nil {
		result["reason"] = "X keyboard mapping unreadable"
		return result
	}
	free := s.reconcile(first, rows)
	result["spare_keycodes"] = len(free)
	result["bound_keycodes"] = len(s.bound)
	if len(free)+len(s.bound) < minSpareKeycodes {
		result["reason"] = fmt.Sprintf("fewer than %d spare keycodes", minSpareKeycodes)
	} else {
		result["available"] = true
	}
	return result
}

func textInfo() (bool, map[string]any) {
	if textInput == nil {
		return false, map[string]any{"enabled": false, "available": false, "reason": textDisabledReason}
	}
	status := textInput.status()
	return status["available"] == true, status
}

func handleTextInput(req request) response {
	if textInput == nil {
		return failure(req.ID, codeUnsupported, textDisabledReason)
	}
	return textInput.handle(req)
}

func (s *textService) handle(req request) response {
	runes, size, err := decodeText(req.TextHex)
	if err != nil {
		return failure(req.ID, codeInvalid, err.Error())
	}
	if !s.mu.TryLock() {
		return failure(req.ID, codeBusy, "another text input request is in progress")
	}
	defer s.mu.Unlock()
	if s.bound == nil {
		s.bound = map[byte]*textBinding{}
	}
	detail, failed := s.inject(runes)
	if len(s.bound) != 0 {
		s.arm(textRestoreIdle)
	}
	if failed != nil {
		return failure(req.ID, failed.code, failed.message)
	}
	result := map[string]any{"injected_scalars": len(runes), "injected_bytes": size, "backend": textBackend, "keymap_restore": "deferred", "guest_delivery_confirmed": false}
	for key, value := range detail {
		if key != "bindings" || s.diagnostic {
			result[key] = value
		}
	}
	return response{ID: req.ID, Result: result}
}

// inject delivers every rune or reports exactly how far it got. `confirmed`
// counts runes whose key events the server acknowledged through a Sync, sent
// only after the server's own mapping was read back and matched.
func (s *textService) inject(runes []rune) (map[string]any, *textFailure) {
	deadline := s.now().Add(textRequestBudget)
	x, err := s.dial(textRequestBudget)
	if err != nil {
		return nil, unavailable(err)
	}
	defer func() { x.Close() }()
	focus, mask, err := x.State()
	if err != nil {
		return nil, &textFailure{codeInjectFailed, "X state query failed; nothing was injected"}
	}
	if focus == xFocusNone {
		return nil, &textFailure{codeNotReady, "no window has keyboard focus; nothing was injected"}
	}
	if mask&textBlockingMask != 0 {
		return nil, &textFailure{codeNotReady, fmt.Sprintf("modifier or CapsLock active (mask 0x%02x); nothing was injected", mask&textBlockingMask)}
	}
	first, rows, err := x.Keymap()
	if err != nil {
		return nil, &textFailure{codeInjectFailed, "X keyboard mapping unreadable; nothing was injected"}
	}
	free := s.reconcile(first, rows)
	pool := len(free) + len(s.bound)
	if pool < minSpareKeycodes {
		return nil, &textFailure{codeUnsupported, fmt.Sprintf("fewer than %d spare keycodes; nothing was injected", minSpareKeycodes)}
	}
	bySym := map[uint32]byte{}
	for keycode, binding := range s.bound {
		bySym[binding.keysym] = keycode
	}
	type record struct {
		Keycode  byte     `json:"keycode"`
		Keysym   uint32   `json:"keysym"`
		Readback []uint32 `json:"readback"`
		Reused   bool     `json:"reused"`
	}
	var records []record
	recorded := map[byte]bool{}
	held, holding := byte(0), false
	confirmed, uncertain, batches := 0, 0, 0
	var waited time.Duration
	run := func() *textFailure {
		for confirmed < len(runes) {
			used := map[byte]bool{}
			var fresh []byte
			var cooling time.Time
			end := confirmed
			for end < len(runes) {
				keysym := textKeysym(runes[end])
				keycode, ok := bySym[keysym]
				if !ok {
					prior := uint32(0)
					if len(free) != 0 {
						keycode, free = free[0], free[1:]
					} else {
						// Least recently used binding that this batch does not need.
						found := false
						for candidate, binding := range s.bound {
							if !used[candidate] && (!found || binding.used.Before(s.bound[keycode].used) || (binding.used.Equal(s.bound[keycode].used) && candidate > keycode)) {
								keycode, found = candidate, true
							}
						}
						if !found {
							break
						}
						if ready := s.bound[keycode].used.Add(textQuietPeriod); s.now().Before(ready) {
							cooling = ready
							break
						}
						prior = s.bound[keycode].keysym
						delete(bySym, prior)
					}
					s.bound[keycode] = &textBinding{keysym: keysym, prior: prior, used: s.now()}
					bySym[keysym] = keycode
					fresh = append(fresh, keycode)
				}
				used[keycode] = true
				end++
			}
			if end == confirmed {
				// Every keycode is still inside its quiet period: wait, never rebind early.
				wait := cooling.Sub(s.now())
				if cooling.IsZero() || s.now().Add(wait+textWaitMargin).After(deadline) {
					return &textFailure{codeBusy, "temporary keycodes are still in their quiet period"}
				}
				s.sleep(wait)
				waited += wait
				continue
			}
			for _, keycode := range fresh {
				if err := x.Map(keycode, s.bound[keycode].keysym); err != nil {
					return &textFailure{codeInjectFailed, "X keyboard mapping change failed"}
				}
			}
			if err := x.Sync(); err != nil {
				return &textFailure{codeInjectFailed, "X server rejected the keyboard mapping change"}
			}
			// Read the server's mapping back: no key is sent for a binding the server does not show.
			if first, rows, err = x.Keymap(); err != nil {
				return &textFailure{codeInjectFailed, "X keyboard mapping read-back failed"}
			}
			for keycode := range used {
				row := rowAt(first, rows, keycode)
				if !recorded[keycode] && len(records) < maxTextScalars {
					reused := true
					for _, other := range fresh {
						reused = reused && other != keycode
					}
					records = append(records, record{keycode, s.bound[keycode].keysym, readback(row), reused})
					recorded[keycode] = true
				}
				if !rowBound(row, s.bound[keycode].keysym) {
					// The keysym is a character of the text: name the keycode only.
					message := fmt.Sprintf("keycode %d reads back a different mapping after binding", keycode)
					if s.diagnostic {
						message += fmt.Sprintf(" (%#x, wanted %#x)", readback(row), s.bound[keycode].keysym)
					}
					return &textFailure{codeInjectFailed, message}
				}
			}
			s.sleep(textMapDelay)
			for _, r := range runes[confirmed:end] {
				keycode := bySym[textKeysym(r)]
				uncertain++
				held, holding = keycode, true
				s.bound[keycode].used = s.now()
				if err := x.Key(keycode, true); err != nil {
					return &textFailure{codeInjectFailed, "X key press failed"}
				}
				s.sleep(textKeyDelay)
				if err := x.Key(keycode, false); err != nil {
					return &textFailure{codeInjectFailed, "X key release failed"}
				}
				holding = false
				s.sleep(textKeyDelay)
			}
			if err := x.Sync(); err != nil {
				return &textFailure{codeInjectFailed, "X server did not acknowledge the key events"}
			}
			stamp := s.now()
			for keycode := range used {
				s.bound[keycode].used, s.bound[keycode].prior = stamp, 0
			}
			confirmed, uncertain = end, 0
			batches++
		}
		return nil
	}
	failed := run()
	released := ""
	if holding {
		// A press without its release: release now, on a fresh connection if needed.
		err := x.Key(held, false)
		if err == nil {
			err = x.Sync()
		}
		if err != nil {
			err = errors.New("no connection")
			if again, dialErr := s.dial(textRestoreBudget); dialErr == nil {
				if err = again.Key(held, false); err == nil {
					err = again.Sync()
				}
				again.Close()
			}
		}
		if err != nil {
			released = fmt.Sprintf("; keycode %d may still be held", held)
		}
	}
	sort.Slice(records, func(a, b int) bool { return records[a].Keycode > records[b].Keycode })
	switch {
	case failed == nil:
		return map[string]any{"bindings": records, "batches": batches, "waited_ms": waited.Milliseconds(), "pool_keycodes": pool}, nil
	case confirmed == 0 && uncertain == 0:
		return nil, &textFailure{failed.code, failed.message + "; nothing was injected"}
	}
	return nil, &textFailure{codePartial, fmt.Sprintf("injected %d of %d characters, %d more unconfirmed: %s%s", confirmed, len(runes), uncertain, failed.message, released)}
}

func (s *textService) arm(after time.Duration) {
	if s.schedule != nil {
		s.schedule(after)
		return
	}
	if s.timer != nil {
		s.timer.Stop()
	}
	s.timer = time.AfterFunc(after, s.restoreIdle)
}

// restoreIdle is the deferred restore: it returns keycodes unused for
// textRestoreIdle to NoSymbol and re-arms while any binding remains.
func (s *textService) restoreIdle() {
	if !s.mu.TryLock() {
		// A request is running; it re-arms when it ends. Retry without touching guarded state.
		if s.schedule == nil {
			time.AfterFunc(time.Second, s.restoreIdle)
		}
		return
	}
	defer s.mu.Unlock()
	_, remaining, err := s.restoreLocked(false)
	if err != nil {
		s.restoreError = err.Error()
		if s.restoreFails++; s.restoreFails >= textRestoreRetries {
			return // reported through info; the next request re-arms
		}
	} else {
		s.restoreError, s.restoreFails = "", 0
	}
	if remaining != 0 {
		s.arm(time.Second)
	}
}

// restoreAll restores every binding regardless of age. Callers that still have
// a client reading must wait textQuietPeriod first.
func (s *textService) restoreAll() (int, int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.restoreLocked(true)
}

func (s *textService) restoreLocked(all bool) (restored, remaining int, err error) {
	if len(s.bound) == 0 {
		return 0, 0, nil
	}
	x, err := s.dial(textRestoreBudget)
	if err != nil {
		return 0, len(s.bound), errors.New("X display unreachable for keymap restore")
	}
	defer x.Close()
	first, rows, err := x.Keymap()
	if err != nil {
		return 0, len(s.bound), errors.New("X keyboard mapping unreadable for keymap restore")
	}
	// Only a row that still shows our binding is ours to clear; reconcile forgets the others.
	s.reconcile(first, rows)
	now := s.now()
	var done []byte
	for keycode, binding := range s.bound {
		if !all && now.Sub(binding.used) < textRestoreIdle {
			remaining++
			continue
		}
		if err = x.Map(keycode, 0); err != nil {
			return 0, len(s.bound), errors.New("X keymap restore request failed")
		}
		restored++
		done = append(done, keycode)
	}
	if err = x.Sync(); err != nil {
		return 0, len(s.bound), errors.New("X server did not acknowledge the keymap restore")
	}
	for _, keycode := range done {
		delete(s.bound, keycode)
	}
	return restored, remaining, nil
}

func (s *textService) boundCount() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.bound)
}
