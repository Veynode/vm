package main

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"testing"
)

// fakeX is a little-endian X server with just the requests the desktop tools
// send: atoms, properties, geometry, pointer, focus and SendEvent.
type fakeX struct {
	mu       sync.Mutex
	atoms    map[string]uint32
	props    map[uint32]map[string]fakeProp
	geometry map[uint32][4]int
	sent     [][]uint32 // [window, type atom, data...]
	refuse   bool
}

type fakeProp struct {
	kind   string
	format int
	data   []byte
}

func cardinalProp(values ...uint32) fakeProp {
	data := make([]byte, 4*len(values))
	for i, v := range values {
		binary.LittleEndian.PutUint32(data[4*i:], v)
	}
	return fakeProp{"CARDINAL", 32, data}
}

func textProp(kind string, parts ...string) fakeProp {
	var data []byte
	for _, part := range parts {
		data = append(append(data, part...), 0)
	}
	return fakeProp{kind, 8, data}
}

func (f *fakeX) atom(name string) uint32 {
	f.mu.Lock()
	defer f.mu.Unlock()
	if atom, ok := f.atoms[name]; ok {
		return atom
	}
	atom := uint32(len(f.atoms) + 100)
	f.atoms[name] = atom
	return atom
}

func (f *fakeX) atomProp(names ...string) fakeProp {
	values := make([]uint32, len(names))
	for i, name := range names {
		values[i] = f.atom(name)
	}
	p := cardinalProp(values...)
	p.kind = "ATOM"
	return p
}

func (f *fakeX) serve(conn net.Conn) {
	defer conn.Close()
	head := make([]byte, 12)
	if _, err := io.ReadFull(conn, head); err != nil {
		return
	}
	n := pad4(int(binary.LittleEndian.Uint16(head[6:]))) + pad4(int(binary.LittleEndian.Uint16(head[8:])))
	if _, err := io.CopyN(io.Discard, conn, int64(n)); err != nil {
		return
	}
	if f.refuse {
		reason := "No protocol specified"
		reply := make([]byte, 8+pad4(len(reason)))
		reply[1] = byte(len(reason))
		binary.LittleEndian.PutUint16(reply[6:], uint16(pad4(len(reason))/4))
		copy(reply[8:], reason)
		conn.Write(reply)
		return
	}
	body := make([]byte, 32+4+40)
	binary.LittleEndian.PutUint16(body[16:], 4) // vendor length
	body[20] = 1                                // screens
	copy(body[32:], "fake")
	binary.LittleEndian.PutUint32(body[36:], 1) // root
	binary.LittleEndian.PutUint16(body[36+20:], 1280)
	binary.LittleEndian.PutUint16(body[36+22:], 800)
	setup := make([]byte, 8)
	setup[0] = 1
	binary.LittleEndian.PutUint16(setup[6:], uint16(len(body)/4))
	conn.Write(append(setup, body...))
	var seq uint16
	for {
		header := make([]byte, 4)
		if _, err := io.ReadFull(conn, header); err != nil {
			return
		}
		data := make([]byte, int(binary.LittleEndian.Uint16(header[2:]))*4-4)
		if _, err := io.ReadFull(conn, data); err != nil {
			return
		}
		seq++
		reply := make([]byte, 32)
		reply[0] = 1
		binary.LittleEndian.PutUint16(reply[2:], seq)
		u32 := func(at int) uint32 { return binary.LittleEndian.Uint32(data[at:]) }
		switch header[0] {
		case x11InternAtom:
			name := string(data[4 : 4+binary.LittleEndian.Uint16(data)])
			binary.LittleEndian.PutUint32(reply[8:], f.atom(name))
		case x11GetProperty:
			f.mu.Lock()
			var prop fakeProp
			var found bool
			for name, atom := range f.atoms {
				if atom == u32(4) {
					prop, found = f.props[u32(0)][name]
				}
			}
			f.mu.Unlock()
			if found {
				reply[1] = byte(prop.format)
				binary.LittleEndian.PutUint32(reply[4:], uint32(pad4(len(prop.data))/4))
				binary.LittleEndian.PutUint32(reply[8:], f.atom(prop.kind))
				binary.LittleEndian.PutUint32(reply[16:], uint32(len(prop.data)/(prop.format/8)))
				reply = append(reply, prop.data...)
				reply = append(reply, make([]byte, pad4(len(prop.data))-len(prop.data))...)
			}
		case x11GetGeometry, x11TranslateCoordinates:
			rect, ok := f.geometry[u32(0)]
			if !ok {
				reply = make([]byte, 32) // BadWindow
				reply[1] = 3
				binary.LittleEndian.PutUint16(reply[2:], seq)
				reply[10] = header[0]
				break
			}
			if header[0] == x11GetGeometry {
				binary.LittleEndian.PutUint16(reply[16:], uint16(rect[2]))
				binary.LittleEndian.PutUint16(reply[18:], uint16(rect[3]))
			} else {
				binary.LittleEndian.PutUint16(reply[12:], uint16(int16(rect[0])))
				binary.LittleEndian.PutUint16(reply[14:], uint16(int16(rect[1])))
			}
		case x11QueryPointer:
			binary.LittleEndian.PutUint16(reply[16:], 640)
			binary.LittleEndian.PutUint16(reply[18:], 400)
		case x11GetInputFocus:
			binary.LittleEndian.PutUint32(reply[8:], 0x100)
		case x11SendEvent:
			event := data[8:]
			sent := []uint32{binary.LittleEndian.Uint32(event[4:]), binary.LittleEndian.Uint32(event[8:])}
			for i := 0; i < 5; i++ {
				sent = append(sent, binary.LittleEndian.Uint32(event[12+4*i:]))
			}
			f.mu.Lock()
			f.sent = append(f.sent, sent)
			f.mu.Unlock()
			continue // no reply
		default:
			continue
		}
		conn.Write(reply)
	}
}

