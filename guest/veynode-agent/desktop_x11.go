package main

// Window, application and environment tools for the X11 desktop of a
// container (enabled with --desktop-stream), as an X client of display :0
// through the container root, so no guest image change is needed:
//
//   desktop_state  screen, cursor, keyboard layout, workspaces, focus and the
//                  managed windows (EWMH) top-most first, with titles, classes,
//                  processes, states and geometry in screen pixels
//   desktop_apps   installed applications (.desktop entries)
//   window_action  activate, close, minimize, (un)maximize, (un)fullscreen,
//                  move_resize a managed window through the window manager
//   app_launch     start an installed application as the desktop session's
//                  user, with the session's environment

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const (
	desktopMaxWindows = 200
	desktopMaxApps    = 500
	desktopDisplay    = "0"
	desktopLaunchWait = 5 * time.Second
)

// desktopEnv locates the desktop: the container root holds the X socket,
// the session processes and the application entries.
type desktopEnv struct {
	root string
}

func (d desktopEnv) path(p string) string { return filepath.Join(d.root, p) }

// desktopSession is a process of the running desktop session; its user and
// environment are what applications are launched with.
type desktopSession struct {
	pid      int
	uid, gid uint32
	command  string
	env      []string
}

func (s desktopSession) getenv(key string) string {
	for _, entry := range s.env {
		if value, ok := strings.CutPrefix(entry, key+"="); ok {
			return value
		}
	}
	return ""
}

var sessionLeaders = []string{"xfce4-session", "gnome-session-b", "gnome-shell", "startplasma-x11", "plasmashell", "lxsession", "mate-session", "cinnamon-sessio", "openbox", "i3", "startxfce4"}

// session finds a process of the container with DISPLAY set, preferring a
// session leader.
func (d desktopEnv) session() (desktopSession, error) {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return desktopSession{}, err
	}
	root, _ := filepath.EvalSymlinks(d.root)
	var best desktopSession
	bestRank := len(sessionLeaders) + 1
	for _, entry := range entries {
		pid, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}
		dir := "/proc/" + entry.Name()
		if link, err := os.Readlink(dir + "/root"); err != nil || filepath.Clean(link) != root {
			continue
		}
		environ, err := os.ReadFile(dir + "/environ")
		if err != nil || !bytes.Contains(environ, []byte("DISPLAY=")) {
			continue
		}
		comm, _ := os.ReadFile(dir + "/comm")
		command := strings.TrimSpace(string(comm))
		rank := slices.Index(sessionLeaders, command)
		if rank < 0 {
			rank = len(sessionLeaders)
		}
		if rank >= bestRank {
			continue
		}
		uid, gid, ok := processIDs(dir + "/status")
		if !ok {
			continue
		}
		var env []string
		for _, part := range bytes.Split(environ, []byte{0}) {
			if len(part) > 0 {
				env = append(env, string(part))
			}
		}
		best, bestRank = desktopSession{pid: pid, uid: uid, gid: gid, command: command, env: env}, rank
	}
	if best.pid == 0 {
		return desktopSession{}, errors.New("no desktop session process (is the desktop running?)")
	}
	return best, nil
}

func processIDs(status string) (uid, gid uint32, ok bool) {
	data, err := os.ReadFile(status)
	if err != nil {
		return 0, 0, false
	}
	var haveUID, haveGID bool
	for _, line := range strings.Split(string(data), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		value, err := strconv.ParseUint(fields[1], 10, 32)
		if err != nil {
			continue
		}
		switch fields[0] {
		case "Uid:":
			uid, haveUID = uint32(value), true
		case "Gid:":
			gid, haveGID = uint32(value), true
		}
	}
	return uid, gid, haveUID && haveGID
}

