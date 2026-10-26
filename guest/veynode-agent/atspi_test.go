package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
)

func TestDBusCodecRoundTrip(t *testing.T) {
	args := []any{"org.a11y.Status", int32(-7), uint32(9), true, 2.5, []any{[]any{":1.2", "/a/b"}}, dbusVariant{"as", []any{"x", "yz"}}, []any{[]any{"k", dbusVariant{"u", uint32(1)}}}}
	sig := "siubda(so)va{sv}"
	raw, err := encodeDBusCall(7, "dest", "/obj", "iface.I", "Member", sig, args)
	if err != nil {
		t.Fatal(err)
	}
	m, err := readDBusMessage(bufio.NewReader(strings.NewReader(string(raw))))
	if err != nil {
		t.Fatal(err)
	}
	if m.kind != dbusMethodCall || m.serial != 7 || m.dest != "dest" || m.path != "/obj" || m.iface != "iface.I" || m.member != "Member" || m.sig != sig {
		t.Fatalf("header %+v", m)
	}
	values, err := m.values()
	if err != nil {
		t.Fatal(err)
	}
	got, _ := json.Marshal(values)
	want, _ := json.Marshal(args)
	if string(got) != string(want) {
		t.Fatalf("values %s, want %s", got, want)
	}
	if _, err := readDBusMessage(bufio.NewReader(strings.NewReader(string(raw[:len(raw)-3])))); err == nil {
		t.Fatal("truncated message accepted")
	}
	for _, bad := range []string{"(s", "a", "z", "{sv"} {
		if _, _, err := sigNext(bad); err == nil {
			t.Fatalf("signature %q accepted", bad)
		}
	}
	if got := dbusSocket("unix:path=/run/user/0/bus%2c1,guid=x;tcp:host=a;unix:abstract=/tmp/dbus-1", "/newroot"); !slices.Equal(got, []string{"/newroot/run/user/0/bus,1", "@/tmp/dbus-1"}) {
		t.Fatalf("sockets %q", got)
	}
}

// fakeBus is one D-Bus server playing the session bus (org.a11y.Bus) and the
// accessibility bus with a small application.
type fakeBus struct {
	mu      sync.Mutex
	address string
	enabled bool
	actions []string
	objects map[string]fakeAccessible
}

type fakeAccessible struct {
	role, name string
	states     []string
	ifaces     []string
	children   []string
	box        [4]int32
	actions    []string
	text       string
}

func encodeDBusReply(serial, replySerial uint32, kind byte, sig string, values []any) []byte {
	body := &dbusEncoder{}
	if err := body.all(sig, values); err != nil {
		panic(err)
	}
	fields := []any{[]any{byte(5), dbusVariant{"u", replySerial}}}
	if kind == dbusError {
		fields = append(fields, []any{byte(4), dbusVariant{"s", "org.a11y.Fake.Error"}})
	}
	if sig != "" {
		fields = append(fields, []any{byte(8), dbusVariant{"g", sig}})
	}
	head := &dbusEncoder{b: []byte{'l', kind, 0, 1}}
	head.u32(uint32(len(body.b)))
	head.u32(serial)
	if err := head.one("a(yv)", fields); err != nil {
		panic(err)
	}
	head.align(8)
	return append(head.b, body.b...)
}

func stateWords(states []string) []any {
	words := []uint32{0, 0}
	for _, state := range states {
		i := slices.Index(atspiStates, state)
		words[i/32] |= 1 << (i % 32)
	}
	return []any{words[0], words[1]}
}