// newFakeDesktop serves a fake display :0 under a temporary container root.
func newFakeDesktop(t *testing.T) (*fakeX, desktopEnv) {
	t.Helper()
	root, err := os.MkdirTemp("", "vx") // unix socket paths are short
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(root) })
	if err := os.MkdirAll(filepath.Join(root, "tmp/.X11-unix"), 0o755); err != nil {
		t.Fatal(err)
	}
	listener, err := net.Listen("unix", filepath.Join(root, "tmp/.X11-unix/X0"))
	if err != nil {
		t.Skip("unix socket unavailable:", err)
	}
	t.Cleanup(func() { listener.Close() })
	f := &fakeX{atoms: map[string]uint32{}, props: map[uint32]map[string]fakeProp{}, geometry: map[uint32][4]int{1: {0, 0, 1280, 800}}}
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go f.serve(conn)
		}
	}()
	f.props[1] = map[string]fakeProp{
		"_NET_CLIENT_LIST_STACKING": cardinalProp(0x200, 0x100),
		"_NET_ACTIVE_WINDOW":        cardinalProp(0x100),
		"_NET_SUPPORTING_WM_CHECK":  cardinalProp(0x300),
		"_NET_CURRENT_DESKTOP":      cardinalProp(0),
		"_NET_NUMBER_OF_DESKTOPS":   cardinalProp(2),
		"_NET_DESKTOP_NAMES":        textProp("UTF8_STRING", "Main", "Spare", ""),
		"_XKB_RULES_NAMES":          textProp("STRING", "evdev", "pc105", "us", "", ""),
	}
	f.props[0x300] = map[string]fakeProp{"_NET_WM_NAME": textProp("UTF8_STRING", "Xfwm4")}
	f.props[0x100] = map[string]fakeProp{
		"_NET_WM_NAME":        textProp("UTF8_STRING", "Terminal – ~"),
		"WM_CLASS":            textProp("STRING", "xfce4-terminal", "Xfce4-terminal"),
		"_NET_WM_DESKTOP":     cardinalProp(0),
		"_NET_WM_WINDOW_TYPE": f.atomProp("_NET_WM_WINDOW_TYPE_NORMAL"),
		"_NET_WM_STATE":       f.atomProp("_NET_WM_STATE_MAXIMIZED_VERT", "_NET_WM_STATE_FOCUSED"),
		"_NET_FRAME_EXTENTS":  cardinalProp(1, 1, 24, 1),
	}
	f.props[0x200] = map[string]fakeProp{
		"WM_NAME":             textProp("STRING", "Panel \xe9"),
		"_NET_WM_DESKTOP":     cardinalProp(0xFFFFFFFF),
		"_NET_WM_WINDOW_TYPE": f.atomProp("_NET_WM_WINDOW_TYPE_DOCK"),
	}
	f.geometry[0x100] = [4]int{100, 124, 640, 400}
	f.geometry[0x200] = [4]int{0, 0, 1280, 32}
	return f, desktopEnv{root: root}
}