// dial connects to display :0; when the server wants a cookie, the session's
// Xauthority (or the usual files) supplies MIT-MAGIC-COOKIE-1.
func (d desktopEnv) dial() (*x11Conn, error) {
	connect := func(name string, data []byte) (*x11Conn, error) {
		conn, err := net.DialTimeout("unix", d.path("tmp/.X11-unix/X"+desktopDisplay), 2*time.Second)
		if err != nil {
			return nil, fmt.Errorf("no X11 display :0 in the container: %w", err)
		}
		_ = conn.SetDeadline(time.Now().Add(3 * time.Second))
		x, err := x11Handshake(conn, name, data)
		if err != nil {
			conn.Close()
		}
		return x, err
	}
	x, err := connect("", nil)
	if err == nil || !strings.Contains(err.Error(), "refused") {
		return x, err
	}
	var files []string
	if session, serr := d.session(); serr == nil {
		if file := session.getenv("XAUTHORITY"); file != "" {
			files = append(files, file)
		}
		if home := session.getenv("HOME"); home != "" {
			files = append(files, filepath.Join(home, ".Xauthority"))
		}
	}
	files = append(files, "/root/.Xauthority")
	for _, file := range files {
		data, rerr := os.ReadFile(d.path(file))
		if rerr != nil {
			continue
		}
		if cookie := xauthCookie(data, desktopDisplay); cookie != nil {
			if x, cerr := connect("MIT-MAGIC-COOKIE-1", cookie); cerr == nil {
				return x, nil
			}
		}
	}
	return nil, err
}

// xauthCookie finds the MIT-MAGIC-COOKIE-1 for a display number in an
// Xauthority file (family, address, number, name, data; u16 BE lengths).
func xauthCookie(data []byte, display string) []byte {
	field := func() ([]byte, bool) {
		if len(data) < 2 {
			return nil, false
		}
		n := int(binary.BigEndian.Uint16(data))
		if len(data) < 2+n {
			return nil, false
		}
		value := data[2 : 2+n]
		data = data[2+n:]
		return value, true
	}
	for len(data) >= 2 {
		data = data[2:] // family
		_, ok1 := field()
		number, ok2 := field()
		name, ok3 := field()
		cookie, ok4 := field()
		if !ok1 || !ok2 || !ok3 || !ok4 {
			return nil
		}
		if string(number) == display && string(name) == "MIT-MAGIC-COOKIE-1" {
			return cookie
		}
	}
	return nil
}

type desktopWindow struct {
	ID        uint32         `json:"id"`
	Title     string         `json:"title"`
	Class     string         `json:"class,omitempty"`
	Instance  string         `json:"instance,omitempty"`
	PID       int            `json:"pid,omitempty"`
	Command   string         `json:"command,omitempty"`
	Type      string         `json:"type"`
	States    []string       `json:"states"`
	Workspace *int           `json:"workspace,omitempty"`
	Active    bool           `json:"active"`
	Minimized bool           `json:"minimized"`
	X         int            `json:"x"`
	Y         int            `json:"y"`
	Width     int            `json:"width"`
	Height    int            `json:"height"`
	Frame     map[string]int `json:"frame,omitempty"`
}

var windowStateNames = []string{"MODAL", "STICKY", "MAXIMIZED_VERT", "MAXIMIZED_HORZ", "SHADED", "SKIP_TASKBAR", "SKIP_PAGER", "HIDDEN", "FULLSCREEN", "ABOVE", "BELOW", "DEMANDS_ATTENTION", "FOCUSED"}
var windowTypeNames = []string{"DESKTOP", "DOCK", "TOOLBAR", "MENU", "UTILITY", "SPLASH", "DIALOG", "NORMAL"}

func (d desktopEnv) window(x *x11Conn, id, active uint32) (desktopWindow, error) {
	rect, err := x.geometry(id)
	if err != nil {
		return desktopWindow{}, err
	}
	w := desktopWindow{ID: id, Title: x.text(id, "_NET_WM_NAME", "WM_NAME"), Type: "normal", States: []string{}, Active: id == active,
		X: rect.X, Y: rect.Y, Width: rect.W, Height: rect.H}
	if class := x.strings(id, "WM_CLASS"); len(class) >= 2 {
		w.Instance, w.Class = class[0], class[1]
	}
	if pid, ok := x.cardinal(id, "_NET_WM_PID"); ok && pid > 0 {
		w.PID = int(pid)
		if comm, err := os.ReadFile(fmt.Sprintf("/proc/%d/comm", pid)); err == nil {
			w.Command = strings.TrimSpace(string(comm))
		}
	}
	if desktop, ok := x.cardinal(id, "_NET_WM_DESKTOP"); ok {
		value := int(int32(desktop)) // 0xFFFFFFFF: on every workspace
		w.Workspace = &value
	}
	for _, kind := range x.cardinals(id, "_NET_WM_WINDOW_TYPE") {
		if name := atomName(x, "_NET_WM_WINDOW_TYPE_", windowTypeNames, kind); name != "" {
			w.Type = name
			break
		}
	}
	for _, state := range x.cardinals(id, "_NET_WM_STATE") {
		if name := atomName(x, "_NET_WM_STATE_", windowStateNames, state); name != "" {
			w.States = append(w.States, name)
		}
	}
	w.Minimized = slices.Contains(w.States, "hidden")
	if extents := x.cardinals(id, "_NET_FRAME_EXTENTS"); len(extents) == 4 {
		left, right, top, bottom := int(extents[0]), int(extents[1]), int(extents[2]), int(extents[3])
		w.Frame = map[string]int{"x": rect.X - left, "y": rect.Y - top, "width": rect.W + left + right, "height": rect.H + top + bottom}
	}
	return w, nil
}