func (f *fakeBus) answer(m *dbusMessage) (string, []any, error) {
	args, _ := m.values()
	f.mu.Lock()
	defer f.mu.Unlock()
	switch m.member {
	case "Hello":
		return "s", []any{":1.99"}, nil
	case "GetAddress":
		return "s", []any{f.address}, nil
	case "GetConnectionUnixProcessID":
		return "u", []any{uint32(4242)}, nil
	}
	if m.path == "/org/a11y/bus" && m.member == "Get" {
		return "v", []any{dbusVariant{"b", f.enabled}}, nil
	}
	if m.path == "/org/a11y/bus" && m.member == "Set" {
		f.enabled = true
		return "", nil, nil
	}
	obj, ok := f.objects[m.dest+m.path]
	if !ok {
		return "", nil, errors.New("unknown object")
	}
	refs := func(paths []string) []any {
		var out []any
		for _, path := range paths {
			bus, p, _ := strings.Cut(path, "|")
			out = append(out, []any{bus, p})
		}
		return out
	}
	switch m.member {
	case "GetRoleName":
		return "s", []any{obj.role}, nil
	case "GetState":
		return "au", []any{stateWords(obj.states)}, nil
	case "GetInterfaces":
		var ifaces []any
		for _, iface := range append([]string{"Accessible"}, obj.ifaces...) {
			ifaces = append(ifaces, "org.a11y.atspi."+iface)
		}
		return "as", []any{ifaces}, nil
	case "GetChildren":
		return "a(so)", []any{refs(obj.children)}, nil
	case "Get":
		switch args[1] {
		case "Name":
			return "v", []any{dbusVariant{"s", obj.name}}, nil
		case "ChildCount":
			return "v", []any{dbusVariant{"i", int32(len(obj.children))}}, nil
		case "CharacterCount":
			return "v", []any{dbusVariant{"i", int32(len(obj.text))}}, nil
		}
	case "GetExtents":
		return "(iiii)", []any{[]any{obj.box[0], obj.box[1], obj.box[2], obj.box[3]}}, nil
	case "GetActions":
		var actions []any
		for _, name := range obj.actions {
			actions = append(actions, []any{name, "", ""})
		}
		return "a(sss)", []any{actions}, nil
	case "GetText":
		return "s", []any{obj.text}, nil
	case "DoAction":
		f.actions = append(f.actions, obj.actions[args[0].(int32)])
		return "b", []any{true}, nil
	case "SetTextContents":
		f.actions = append(f.actions, "text:"+args[0].(string))
		return "b", []any{true}, nil
	}
	return "", nil, errors.New("unsupported " + m.member)
}

func (f *fakeBus) serve(conn net.Conn) {
	defer conn.Close()
	r := bufio.NewReader(conn)
	for {
		line, err := r.ReadString('\n')
		if err != nil {
			return
		}
		if strings.HasPrefix(strings.TrimPrefix(line, "\x00"), "AUTH EXTERNAL ") {
			conn.Write([]byte("OK 0123456789abcdef0123456789abcdef\r\n"))
		}
		if line == "BEGIN\r\n" {
			break
		}
	}
	var serial uint32
	for {
		m, err := readDBusMessage(r)
		if err != nil {
			return
		}
		serial++
		sig, values, err := f.answer(m)
		if err != nil {
			conn.Write(encodeDBusReply(serial, m.serial, dbusError, "s", []any{err.Error()}))
			continue
		}
		conn.Write(encodeDBusReply(serial, m.serial, dbusMethodReturn, sig, values))
	}
}

func newFakeAccessibility(t *testing.T) (*fakeBus, a11yJob) {
	t.Helper()
	root, err := os.MkdirTemp("", "va") // unix socket paths are short
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(root) })
	listener, err := net.Listen("unix", filepath.Join(root, "bus"))
	if err != nil {
		t.Skip("unix socket unavailable:", err)
	}
	t.Cleanup(func() { listener.Close() })
	f := &fakeBus{address: "unix:path=/bus", objects: map[string]fakeAccessible{
		"org.a11y.atspi.Registry/org/a11y/atspi/accessible/root": {role: "desktop frame", children: []string{":1.5|/root"}},
		":1.5/root":   {role: "application", name: "demo", children: []string{":1.5|/frame"}},
		":1.5/frame":  {role: "frame", name: "Window", states: []string{"showing", "sensitive", "active"}, ifaces: []string{"Component"}, box: [4]int32{10, 20, 300, 200}, children: []string{":1.5|/ok", ":1.5|/hidden", ":1.5|/entry", ":1.5|/term", ":1.5|/gone"}},
		":1.5/ok":     {role: "push button", name: "OK", states: []string{"showing", "sensitive", "focusable"}, ifaces: []string{"Component", "Action"}, box: [4]int32{20, 180, 60, 30}, actions: []string{"Press", "Click"}},
		":1.5/hidden": {role: "label", name: "Secret OK", states: []string{"sensitive"}},
		":1.5/entry":  {role: "text", states: []string{"showing", "editable"}, ifaces: []string{"Component", "Text", "EditableText"}, box: [4]int32{-5000, -5000, 10, 10}, text: "draft"},
		":1.5/term":   {role: "terminal", name: "Terminal", states: []string{"showing", "sensitive"}, ifaces: []string{"Text"}, text: "a\n$ ls   \nb  \n\n\n"},
	}}
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go f.serve(conn)
		}
	}()
	return f, a11yJob{Method: "accessibility_tree", Root: root, Env: []string{"DBUS_SESSION_BUS_ADDRESS=unix:path=/bus"}}
}