func TestDesktopStateReadsEWMH(t *testing.T) {
	_, env := newFakeDesktop(t)
	state, err := env.state()
	if err != nil {
		t.Fatal(err)
	}
	if state["window_manager"] != "Xfwm4" || state["active_window"] != uint32(0x100) {
		t.Fatalf("state: %v", state)
	}
	if got := state["screen"].(map[string]int); got["width"] != 1280 || got["height"] != 800 {
		t.Fatalf("screen: %v", got)
	}
	if got := state["keyboard"].(map[string]string); got["layout"] != "us" || got["model"] != "pc105" {
		t.Fatalf("keyboard: %v", got)
	}
	if got := state["workspace"].(map[string]any); !slices.Equal(got["names"].([]string), []string{"Main", "Spare"}) || got["count"] != uint32(2) {
		t.Fatalf("workspace: %v", got)
	}
	windows := state["windows"].([]desktopWindow)
	if len(windows) != 2 {
		t.Fatalf("windows: %+v", windows)
	}
	top, panel := windows[0], windows[1]
	if top.ID != 0x100 || top.Title != "Terminal – ~" || top.Class != "Xfce4-terminal" || !top.Active || top.Type != "normal" ||
		!slices.Equal(top.States, []string{"maximized_vert", "focused"}) || top.X != 100 || top.Width != 640 ||
		top.Frame["y"] != 100 || top.Frame["height"] != 425 || *top.Workspace != 0 {
		t.Fatalf("top window: %+v", top)
	}
	if panel.Title != "Panel é" || panel.Type != "dock" || panel.Active || *panel.Workspace != -1 {
		t.Fatalf("panel: %+v", panel)
	}
	if focused := state["focused_app"].(map[string]any); focused["class"] != "Xfce4-terminal" {
		t.Fatalf("focused: %v", focused)
	}
	if _, err := json.Marshal(state); err != nil {
		t.Fatal(err)
	}
}

func TestWindowActionsSendEWMHRequests(t *testing.T) {
	f, env := newFakeDesktop(t)
	maxV, maxH, full := f.atom("_NET_WM_STATE_MAXIMIZED_VERT"), f.atom("_NET_WM_STATE_MAXIMIZED_HORZ"), f.atom("_NET_WM_STATE_FULLSCREEN")
	x, w := 10, 500
	for _, tc := range []struct {
		args windowArgs
		want [][]uint32
	}{
		{windowArgs{Window: 0x100, Action: "activate"}, [][]uint32{{0x100, f.atom("_NET_ACTIVE_WINDOW"), 2, 0, 0, 0, 0}}},
		{windowArgs{Window: 0x100, Action: "close"}, [][]uint32{{0x100, f.atom("_NET_CLOSE_WINDOW"), 0, 2, 0, 0, 0}}},
		{windowArgs{Window: 0x100, Action: "minimize"}, [][]uint32{{0x100, f.atom("WM_CHANGE_STATE"), 3, 0, 0, 0, 0}}},
		{windowArgs{Window: 0x100, Action: "maximize"}, [][]uint32{{0x100, f.atom("_NET_WM_STATE"), 1, maxV, maxH, 2, 0}}},
		{windowArgs{Window: 0x100, Action: "unfullscreen"}, [][]uint32{{0x100, f.atom("_NET_WM_STATE"), 0, full, 0, 2, 0}}},
		{windowArgs{Window: 0x100, Action: "move_resize", X: &x, Width: &w}, [][]uint32{
			{0x100, f.atom("_NET_WM_STATE"), 0, maxV, maxH, 2, 0},
			{0x100, f.atom("_NET_MOVERESIZE_WINDOW"), 10 | 1<<8 | 1<<10 | 2<<12, 10, 0, 500, 0},
		}},
	} {
		f.mu.Lock()
		f.sent = nil
		f.mu.Unlock()
		result, err := env.windowAction(tc.args)
		if err != nil {
			t.Fatalf("%s: %v", tc.args.Action, err)
		}
		if result["window_after"].(desktopWindow).ID != 0x100 {
			t.Fatalf("%s: %v", tc.args.Action, result)
		}
		f.mu.Lock()
		sent := f.sent
		f.mu.Unlock()
		if !slices.EqualFunc(sent, tc.want, slices.Equal) {
			t.Fatalf("%s sent %v, want %v", tc.args.Action, sent, tc.want)
		}
	}
	for _, bad := range []windowArgs{{Window: 0x999, Action: "close"}, {Window: 0x100, Action: "explode"}, {Window: 0x100, Action: "move_resize"}} {
		_, err := env.windowAction(bad)
		var coded *observeError
		if !errors.As(err, &coded) || coded.code != "invalid_request" {
			t.Fatalf("%+v: %v", bad, err)
		}
	}
}