func atomName(x *x11Conn, prefix string, names []string, atom uint32) string {
	for _, name := range names {
		if value, err := x.atom(prefix + name); err == nil && value == atom {
			return strings.ToLower(name)
		}
	}
	return ""
}

// clients lists managed windows top-most first.
func clients(x *x11Conn) []uint32 {
	ids := x.cardinals(x.root, "_NET_CLIENT_LIST_STACKING")
	if len(ids) == 0 {
		ids = x.cardinals(x.root, "_NET_CLIENT_LIST")
	}
	slices.Reverse(ids)
	return ids
}

func (d desktopEnv) state() (map[string]any, error) {
	x, err := d.dial()
	if err != nil {
		return nil, observeFail("desktop_unavailable", "%v", err)
	}
	defer x.close()
	active, _ := x.cardinal(x.root, "_NET_ACTIVE_WINDOW")
	result := map[string]any{"display": ":" + desktopDisplay, "coordinate_space": "screen-pixels",
		"screen": map[string]int{"width": x.width, "height": x.height}}
	if px, py, err := x.pointer(); err == nil {
		result["cursor"] = map[string]int{"x": px, "y": py}
	}
	if check, ok := x.cardinal(x.root, "_NET_SUPPORTING_WM_CHECK"); ok {
		result["window_manager"] = x.text(check, "_NET_WM_NAME", "WM_NAME")
	}
	if xkb := x.strings(x.root, "_XKB_RULES_NAMES"); len(xkb) >= 5 {
		result["keyboard"] = map[string]string{"rules": xkb[0], "model": xkb[1], "layout": xkb[2], "variant": xkb[3], "options": xkb[4]}
	}
	workspace := map[string]any{}
	if current, ok := x.cardinal(x.root, "_NET_CURRENT_DESKTOP"); ok {
		workspace["current"] = current
	}
	if count, ok := x.cardinal(x.root, "_NET_NUMBER_OF_DESKTOPS"); ok {
		workspace["count"] = count
	}
	names := x.strings(x.root, "_NET_DESKTOP_NAMES")
	for len(names) > 0 && names[len(names)-1] == "" {
		names = names[:len(names)-1] // xfwm4 ends the list with an extra NUL
	}
	if len(names) > 0 {
		workspace["names"] = names
	}
	result["workspace"] = workspace
	windows := []desktopWindow{}
	ids := clients(x)
	for _, id := range ids[:min(len(ids), desktopMaxWindows)] {
		if w, err := d.window(x, id, active); err == nil {
			windows = append(windows, w)
		}
	}
	result["windows"] = windows
	result["active_window"] = nil
	for _, w := range windows {
		if w.Active {
			result["active_window"] = w.ID
			result["focused_app"] = map[string]any{"window": w.ID, "title": w.Title, "class": w.Class, "pid": w.PID, "command": w.Command}
		}
	}
	return result, nil
}

type windowArgs struct {
	Window uint32 `json:"window"`
	Action string `json:"action"`
	X      *int   `json:"x"`
	Y      *int   `json:"y"`
	Width  *int   `json:"width"`
	Height *int   `json:"height"`
	App    string `json:"app"`
}