func TestAccessibilityTreeReadsAtspi(t *testing.T) {
	f, job := newFakeAccessibility(t)
	result, err := runA11y(job)
	if err != nil {
		t.Fatal(err)
	}
	if result["accessibility_enabled_now"] != true || !f.enabled {
		t.Fatalf("accessibility not turned on: %v", result)
	}
	nodes := result["nodes"].([]*a11yNode)
	var roles []string
	for _, n := range nodes {
		roles = append(roles, n.Role)
	}
	if !slices.Equal(roles, []string{"application", "frame", "push button", "text", "terminal"}) {
		t.Fatalf("roles %v", roles)
	}
	app, button, entry, term := nodes[0], nodes[2], nodes[3], nodes[4]
	if app.PID != 4242 || app.Name != "demo" || app.Ref != ":1.5|/root" {
		t.Fatalf("app %+v", app)
	}
	if button.Name != "OK" || button.Depth != 2 || !slices.Equal(button.Actions, []string{"Press", "Click"}) || button.Box["center_x"] != 50 || button.Box["center_y"] != 195 || len(button.States) != 0 {
		t.Fatalf("button %+v", button)
	}
	if entry.Box != nil || entry.Text != "draft" || !slices.Equal(entry.States, []string{"editable", "disabled"}) {
		t.Fatalf("entry %+v", entry)
	}
	if term.Text != "a\n$ ls\nb" {
		t.Fatalf("terminal %+v", term)
	}
	// Hidden elements appear on request; search returns matches with context.
	job.Args = a11yArgs{Name: "ok", IncludeHidden: true}
	found, err := runA11y(job)
	if err != nil {
		t.Fatal(err)
	}
	matches := found["nodes"].([]*a11yNode)
	if len(matches) != 2 || matches[0].Context != "demo > Window" || matches[1].Name != "Secret OK" {
		t.Fatalf("find %+v", matches)
	}
	job.Args = a11yArgs{Role: "push button", App: "nothing"}
	if found, _ := runA11y(job); found["count"] != 0 {
		t.Fatalf("app filter: %v", found)
	}
}

func TestTerminalTail(t *testing.T) {
	if got := terminalTail("a\n$ ls   \nb  \n\n   \n"); got != "a\n$ ls\nb" {
		t.Fatalf("tail %q", got)
	}
	long := strings.Repeat("line\n", 100)
	if got := strings.Count(terminalTail(long), "\n"); got != 39 {
		t.Fatalf("lines %d", got)
	}
}

func TestAccessibilityActions(t *testing.T) {
	f, job := newFakeAccessibility(t)
	job.Method = "accessibility_action"
	run := func(args a11yArgs) (map[string]any, error) {
		job.Args = args
		return runA11y(job)
	}
	result, err := run(a11yArgs{Ref: ":1.5|/ok", Action: "do"})
	if err != nil || result["action_name"] != "Click" || result["done"] != true || result["element_after"].(*a11yNode).Name != "OK" {
		t.Fatalf("default action: %v %v", result, err)
	}
	if _, err := run(a11yArgs{Ref: ":1.5|/ok", Action: "do", ActionName: "press"}); err != nil {
		t.Fatal(err)
	}
	text := "hello"
	if _, err := run(a11yArgs{Ref: ":1.5|/entry", Action: "set_text", Text: &text}); err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(f.actions, []string{"Click", "Press", "text:hello"}) {
		t.Fatalf("actions %v", f.actions)
	}
	for _, bad := range []a11yArgs{
		{Ref: ":1.5|/ok", Action: "do", ActionName: "explode"},
		{Ref: ":1.5|/ok", Action: "set_text", Text: &text},
		{Ref: ":1.5|/gone", Action: "do"},
		{Ref: "no-ref", Action: "do"},
		{Ref: ":1.5|/ok", Action: "dance"},
	} {
		_, err := run(bad)
		var coded *observeError
		if !errors.As(err, &coded) || coded.code != "invalid_request" {
			t.Fatalf("%+v: %v", bad, err)
		}
	}
}