func TestDesktopRefusedWithoutCookie(t *testing.T) {
	f, env := newFakeDesktop(t)
	f.refuse = true
	_, err := env.state()
	var coded *observeError
	if !errors.As(err, &coded) || coded.code != "desktop_unavailable" {
		t.Fatalf("refused display: %v", err)
	}
}

func TestXauthCookie(t *testing.T) {
	entry := func(number, name, cookie string) []byte {
		var out []byte
		out = binary.BigEndian.AppendUint16(out, 256) // FamilyLocal
		for _, field := range []string{"host", number, name, cookie} {
			out = binary.BigEndian.AppendUint16(out, uint16(len(field)))
			out = append(out, field...)
		}
		return out
	}
	file := append(entry("1", "MIT-MAGIC-COOKIE-1", "wrong"), entry("0", "MIT-MAGIC-COOKIE-1", "right")...)
	if got := xauthCookie(file, "0"); string(got) != "right" {
		t.Fatalf("cookie %q", got)
	}
	if xauthCookie(file[:10], "0") != nil || xauthCookie(file, "2") != nil {
		t.Fatal("truncated or absent entries must not yield a cookie")
	}
}

func TestExecArgvFollowsDesktopEntrySpec(t *testing.T) {
	for _, tc := range []struct {
		exec string
		want []string
	}{
		{"firefox %u", []string{"firefox"}},
		{`"/opt/My App/run" --name=%c --flag %F`, []string{"/opt/My App/run", "--name=Editor", "--flag"}},
		{`sh -c "echo \"a b\" \$HOME"`, []string{"sh", "-c", `echo "a b" $HOME`}},
		{"tool 100%% %i", []string{"tool", "100%"}},
		{`app ""`, []string{"app", ""}},
	} {
		got, err := execArgv(tc.exec, "Editor")
		if err != nil || !slices.Equal(got, tc.want) {
			t.Fatalf("%q: %q %v", tc.exec, got, err)
		}
	}
	for _, bad := range []string{`app "open`, "%f %U", ""} {
		if _, err := execArgv(bad, "x"); err == nil {
			t.Fatalf("%q accepted", bad)
		}
	}
}

func TestDesktopAppsListsVisibleApplications(t *testing.T) {
	root := t.TempDir()
	write := func(path, body string) {
		full := filepath.Join(root, path)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write("usr/share/applications/xfce4-terminal.desktop", "[Desktop Entry]\nType=Application\nName=Xfce Terminal\nName[de]=Terminal\nGenericName=Terminal Emulator\nExec=xfce4-terminal\nCategories=System;TerminalEmulator;\n[Desktop Action new]\nName=New Window\nExec=other\n")
	write("usr/share/applications/hidden.desktop", "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\nNoDisplay=true\n")
	write("usr/share/applications/link.desktop", "[Desktop Entry]\nType=Link\nName=Link\nURL=http://x\n")
	write("usr/share/applications/kde/editor.desktop", "[Desktop Entry]\nType=Application\nName=Editor\nComment=Edit\\stext\nExec=kate %U\nTerminal=false\n")
	write("usr/share/applications/top.desktop", "[Desktop Entry]\nType=Application\nName=System Monitor\nExec=top\nTerminal=true\n")
	write("home/u/.local/share/applications/top.desktop", "[Desktop Entry]\nType=Application\nName=My Top\nExec=htop\nTerminal=true\n")
	apps := desktopEnv{root: root}.apps("/home/u")
	var names, ids []string
	for _, app := range apps {
		names, ids = append(names, app.Name), append(ids, app.ID)
	}
	if !slices.Equal(names, []string{"Editor", "My Top", "Xfce Terminal"}) || !slices.Equal(ids, []string{"kde-editor", "top", "xfce4-terminal"}) {
		t.Fatalf("apps %v %v", names, ids)
	}
	if apps[0].Comment != "Edit text" || !apps[1].Terminal || apps[2].Exec != "xfce4-terminal" || !slices.Equal(apps[2].Categories, []string{"System", "TerminalEmulator"}) {
		t.Fatalf("fields %+v", apps)
	}
	if !appID.MatchString("kde-editor") || appID.MatchString("../x") || appID.MatchString("a/b") {
		t.Fatal("app id pattern")
	}
}

func TestDesktopToolsNeedDesktop(t *testing.T) {
	saved := desktop
	defer func() { desktop = saved }()
	desktop = nil
	for _, method := range []string{"desktop_state", "window_action", "app_launch", "accessibility_action"} {
		if reply := handle(request{ID: "1", Method: method}, false); reply.Error == nil || reply.Error.Code != "unsupported" {
			t.Fatalf("%s without desktop: %+v", method, reply)
		}
	}
}