func (d desktopEnv) windowAction(args windowArgs) (map[string]any, error) {
	x, err := d.dial()
	if err != nil {
		return nil, observeFail("desktop_unavailable", "%v", err)
	}
	defer x.close()
	if !slices.Contains(clients(x), args.Window) {
		return nil, observeFail("invalid_request", "window %d is not a managed window; read desktop_state", args.Window)
	}
	const source = 2 // a pager/agent request: honoured without focus-stealing checks
	state := func(op uint32, names ...string) error {
		data := []uint32{op, 0, 0, source}
		for i, name := range names {
			atom, err := x.atom("_NET_WM_STATE_" + name)
			if err != nil {
				return err
			}
			data[1+i] = atom
		}
		return x.message(args.Window, "_NET_WM_STATE", data...)
	}
	switch args.Action {
	case "activate":
		err = x.message(args.Window, "_NET_ACTIVE_WINDOW", source, 0, 0)
	case "close":
		err = x.message(args.Window, "_NET_CLOSE_WINDOW", 0, source)
	case "minimize":
		err = x.message(args.Window, "WM_CHANGE_STATE", 3) // IconicState
	case "maximize":
		err = state(1, "MAXIMIZED_VERT", "MAXIMIZED_HORZ")
	case "unmaximize":
		err = state(0, "MAXIMIZED_VERT", "MAXIMIZED_HORZ")
	case "fullscreen":
		err = state(1, "FULLSCREEN")
	case "unfullscreen":
		err = state(0, "FULLSCREEN")
	case "move_resize":
		flags := uint32(10) | source<<12 // StaticGravity: x, y place the client area
		values := []uint32{0, 0, 0, 0}
		for i, value := range []*int{args.X, args.Y, args.Width, args.Height} {
			if value == nil {
				continue
			}
			if *value < -16384 || *value > 16384 || i >= 2 && *value < 1 {
				return nil, observeFail("invalid_request", "geometry outside bound")
			}
			flags |= 1 << (8 + i)
			values[i] = uint32(int32(*value))
		}
		if flags>>8&0xf == 0 {
			return nil, observeFail("invalid_request", "move_resize needs x, y, width or height")
		}
		if err = state(0, "MAXIMIZED_VERT", "MAXIMIZED_HORZ"); err == nil {
			err = x.message(args.Window, "_NET_MOVERESIZE_WINDOW", append([]uint32{flags}, values...)...)
		}
	default:
		return nil, observeFail("invalid_request", "unknown window action %q", args.Action)
	}
	if err != nil {
		return nil, observeFail("window_action_failed", "%v", err)
	}
	// The window manager applies requests asynchronously; report what it did.
	time.Sleep(150 * time.Millisecond)
	result := map[string]any{"requested": true, "window": args.Window, "action": args.Action}
	active, _ := x.cardinal(x.root, "_NET_ACTIVE_WINDOW")
	if w, err := d.window(x, args.Window, active); err == nil {
		result["window_after"] = w
	} else {
		result["window_after"] = nil // closed
	}
	return result, nil
}

type desktopApp struct {
	ID          string   `json:"id"`
	Name        string   `json:"name"`
	GenericName string   `json:"generic_name,omitempty"`
	Comment     string   `json:"comment,omitempty"`
	Categories  []string `json:"categories,omitempty"`
	Terminal    bool     `json:"terminal,omitempty"`
	Exec        string   `json:"exec"`
	path        string
}

var appDirs = []string{"usr/share/applications", "usr/local/share/applications", "var/lib/flatpak/exports/share/applications"}

// parseDesktopEntry reads the [Desktop Entry] group of a .desktop file;
// nil when it is not a launchable, visible application.
func parseDesktopEntry(data []byte) *desktopApp {
	app := &desktopApp{}
	inEntry, hidden := false, false
	values := map[string]string{}
	scanner := bufio.NewScanner(bytes.NewReader(data))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if strings.HasPrefix(line, "[") {
			inEntry = line == "[Desktop Entry]"
			continue
		}
		key, value, ok := strings.Cut(line, "=")
		if !inEntry || !ok || strings.HasPrefix(line, "#") {
			continue
		}
		key = strings.TrimSpace(key)
		if _, seen := values[key]; !seen {
			values[key] = unescapeDesktopValue(strings.TrimSpace(value))
		}
	}
	hidden = values["NoDisplay"] == "true" || values["Hidden"] == "true"
	if values["Type"] != "Application" || values["Exec"] == "" || values["Name"] == "" || hidden {
		return nil
	}
	app.Name, app.GenericName, app.Comment, app.Exec = values["Name"], values["GenericName"], values["Comment"], values["Exec"]
	app.Terminal = values["Terminal"] == "true"
	for _, category := range strings.Split(values["Categories"], ";") {
		if category != "" {
			app.Categories = append(app.Categories, category)
		}
	}
	return app
}

