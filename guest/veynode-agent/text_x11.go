// Minimal X11 core + XTEST client for Unicode text injection. Pure Go, local
// Unix sockets only, no TCP. It speaks exactly the six requests the injector
// needs and treats every unexpected server answer as a failure.
package main

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const (
	xOpQueryPointer          = 38
	xOpGetInputFocus         = 43
	xOpQueryExtension        = 98
	xOpChangeKeyboardMapping = 100
	xOpGetKeyboardMapping    = 101
	xtestFakeInput           = 2
	xKeyPress                = 2
	xKeyRelease              = 3
	xMaxReplyUnits           = 262144 // 1 MiB of additional reply data
	xMaxSkippedEvents        = 4096
	xMaxAuthorityFile        = 65536
)

var xle = binary.LittleEndian

// errXUnavailable marks conditions under which the Guest has no usable text
// service at all (no display, refused setup, no XTEST). Nothing was injected.
var errXUnavailable = errors.New("x11 text service unavailable")

type xProtocolError struct {
	code, major byte
	minor, seq  uint16
}

func (e xProtocolError) Error() string {
	return fmt.Sprintf("X error code=%d major=%d minor=%d sequence=%d", e.code, e.major, e.minor, e.seq)
}

type xtestConn struct {
	c                      net.Conn
	seq                    uint16
	minKeycode, maxKeycode byte
	root                   uint32
	xtest                  byte
}

func xPad(n int) int { return (4 - n%4) % 4 }

// parseDisplay accepts only local ":N" or ":N.S" names.
func parseDisplay(display string) (int, error) {
	if !strings.HasPrefix(display, ":") {
		return 0, fmt.Errorf("only local :N displays are supported")
	}
	number := strings.TrimPrefix(display, ":")
	if dot := strings.IndexByte(number, '.'); dot >= 0 {
		screen, err := strconv.ParseUint(number[dot+1:], 10, 8)
		if err != nil || strconv.FormatUint(screen, 10) != number[dot+1:] {
			return 0, fmt.Errorf("invalid display screen")
		}
		number = number[:dot]
	}
	n, err := strconv.ParseUint(number, 10, 8)
	if err != nil || n > 99 || strconv.FormatUint(n, 10) != number {
		return 0, fmt.Errorf("invalid display number")
	}
	return int(n), nil
}

// parseXauthority returns the MIT-MAGIC-COOKIE-1 for a local display number.
// Entry layout: family(2) then four length-prefixed (2-byte big-endian) fields:
// address, display number, authorization name, authorization data.
func parseXauthority(data []byte, display int) ([]byte, bool) {
	want := strconv.Itoa(display)
	for len(data) > 0 {
		if len(data) < 2 {
			return nil, false
		}
		family := binary.BigEndian.Uint16(data)
		data = data[2:]
		var fields [4][]byte
		for i := range fields {
			if len(data) < 2 {
				return nil, false
			}
			n := int(binary.BigEndian.Uint16(data))
			data = data[2:]
			if len(data) < n {
				return nil, false
			}
			fields[i], data = data[:n], data[n:]
		}
		// 256 = FamilyLocal, 65535 = FamilyWild. An empty number matches any display.
		if (family == 256 || family == 65535) && (len(fields[1]) == 0 || string(fields[1]) == want) && string(fields[2]) == "MIT-MAGIC-COOKIE-1" && len(fields[3]) == 16 {
			return append([]byte(nil), fields[3]...), true
		}
	}
	return nil, false
}

