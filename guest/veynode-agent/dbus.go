package main

// Minimal D-Bus client for the accessibility tools (atspi.go): EXTERNAL
// authentication over unix sockets, the wire format for the types AT-SPI
// uses, method calls and pipelined batches of calls. No library is needed in
// the guest image.

import (
	"bufio"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"math"
	"net"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const (
	dbusMethodCall   = 1
	dbusMethodReturn = 2
	dbusError        = 3
	dbusMaxMessage   = 8 << 20
)

type dbusVariant struct {
	Sig   string
	Value any
}

// sigNext splits the first complete type off a signature.
func sigNext(sig string) (string, string, error) {
	if sig == "" {
		return "", "", errors.New("empty signature")
	}
	switch sig[0] {
	case 'a':
		elem, rest, err := sigNext(sig[1:])
		return "a" + elem, rest, err
	case '(', '{':
		closer := map[byte]byte{'(': ')', '{': '}'}[sig[0]]
		depth := 0
		for i := 0; i < len(sig); i++ {
			switch sig[i] {
			case '(', '{':
				depth++
			case ')', '}':
				depth--
				if depth == 0 {
					if sig[i] != closer {
						return "", "", errors.New("unbalanced signature")
					}
					return sig[:i+1], sig[i+1:], nil
				}
			}
		}
		return "", "", errors.New("unbalanced signature")
	case 'y', 'b', 'n', 'q', 'i', 'u', 'x', 't', 'd', 's', 'o', 'g', 'v', 'h':
		return sig[:1], sig[1:], nil
	}
	return "", "", fmt.Errorf("unsupported signature %q", sig)
}

func dbusAlignment(t byte) int {
	switch t {
	case 'n', 'q':
		return 2
	case 'b', 'i', 'u', 'h', 's', 'o', 'a':
		return 4
	case 'x', 't', 'd', '(', '{':
		return 8
	}
	return 1
}

type dbusEncoder struct{ b []byte }

func (e *dbusEncoder) align(n int) {
	for len(e.b)%n != 0 {
		e.b = append(e.b, 0)
	}
}

func (e *dbusEncoder) u32(v uint32) {
	e.align(4)
	e.b = binary.LittleEndian.AppendUint32(e.b, v)
}

func (e *dbusEncoder) all(sig string, values []any) error {
	for i := 0; sig != ""; i++ {
		one, rest, err := sigNext(sig)
		if err != nil {
			return err
		}
		if i >= len(values) {
			return errors.New("too few values for signature")
		}
		if err := e.one(one, values[i]); err != nil {
			return err
		}
		sig = rest
	}
	return nil
}

func (e *dbusEncoder) one(t string, v any) error {
	bad := fmt.Errorf("value %T does not match D-Bus type %s", v, t)
	switch t[0] {
	case 'y':
		b, ok := v.(byte)
		if !ok {
			return bad
		}
		e.b = append(e.b, b)
	case 'b':
		b, ok := v.(bool)
		if !ok {
			return bad
		}
		e.u32(map[bool]uint32{false: 0, true: 1}[b])
	case 'i':
		n, ok := v.(int32)
		if !ok {
			return bad
		}
		e.u32(uint32(n))
	case 'u':
		n, ok := v.(uint32)
		if !ok {
			return bad
		}
		e.u32(n)
	case 'd':
		f, ok := v.(float64)
		if !ok {
			return bad
		}
		e.align(8)
		e.b = binary.LittleEndian.AppendUint64(e.b, math.Float64bits(f))
	case 's', 'o':
		s, ok := v.(string)
		if !ok {
			return bad
		}
		e.u32(uint32(len(s)))
		e.b = append(append(e.b, s...), 0)
	case 'g':
		s, ok := v.(string)
		if !ok || len(s) > 255 {
			return bad
		}
		e.b = append(append(append(e.b, byte(len(s))), s...), 0)
	case 'v':
		variant, ok := v.(dbusVariant)
		if !ok {
			return bad
		}
		if err := e.one("g", variant.Sig); err != nil {
			return err
		}
		return e.one(variant.Sig, variant.Value)
	case 'a':
		items, ok := v.([]any)
		if !ok {
			return bad
		}
		e.u32(0)
		at := len(e.b) - 4
		e.align(dbusAlignment(t[1]))
		start := len(e.b)
		for _, item := range items {
			if err := e.one(t[1:], item); err != nil {
				return err
			}
		}
		binary.LittleEndian.PutUint32(e.b[at:], uint32(len(e.b)-start))
	case '(', '{':
		fields, ok := v.([]any)
		if !ok {
			return bad
		}
		e.align(8)
		return e.all(t[1:len(t)-1], fields)
	default:
		return bad
	}
	return nil
}

type dbusDecoder struct {
	b     []byte
	pos   int
	order binary.ByteOrder
}

var errDBusShort = errors.New("truncated D-Bus message")

func (d *dbusDecoder) align(n int) error {
	for d.pos%n != 0 {
		if d.pos >= len(d.b) {
			return errDBusShort
		}
		d.pos++
	}
	return nil
}

func (d *dbusDecoder) take(n int) ([]byte, error) {
	if n < 0 || d.pos+n > len(d.b) {
		return nil, errDBusShort
	}
	out := d.b[d.pos : d.pos+n]
	d.pos += n
	return out, nil
}

func (d *dbusDecoder) fixed(n int) (uint64, error) {
	if err := d.align(n); err != nil {
		return 0, err
	}
	raw, err := d.take(n)
	if err != nil {
		return 0, err
	}
	switch n {
	case 1:
		return uint64(raw[0]), nil
	case 2:
		return uint64(d.order.Uint16(raw)), nil
	case 4:
		return uint64(d.order.Uint32(raw)), nil
	}
	return d.order.Uint64(raw), nil
}

func (d *dbusDecoder) all(sig string) ([]any, error) {
	var out []any
	for sig != "" {
		one, rest, err := sigNext(sig)
		if err != nil {
			return nil, err
		}
		value, err := d.one(one, 0)
		if err != nil {
			return nil, err
		}
		out = append(out, value)
		sig = rest
	}
	return out, nil
}

func (d *dbusDecoder) one(t string, depth int) (any, error) {
	if depth > 32 {
		return nil, errors.New("D-Bus value nested too deeply")
	}
	switch t[0] {
	case 'y':
		v, err := d.fixed(1)
		return byte(v), err
	case 'b':
		v, err := d.fixed(4)
		return v != 0, err
	case 'n':
		v, err := d.fixed(2)
		return int16(v), err
	case 'q':
		v, err := d.fixed(2)
		return uint16(v), err
	case 'i':
		v, err := d.fixed(4)
		return int32(v), err
	case 'u', 'h':
		v, err := d.fixed(4)
		return uint32(v), err
	case 'x':
		v, err := d.fixed(8)
		return int64(v), err
	case 't':
		v, err := d.fixed(8)
		return v, err
	case 'd':
		v, err := d.fixed(8)
		return math.Float64frombits(v), err
	case 's', 'o':
		n, err := d.fixed(4)
		if err != nil {
			return nil, err
		}
		raw, err := d.take(int(n) + 1)
		if err != nil {
			return nil, err
		}
		return string(raw[:n]), nil
	case 'g':
		n, err := d.fixed(1)
		if err != nil {
			return nil, err
		}
		raw, err := d.take(int(n) + 1)
		if err != nil {
			return nil, err
		}
		return string(raw[:n]), nil
	case 'v':
		sig, err := d.one("g", depth+1)
		if err != nil {
			return nil, err
		}
		one, rest, err := sigNext(sig.(string))
		if err != nil || rest != "" {
			return nil, errors.New("invalid variant signature")
		}
		value, err := d.one(one, depth+1)
		return dbusVariant{one, value}, err
	case 'a':
		n, err := d.fixed(4)
		if err != nil {
			return nil, err
		}
		if err := d.align(dbusAlignment(t[1])); err != nil {
			return nil, err
		}
		end := d.pos + int(n)
		if n > dbusMaxMessage || end > len(d.b) {
			return nil, errDBusShort
		}
		items := []any{}
		for d.pos < end {
			item, err := d.one(t[1:], depth+1)
			if err != nil {
				return nil, err
			}
			items = append(items, item)
		}
		return items, nil
	case '(', '{':
		if err := d.align(8); err != nil {
			return nil, err
		}
		var fields []any
		inner := t[1 : len(t)-1]
		for inner != "" {
			one, rest, err := sigNext(inner)
			if err != nil {
				return nil, err
			}
			value, err := d.one(one, depth+1)
			if err != nil {
				return nil, err
			}
			fields = append(fields, value)
			inner = rest
		}
		return fields, nil
	}
	return nil, fmt.Errorf("unsupported D-Bus type %s", t)
}

type dbusMessage struct {
	kind        byte
	serial      uint32
	replySerial uint32
	path        string
	iface       string
	member      string
	errName     string
	dest        string
	sender      string
	sig         string
	body        []byte
	order       binary.ByteOrder
}

// values decodes the body by its signature.
func (m *dbusMessage) values() ([]any, error) {
	return (&dbusDecoder{b: m.body, order: m.order}).all(m.sig)
}

func encodeDBusCall(serial uint32, dest, path, iface, member, sig string, args []any) ([]byte, error) {
	body := &dbusEncoder{}
	if err := body.all(sig, args); err != nil {
		return nil, err
	}
	fields := []any{
		[]any{byte(1), dbusVariant{"o", path}},
		[]any{byte(3), dbusVariant{"s", member}},
	}
	if iface != "" {
		fields = append(fields, []any{byte(2), dbusVariant{"s", iface}})
	}
	if dest != "" {
		fields = append(fields, []any{byte(6), dbusVariant{"s", dest}})
	}
	if sig != "" {
		fields = append(fields, []any{byte(8), dbusVariant{"g", sig}})
	}
	head := &dbusEncoder{b: []byte{'l', dbusMethodCall, 0, 1}}
	head.u32(uint32(len(body.b)))
	head.u32(serial)
	if err := head.one("a(yv)", fields); err != nil {
		return nil, err
	}
	head.align(8)
	return append(head.b, body.b...), nil
}

func readDBusMessage(r io.Reader) (*dbusMessage, error) {
	fixed := make([]byte, 16)
	if _, err := io.ReadFull(r, fixed); err != nil {
		return nil, err
	}
	var order binary.ByteOrder
	switch fixed[0] {
	case 'l':
		order = binary.LittleEndian
	case 'B':
		order = binary.BigEndian
	default:
		return nil, errors.New("invalid D-Bus byte order")
	}
	bodyLen, fieldsLen := order.Uint32(fixed[4:]), order.Uint32(fixed[12:])
	if bodyLen > dbusMaxMessage || fieldsLen > dbusMaxMessage {
		return nil, errors.New("D-Bus message too large")
	}
	headerLen := (16 + int(fieldsLen) + 7) &^ 7
	raw := make([]byte, headerLen+int(bodyLen))
	copy(raw, fixed)
	if _, err := io.ReadFull(r, raw[16:]); err != nil {
		return nil, err
	}
	m := &dbusMessage{kind: fixed[1], serial: order.Uint32(fixed[8:]), body: raw[headerLen:], order: order}
	d := &dbusDecoder{b: raw[:16+fieldsLen], pos: 12, order: order}
	fields, err := d.one("a(yv)", 0)
	if err != nil {
		return nil, err
	}
	for _, field := range fields.([]any) {
		pair := field.([]any)
		value := pair[1].(dbusVariant).Value
		switch pair[0].(byte) {
		case 1:
			m.path, _ = value.(string)
		case 2:
			m.iface, _ = value.(string)
		case 3:
			m.member, _ = value.(string)
		case 4:
			m.errName, _ = value.(string)
		case 5:
			m.replySerial, _ = value.(uint32)
		case 6:
			m.dest, _ = value.(string)
		case 7:
			m.sender, _ = value.(string)
		case 8:
			m.sig, _ = value.(string)
		}
	}
	return m, nil
}

type dbusConn struct {
	conn     net.Conn
	r        *bufio.Reader
	serial   uint32
	name     string
	deadline time.Time
}

type dbusCall struct {
	dest, path, iface, member, sig string
	args                           []any
}

type dbusReply struct {
	values []any
	err    error
}

// dbusSocket turns the unix addresses of a bus address into dialable paths
// (abstract names are prefixed with @), resolved inside root.
func dbusSocket(address, root string) []string {
	var out []string
	for _, entry := range strings.Split(address, ";") {
		transport, params, ok := strings.Cut(entry, ":")
		if !ok || transport != "unix" {
			continue
		}
		for _, param := range strings.Split(params, ",") {
			key, value, _ := strings.Cut(param, "=")
			value = dbusUnescape(value)
			switch key {
			case "path":
				out = append(out, filepath.Join(root, value))
			case "abstract":
				out = append(out, "@"+value)
			}
		}
	}
	return out
}

func dbusUnescape(value string) string {
	if !strings.Contains(value, "%") {
		return value
	}
	var b strings.Builder
	for i := 0; i < len(value); i++ {
		if value[i] == '%' && i+2 < len(value) {
			if c, err := strconv.ParseUint(value[i+1:i+3], 16, 8); err == nil {
				b.WriteByte(byte(c))
				i += 2
				continue
			}
		}
		b.WriteByte(value[i])
	}
	return b.String()
}

// dialDBus connects and authenticates as uid, then registers with Hello.
func dialDBus(address, root string, uid int, deadline time.Time) (*dbusConn, error) {
	var lastErr error = errors.New("no unix address in bus address")
	for _, socket := range dbusSocket(address, root) {
		conn, err := net.DialTimeout("unix", socket, time.Until(deadline))
		if err != nil {
			lastErr = err
			continue
		}
		_ = conn.SetDeadline(deadline)
		c := &dbusConn{conn: conn, r: bufio.NewReader(conn), deadline: deadline}
		if err := c.auth(uid); err != nil {
			conn.Close()
			lastErr = err
			continue
		}
		hello, err := c.call(dbusCall{"org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", "", nil})
		if err != nil {
			conn.Close()
			lastErr = err
			continue
		}
		if len(hello) == 1 {
			c.name, _ = hello[0].(string)
		}
		return c, nil
	}
	return nil, lastErr
}

func (c *dbusConn) auth(uid int) error {
	if _, err := fmt.Fprintf(c.conn, "\x00AUTH EXTERNAL %s\r\n", hex.EncodeToString([]byte(strconv.Itoa(uid)))); err != nil {
		return err
	}
	line, err := c.r.ReadString('\n')
	if err != nil {
		return err
	}
	if !strings.HasPrefix(line, "OK ") {
		return fmt.Errorf("D-Bus authentication refused: %s", strings.TrimSpace(line))
	}
	_, err = io.WriteString(c.conn, "BEGIN\r\n")
	return err
}

func (c *dbusConn) close() { c.conn.Close() }

func (c *dbusConn) call(call dbusCall) ([]any, error) {
	reply := c.batch([]dbusCall{call})[0]
	return reply.values, reply.err
}

// batch sends every call before reading any reply, so a level of the
// accessibility tree costs one round trip; replies match by serial.
// Calls still unanswered at the connection deadline fail.
func (c *dbusConn) batch(calls []dbusCall) []dbusReply {
	out := make([]dbusReply, len(calls))
	pending := map[uint32]int{}
	var buf []byte
	for i, call := range calls {
		c.serial++
		msg, err := encodeDBusCall(c.serial, call.dest, call.path, call.iface, call.member, call.sig, call.args)
		if err != nil {
			out[i].err = err
			continue
		}
		pending[c.serial] = i
		buf = append(buf, msg...)
	}
	if _, err := c.conn.Write(buf); err != nil {
		for _, i := range pending {
			out[i].err = err
		}
		return out
	}
	for len(pending) > 0 {
		m, err := readDBusMessage(c.r)
		if err != nil {
			for _, i := range pending {
				out[i].err = err
			}
			return out
		}
		i, ok := pending[m.replySerial]
		if !ok || (m.kind != dbusMethodReturn && m.kind != dbusError) {
			continue // signals and stray replies
		}
		delete(pending, m.replySerial)
		values, err := m.values()
		if m.kind == dbusError {
			message := m.errName
			if len(values) > 0 {
				if text, ok := values[0].(string); ok {
					message += ": " + text
				}
			}
			err = errors.New(message)
		}
		out[i] = dbusReply{values, err}
	}
	return out
}