func unescapeDesktopValue(value string) string {
	return strings.NewReplacer(`\s`, " ", `\n`, "\n", `\t`, "\t", `\r`, "\r", `\\`, `\`).Replace(value)
}

func (d desktopEnv) apps(home string) []desktopApp {
	dirs := slices.Clone(appDirs)
	if home != "" {
		dirs = append([]string{strings.TrimPrefix(filepath.Join(home, ".local/share/applications"), "/")}, dirs...)
	}
	seen := map[string]bool{}
	var out []desktopApp
	for _, dir := range dirs {
		base := d.path(dir)
		_ = filepath.WalkDir(base, func(path string, entry os.DirEntry, err error) error {
			if err != nil || entry.IsDir() || !strings.HasSuffix(path, ".desktop") || len(out) >= desktopMaxApps {
				return nil
			}
			rel, _ := filepath.Rel(base, path)
			id := strings.ReplaceAll(strings.TrimSuffix(rel, ".desktop"), "/", "-")
			if seen[id] {
				return nil // the first directory wins, as in the spec
			}
			seen[id] = true
			info, err := entry.Info()
			if err != nil || !info.Mode().IsRegular() || info.Size() > 256*1024 {
				return nil
			}
			data, err := os.ReadFile(path)
			if err != nil {
				return nil
			}
			if app := parseDesktopEntry(data); app != nil {
				app.ID, app.path = id, path
				out = append(out, *app)
			}
			return nil
		})
	}
	slices.SortFunc(out, func(a, b desktopApp) int { return strings.Compare(strings.ToLower(a.Name), strings.ToLower(b.Name)) })
	return out
}

// execArgv splits a desktop entry Exec value into argv and drops field codes
// (no files or URLs are passed).
func execArgv(command, name string) ([]string, error) {
	var argv []string
	var current strings.Builder
	quoted, escaped, have := false, false, false
	for _, r := range command {
		switch {
		case escaped:
			current.WriteRune(r)
			escaped = false
		case quoted && r == '\\':
			escaped = true
		case r == '"':
			quoted, have = !quoted, true
		case !quoted && (r == ' ' || r == '\t'):
			if have {
				argv = append(argv, current.String())
				current.Reset()
				have = false
			}
		default:
			current.WriteRune(r)
			have = true
		}
	}
	if quoted || escaped {
		return nil, errors.New("unterminated quote in Exec")
	}
	if have {
		argv = append(argv, current.String())
	}
	var out []string
	for _, arg := range argv {
		switch arg {
		case "%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%v", "%m", "%i", "%k":
			continue
		case "%c":
			out = append(out, name)
			continue
		}
		var b strings.Builder
		for i := 0; i < len(arg); i++ {
			if arg[i] == '%' && i+1 < len(arg) {
				switch arg[i+1] {
				case '%':
					b.WriteByte('%')
				case 'c':
					b.WriteString(name)
				}
				i++
				continue
			}
			b.WriteByte(arg[i])
		}
		out = append(out, b.String())
	}
	if len(out) == 0 {
		return nil, errors.New("empty Exec")
	}
	return out, nil
}

var appID = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._+-]{0,200}$`)

// terminalWrappers run a Terminal=true application, first one installed wins.
var terminalWrappers = [][]string{{"xfce4-terminal", "-x"}, {"gnome-terminal", "--"}, {"konsole", "-e"}, {"x-terminal-emulator", "-e"}, {"xterm", "-e"}}

func (d desktopEnv) launch(args windowArgs) (map[string]any, error) {
	if !appID.MatchString(args.App) {
		return nil, observeFail("invalid_request", "app must be an application id from desktop_apps")
	}
	session, err := d.session()
	if err != nil {
		return nil, observeFail("desktop_unavailable", "%v", err)
	}
	var app *desktopApp
	for _, candidate := range d.apps(session.getenv("HOME")) {
		if candidate.ID == args.App {
			app = &candidate
			break
		}
	}
	if app == nil {
		return nil, observeFail("invalid_request", "no installed application %q; read desktop_apps", args.App)
	}
	argv, err := execArgv(app.Exec, app.Name)
	if err != nil {
		return nil, observeFail("launch_failed", "%s: %v", app.ID, err)
	}
	identity := executionIdentity{Chroot: d.root, env: session.env}
	if app.Terminal {
		wrapped := false
		for _, wrapper := range terminalWrappers {
			if _, err := identity.executable(wrapper[0]); err == nil {
				argv, wrapped = append(slices.Clone(wrapper), argv...), true
				break
			}
		}
		if !wrapped {
			return nil, observeFail("launch_failed", "%s needs a terminal and none is installed", app.ID)
		}
	}
	path, err := identity.executable(argv[0])
	if err != nil {
		return nil, observeFail("launch_failed", "%s: %s not found", app.ID, argv[0])
	}
	cmd := exec.Command(path, argv[1:]...)
	// Toolkits that wait for a screen reader expose themselves to the
	// accessibility tools (accessibility_tree) from the start.
	cmd.Path, cmd.Env = path, append(slices.Clone(session.env), "QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1", "ACCESSIBILITY_ENABLED=1")
	cmd.Dir = "/"
	if home := session.getenv("HOME"); home != "" {
		if info, err := os.Stat(d.path(home)); err == nil && info.IsDir() {
			cmd.Dir = home
		}
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Credential: &syscall.Credential{Uid: session.uid, Gid: session.gid, NoSetGroups: true}}
	if d.root != "/" {
		cmd.SysProcAttr.Chroot = d.root
	}
	x, xerr := d.dial()
	var before []uint32
	if xerr == nil {
		defer x.close()
		before = clients(x)
	}
	if err := cmd.Start(); err != nil {
		return nil, observeFail("launch_failed", "%s: %v", app.ID, err)
	}
	go func() { _ = cmd.Wait() }()
	result := map[string]any{"launched": true, "app": app.ID, "name": app.Name, "pid": cmd.Process.Pid, "argv": argv, "user_id": session.uid, "window": nil}
	if xerr != nil {
		return result, nil
	}
	// Window managers keep a window started without a user action from taking
	// focus; wait for the application's window and activate it, so input goes
	// to it. Launchers (exo-open, terminals reusing a server) show a window of
	// another process, so any new normal window counts.
	deadline := time.Now().Add(desktopLaunchWait)
	for time.Now().Before(deadline) {
		time.Sleep(100 * time.Millisecond)
		var found *desktopWindow
		for _, id := range clients(x) {
			if slices.Contains(before, id) {
				continue
			}
			w, err := d.window(x, id, 0)
			if err != nil || w.Type != "normal" && w.Type != "dialog" {
				continue
			}
			if found == nil || w.PID == cmd.Process.Pid {
				found = &w
			}
		}
		if found == nil {
			continue
		}
		_ = x.message(found.ID, "_NET_ACTIVE_WINDOW", 2, 0, 0)
		time.Sleep(150 * time.Millisecond)
		active, _ := x.cardinal(x.root, "_NET_ACTIVE_WINDOW")
		if w, err := d.window(x, found.ID, active); err == nil {
			result["window"] = w
		}
		return result, nil
	}
	result["note"] = "no new window within 5 s; it may still appear (use screen_wait or desktop_state) or the application runs without one"
	return result, nil
}

func (d desktopEnv) handle(req request) response {
	var args windowArgs
	if len(req.Arguments) > 0 {
		decoder := json.NewDecoder(bytes.NewReader(req.Arguments))
		if err := decoder.Decode(&args); err != nil && err != io.EOF {
			return failure(req.ID, "invalid_request", "invalid arguments")
		}
	}
	var result any
	var err error
	switch req.Method {
	case "desktop_state":
		result, err = d.state()
	case "desktop_apps":
		home := ""
		if session, serr := d.session(); serr == nil {
			home = session.getenv("HOME")
		}
		apps := d.apps(home)
		if apps == nil {
			apps = []desktopApp{}
		}
		result = map[string]any{"apps": apps}
	case "window_action":
		result, err = d.windowAction(args)
	case "app_launch":
		result, err = d.launch(args)
	}
	if err != nil {
		var coded *observeError
		if errors.As(err, &coded) {
			return failure(req.ID, coded.code, coded.message)
		}
		return failure(req.ID, "desktop_failed", err.Error())
	}
	return response{ID: req.ID, Result: result}
}