func readXauthority(path string, display int) ([]byte, error) {
	// Open a FIFO without waiting for a writer before we can inspect its type.
	// Regular files retain ordinary read semantics; symlinked authority files
	// remain usable when their opened target is a regular file.
	f, err := os.OpenFile(path, os.O_RDONLY|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil || !info.Mode().IsRegular() {
		return nil, fmt.Errorf("authority must be a regular file")
	}
	data, err := io.ReadAll(io.LimitReader(f, xMaxAuthorityFile+1))
	if err != nil || len(data) > xMaxAuthorityFile {
		return nil, fmt.Errorf("authority file unreadable or too large")
	}
	cookie, ok := parseXauthority(data, display)
	if !ok {
		return nil, fmt.Errorf("no MIT-MAGIC-COOKIE-1 entry for display")
	}
	return cookie, nil
}

// dialX11 connects, completes setup and requires XTEST. Failures here are all
// "service unavailable": no request that changes server state has been sent.
func dialX11(socket string, cookie []byte, deadline time.Time) (*xtestConn, error) {
	c, err := net.DialTimeout("unix", socket, time.Until(deadline))
	if err != nil {
		return nil, fmt.Errorf("%w: X display socket not reachable", errXUnavailable)
	}
	x, err := newXtestConn(c, cookie, deadline)
	if err != nil {
		c.Close()
		return nil, err
	}
	return x, nil
}

func newXtestConn(c net.Conn, cookie []byte, deadline time.Time) (*xtestConn, error) {
	if err := c.SetDeadline(deadline); err != nil {
		return nil, fmt.Errorf("%w: deadline unsupported", errXUnavailable)
	}
	name := ""
	if len(cookie) != 0 {
		name = "MIT-MAGIC-COOKIE-1"
	}
	hello := make([]byte, 12, 12+len(name)+xPad(len(name))+len(cookie)+xPad(len(cookie)))
	hello[0] = 'l'
	xle.PutUint16(hello[2:], 11)
	xle.PutUint16(hello[6:], uint16(len(name)))
	xle.PutUint16(hello[8:], uint16(len(cookie)))
	hello = append(hello, name...)
	hello = append(hello, make([]byte, xPad(len(name)))...)
	hello = append(hello, cookie...)
	hello = append(hello, make([]byte, xPad(len(cookie)))...)
	if _, err := c.Write(hello); err != nil {
		return nil, fmt.Errorf("%w: X setup write failed", errXUnavailable)
	}
	var head [8]byte
	if _, err := io.ReadFull(c, head[:]); err != nil {
		return nil, fmt.Errorf("%w: X setup not answered", errXUnavailable)
	}
	extra := make([]byte, 4*int(xle.Uint16(head[6:])))
	if _, err := io.ReadFull(c, extra); err != nil {
		return nil, fmt.Errorf("%w: X setup truncated", errXUnavailable)
	}
	if head[0] != 1 {
		return nil, fmt.Errorf("%w: X setup refused (authorization)", errXUnavailable)
	}
	if xle.Uint16(head[2:]) != 11 || len(extra) < 32 {
		return nil, fmt.Errorf("%w: unexpected X protocol", errXUnavailable)
	}
	vendor, screens, formats := int(xle.Uint16(extra[16:])), int(extra[20]), int(extra[21])
	x := &xtestConn{c: c, minKeycode: extra[26], maxKeycode: extra[27]}
	rootAt := 32 + vendor + xPad(vendor) + 8*formats
	if screens < 1 || len(extra) < rootAt+4 || x.minKeycode < 8 || x.maxKeycode < x.minKeycode {
		return nil, fmt.Errorf("%w: unexpected X setup contents", errXUnavailable)
	}
	x.root = xle.Uint32(extra[rootAt:])
	const extension = "XTEST"
	query := make([]byte, 8, 16)
	query[0] = xOpQueryExtension
	xle.PutUint16(query[2:], uint16(2+(len(extension)+xPad(len(extension)))/4))
	xle.PutUint16(query[4:], uint16(len(extension)))
	query = append(query, extension...)
	query = append(query, make([]byte, xPad(len(extension)))...)
	reply, err := x.roundTrip(query)
	if err != nil {
		return nil, fmt.Errorf("%w: extension query failed", errXUnavailable)
	}
	if reply[8] != 1 || reply[9] < 128 {
		return nil, fmt.Errorf("%w: XTEST extension absent", errXUnavailable)
	}
	x.xtest = reply[9]
	return x, nil
}

func (x *xtestConn) send(request []byte) (uint16, error) {
	if len(request)%4 != 0 || int(xle.Uint16(request[2:]))*4 != len(request) {
		return 0, fmt.Errorf("internal X request length")
	}
	if _, err := x.c.Write(request); err != nil {
		return 0, err
	}
	x.seq++
	return x.seq, nil
}

// reply waits for the reply to sequence seq. Events are skipped (MappingNotify
// is sent to every client). Any error packet fails the wait, including errors
// for earlier no-reply requests, so ChangeKeyboardMapping/FakeInput failures
// surface at the next Sync.
func (x *xtestConn) reply(seq uint16) ([]byte, error) {
	for skipped := 0; skipped < xMaxSkippedEvents; skipped++ {
		packet := make([]byte, 32)
		if _, err := io.ReadFull(x.c, packet); err != nil {
			return nil, err
		}
		switch packet[0] {
		case 0:
			return nil, xProtocolError{code: packet[1], seq: xle.Uint16(packet[2:]), minor: xle.Uint16(packet[8:]), major: packet[10]}
		case 1:
			units := xle.Uint32(packet[4:])
			if units > xMaxReplyUnits {
				return nil, fmt.Errorf("X reply too large")
			}
			packet = append(packet, make([]byte, 4*units)...)
			if _, err := io.ReadFull(x.c, packet[32:]); err != nil {
				return nil, err
			}
			if xle.Uint16(packet[2:]) != seq {
				return nil, fmt.Errorf("X reply sequence mismatch")
			}
			return packet, nil
		default:
			if packet[0]&0x7f == 35 { // GenericEvent carries a length.
				units := xle.Uint32(packet[4:])
				if units > xMaxReplyUnits {
					return nil, fmt.Errorf("X event too large")
				}
				if _, err := io.CopyN(io.Discard, x.c, int64(4*units)); err != nil {
					return nil, err
				}
			}
		}
	}
	return nil, fmt.Errorf("X event flood while waiting for reply")
}

func (x *xtestConn) roundTrip(request []byte) ([]byte, error) {
	seq, err := x.send(request)
	if err != nil {
		return nil, err
	}
	return x.reply(seq)
}

// State returns the focus window (0 = None, 1 = PointerRoot) and the current
// modifier/button mask.
func (x *xtestConn) State() (uint32, uint16, error) {
	focus, err := x.roundTrip([]byte{xOpGetInputFocus, 0, 1, 0})
	if err != nil {
		return 0, 0, err
	}
	query := []byte{xOpQueryPointer, 0, 2, 0, 0, 0, 0, 0}
	xle.PutUint32(query[4:], x.root)
	pointer, err := x.roundTrip(query)
	if err != nil {
		return 0, 0, err
	}
	return xle.Uint32(focus[8:]), xle.Uint16(pointer[24:]), nil
}

// Keymap reads the whole core keyboard mapping: rows[i] holds every column of
// keycode first+i. It is the read-back used to verify bindings before typing.
func (x *xtestConn) Keymap() (byte, [][]uint32, error) {
	count := int(x.maxKeycode) - int(x.minKeycode) + 1
	reply, err := x.roundTrip([]byte{xOpGetKeyboardMapping, 0, 2, 0, x.minKeycode, byte(count), 0, 0})
	if err != nil {
		return 0, nil, err
	}
	per := int(reply[1])
	if per < 1 || per > 64 || int(xle.Uint32(reply[4:])) != per*count || len(reply) != 32+4*per*count {
		return 0, nil, fmt.Errorf("X keyboard mapping reply malformed")
	}
	rows := make([][]uint32, count)
	for index := range rows {
		rows[index] = make([]uint32, per)
		for column := range rows[index] {
			rows[index][column] = xle.Uint32(reply[32+4*(index*per+column):])
		}
	}
	return x.minKeycode, rows, nil
}

// Map binds one keycode to keysym in both shift levels (0 restores NoSymbol).
// No reply: a server error is reported by the next Sync.
func (x *xtestConn) Map(keycode byte, keysym uint32) error {
	request := []byte{xOpChangeKeyboardMapping, 1, 4, 0, keycode, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}
	xle.PutUint32(request[8:], keysym)
	xle.PutUint32(request[12:], keysym)
	_, err := x.send(request)
	return err
}

// Key sends one XTEST FakeInput key event with no server-side delay.
func (x *xtestConn) Key(keycode byte, down bool) error {
	request := make([]byte, 36)
	request[0], request[1], request[2] = x.xtest, xtestFakeInput, 9
	request[4] = xKeyRelease
	if down {
		request[4] = xKeyPress
	}
	request[5] = keycode
	_, err := x.send(request)
	return err
}

func (x *xtestConn) Sync() error {
	_, err := x.roundTrip([]byte{xOpGetInputFocus, 0, 1, 0})
	return err
}
func (x *xtestConn) Close() error { return x.c.Close() }
