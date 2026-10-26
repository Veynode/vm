package main

// Accessibility tree (AT-SPI 2) of a desktop container, read over D-Bus with
// dbus.go, so an agent can find controls by role and name and act on them
// without a screenshot:
//
//   accessibility_tree    applications, windows and controls in tree order
//                         with role, name, text, value, states, actions and
//                         a box in screen pixels; name/role turn it into a
//                         search that returns only matches
//   accessibility_action  do an action (click, press, toggle ...), focus,
//                         set text or set a value on one element by ref
//
// The desktop's own accessibility bus is used (org.a11y.Bus on the session
// bus, or the AT_SPI_BUS root window property). Reading turns accessibility
// on for the session (org.a11y.Status.IsEnabled), which toolkits such as Qt
// and Chromium wait for; GTK exposes its widgets whenever the bus exists.
// The work runs as the desktop session's user, in a helper process when that
// is not the agent's user, because the buses only admit their owner.

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"os"
	"os/exec"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const (
	a11yAccessible     = "org.a11y.atspi.Accessible"
	a11yProperties     = "org.freedesktop.DBus.Properties"
	a11yTreeDeadline   = 8 * time.Second
	a11yActionDeadline = 5 * time.Second
	a11yMaxVisited     = 6000
	a11yMaxChildren    = 300
	a11yMaxText        = 200
	a11yMaxTerminal    = 4000
	a11yHelperArg      = "__accessibility"
	a11yMaxReply       = 4 << 20
)

type a11yArgs struct {
	App           string   `json:"app"`
	Name          string   `json:"name"`
	Role          string   `json:"role"`
	MaxDepth      int      `json:"max_depth"`
	MaxNodes      int      `json:"max_nodes"`
	IncludeHidden bool     `json:"include_hidden"`
	Ref           string   `json:"ref"`
	Action        string   `json:"action"`
	ActionName    string   `json:"action_name"`
	Text          *string  `json:"text"`
	Value         *float64 `json:"value"`
}

// a11yJob is everything the work needs, so it can run in a helper process.
type a11yJob struct {
	Method string   `json:"method"`
	Root   string   `json:"root"`
	Env    []string `json:"env"`
	Args   a11yArgs `json:"args"`
}

type a11yNode struct {
	Ref      string         `json:"ref"`
	Depth    int            `json:"depth"`
	Role     string         `json:"role"`
	Name     string         `json:"name,omitempty"`
	Text     string         `json:"text,omitempty"`
	Value    *float64       `json:"value,omitempty"`
	States   []string       `json:"states,omitempty"`
	Actions  []string       `json:"actions,omitempty"`
	Box      map[string]int `json:"box,omitempty"`
	PID      uint32         `json:"pid,omitempty"`
	Children int            `json:"unexpanded_children,omitempty"`
	Context  string         `json:"context,omitempty"`

	bus, path string
	ifaces    []string
	showing   bool
	parent    *a11yNode
	kids      []*a11yNode
	childRefs [][2]string
}

// atspiStates is AtspiStateType in order.
var atspiStates = []string{"invalid", "active", "armed", "busy", "checked", "collapsed", "defunct", "editable", "enabled", "expandable", "expanded", "focusable", "focused", "has_tooltip", "horizontal", "iconified", "modal", "multi_line", "multiselectable", "opaque", "pressed", "resizable", "selectable", "selected", "sensitive", "showing", "single_line", "stale", "transient", "vertical", "visible", "manages_descendants", "indeterminate", "required", "truncated", "animated", "invalid_entry", "supports_autocompletion", "selectable_text", "is_default", "visited", "checkable", "has_popup", "read_only"}

// Reported states leave out what nearly every element has.
var a11yQuietStates = []string{"invalid", "enabled", "sensitive", "showing", "visible", "opaque", "horizontal", "vertical", "single_line", "resizable", "selectable_text", "has_tooltip", "focusable", "selectable", "manages_descendants"}

var a11yRef = regexp.MustCompile(`^(:[0-9]+\.[0-9]+|[A-Za-z_][A-Za-z0-9_.-]{0,254})\|(/[A-Za-z0-9_/]{0,1000})$`)

func (n *a11yNode) has(iface string) bool {
	return slices.Contains(n.ifaces, "org.a11y.atspi."+iface)
}

func newA11yNode(bus, path string, depth int, parent *a11yNode) *a11yNode {
	return &a11yNode{Ref: bus + "|" + path, bus: bus, path: path, Depth: depth, parent: parent}
}

func (n *a11yNode) call(iface, member, sig string, args ...any) dbusCall {
	return dbusCall{n.bus, n.path, iface, member, sig, args}
}

func (n *a11yNode) property(iface, name string) dbusCall {
	return n.call(a11yProperties, "Get", "ss", iface, name)
}

// describe fills role, name, states and interfaces of nodes, and their
// children when expand is set, in one batch.
func describe(c *dbusConn, nodes []*a11yNode, expand bool) {
	var calls []dbusCall
	for _, n := range nodes {
		calls = append(calls, n.call(a11yAccessible, "GetRoleName", ""), n.call(a11yAccessible, "GetState", ""),
			n.call(a11yAccessible, "GetInterfaces", ""), n.property(a11yAccessible, "Name"))
		if expand {
			calls = append(calls, n.call(a11yAccessible, "GetChildren", ""))
		} else {
			calls = append(calls, n.property(a11yAccessible, "ChildCount"))
		}
	}
	replies := c.batch(calls)
	for i, n := range nodes {
		r := replies[5*i : 5*i+5]
		if r[0].err != nil {
			n.Role = "" // gone or not accessible; dropped by the caller
			continue
		}
		n.Role = firstString(r[0].values)
		if len(r[1].values) == 1 {
			words, _ := r[1].values[0].([]any)
			for w, word := range words {
				bits, _ := word.(uint32)
				for b := 0; b < 32; b++ {
					if bits&(1<<b) != 0 && 32*w+b < len(atspiStates) {
						state := atspiStates[32*w+b]
						n.showing = n.showing || state == "showing"
						if !slices.Contains(a11yQuietStates, state) {
							n.States = append(n.States, state)
						}
					}
				}
			}
			if !slices.Contains(stateNames(words), "sensitive") && n.Role != "application" {
				n.States = append(n.States, "disabled")
			}
		}
		if len(r[2].values) == 1 {
			for _, iface := range asSlice(r[2].values[0]) {
				if name, ok := iface.(string); ok {
					n.ifaces = append(n.ifaces, name)
				}
			}
		}
		n.Name = firstString(variantValues(r[3].values))
		if expand {
			if len(r[4].values) == 1 {
				for _, child := range asSlice(r[4].values[0]) {
					pair := asSlice(child)
					if len(pair) == 2 {
						bus, _ := pair[0].(string)
						path, _ := pair[1].(string)
						if bus != "" && path != "" && path != "/org/a11y/atspi/null" {
							n.childRefs = append(n.childRefs, [2]string{bus, path})
						}
					}
				}
			}
		} else if count, ok := firstValue(variantValues(r[4].values)).(int32); ok {
			n.Children = int(count)
		}
	}
}

// detail fills boxes, actions, text and values in one batch.
func detail(c *dbusConn, nodes []*a11yNode) {
	type pending struct {
		node *a11yNode
		kind string
	}
	var calls []dbusCall
	var what []pending
	for _, n := range nodes {
		if n.has("Component") {
			calls, what = append(calls, n.call("org.a11y.atspi.Component", "GetExtents", "u", uint32(0))), append(what, pending{n, "box"})
		}
		if n.has("Action") {
			calls, what = append(calls, n.call("org.a11y.atspi.Action", "GetActions", "")), append(what, pending{n, "actions"})
		}
		if n.has("Text") && n.Role == "terminal" {
			calls, what = append(calls, n.property("org.a11y.atspi.Text", "CharacterCount")), append(what, pending{n, "terminal"})
		} else if n.has("Text") && n.Name == "" && n.Role != "password text" {
			calls, what = append(calls, n.call("org.a11y.atspi.Text", "GetText", "ii", int32(0), int32(a11yMaxText))), append(what, pending{n, "text"})
		}
		if n.has("Value") {
			calls, what = append(calls, n.property("org.a11y.atspi.Value", "CurrentValue")), append(what, pending{n, "value"})
		}
	}
	var tails []dbusCall
	var terminals []*a11yNode
	for i, reply := range c.batch(calls) {
		if reply.err != nil {
			continue
		}
		n := what[i].node
		switch what[i].kind {
		case "terminal":
			// The end of a terminal's text is its most recent output.
			if count, ok := firstValue(variantValues(reply.values)).(int32); ok && count > 0 {
				tails = append(tails, n.call("org.a11y.atspi.Text", "GetText", "ii", max(0, count-a11yMaxTerminal), count))
				terminals = append(terminals, n)
			}
		case "box":
			box := asSlice(firstValue(reply.values))
			if len(box) != 4 {
				continue
			}
			v := make([]int, 4)
			for j, value := range box {
				number, _ := value.(int32)
				v[j] = int(number)
			}
			if v[2] > 0 && v[3] > 0 && v[0]+v[2] > 0 && v[1]+v[3] > 0 { // off-screen parking has no box
				n.Box = map[string]int{"x": v[0], "y": v[1], "width": v[2], "height": v[3], "center_x": v[0] + v[2]/2, "center_y": v[1] + v[3]/2}
			}
		case "actions":
			for _, action := range asSlice(firstValue(reply.values)) {
				if fields := asSlice(action); len(fields) > 0 {
					if name, _ := fields[0].(string); name != "" {
						n.Actions = append(n.Actions, name)
					}
				}
			}
		case "text":
			text := strings.TrimSpace(firstString(reply.values))
			if runes := []rune(text); len(runes) >= a11yMaxText {
				text = string(runes[:a11yMaxText]) + "…"
			}
			n.Text = text
		case "value":
			if value, ok := firstValue(variantValues(reply.values)).(float64); ok {
				n.Value = &value
			}
		}
	}
	for i, reply := range c.batch(tails) {
		if reply.err == nil {
			terminals[i].Text = terminalTail(firstString(reply.values))
		}
	}
}

// terminalTail keeps the last lines of terminal text without trailing blank
// space (a terminal's text is padded to its full height).
func terminalTail(text string) string {
	lines := strings.Split(text, "\n")
	for i := range lines {
		lines[i] = strings.TrimRight(lines[i], " \t")
	}
	for len(lines) > 0 && lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	if len(lines) > 40 {
		lines = lines[len(lines)-40:]
	}
	return strings.Join(lines, "\n")
}

func stateNames(words []any) []string {
	var out []string
	for w, word := range words {
		bits, _ := word.(uint32)
		for b := 0; b < 32; b++ {
			if bits&(1<<b) != 0 && 32*w+b < len(atspiStates) {
				out = append(out, atspiStates[32*w+b])
			}
		}
	}
	return out
}

func asSlice(v any) []any {
	out, _ := v.([]any)
	return out
}

func firstValue(values []any) any {
	if len(values) == 0 {
		return nil
	}
	return values[0]
}

func firstString(values []any) string {
	s, _ := firstValue(values).(string)
	return s
}

// variantValues unwraps a Properties.Get reply.
func variantValues(values []any) []any {
	if v, ok := firstValue(values).(dbusVariant); ok {
		return []any{v.Value}
	}
	return nil
}

func lower(s string) string { return strings.ToLower(s) }

// a11yBus finds and connects the desktop's accessibility bus, turning
// accessibility on for the session when it is off.
func a11yBus(job a11yJob, deadline time.Time) (*dbusConn, bool, error) {
	env := desktopSession{env: job.Env}
	uid := os.Geteuid()
	address, enabledNow := "", false
	if session := env.getenv("DBUS_SESSION_BUS_ADDRESS"); session != "" {
		if c, err := dialDBus(session, job.Root, uid, deadline); err == nil {
			status := dbusCall{"org.a11y.Bus", "/org/a11y/bus", a11yProperties, "Get", "ss", []any{"org.a11y.Status", "IsEnabled"}}
			if enabled, ok := firstValue(variantValues(must(c.call(status)))).(bool); ok && !enabled {
				set := dbusCall{"org.a11y.Bus", "/org/a11y/bus", a11yProperties, "Set", "ssv", []any{"org.a11y.Status", "IsEnabled", dbusVariant{"b", true}}}
				_, err := c.call(set)
				enabledNow = err == nil
			}
			address = firstString(must(c.call(dbusCall{"org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", "", nil})))
			c.close()
		}
	}
	if address == "" {
		if x, err := (desktopEnv{root: job.Root}).dial(); err == nil {
			if values := x.strings(x.root, "AT_SPI_BUS"); len(values) > 0 {
				address = values[0]
			}
			x.close()
		}
	}
	if address == "" {
		return nil, false, observeFail("accessibility_unavailable", "the desktop has no accessibility bus (at-spi2-core not installed or not started)")
	}
	c, err := dialDBus(address, job.Root, uid, deadline)
	if err != nil {
		return nil, false, observeFail("accessibility_unavailable", "accessibility bus: %v", err)
	}
	return c, enabledNow, nil
}

func must(values []any, err error) []any {
	if err != nil {
		return nil
	}
	return values
}

func a11yTree(c *dbusConn, args a11yArgs) (map[string]any, error) {
	maxDepth, maxNodes := args.MaxDepth, args.MaxNodes
	if maxDepth <= 0 || maxDepth > 40 {
		maxDepth = 25
	}
	if maxNodes <= 0 || maxNodes > 2000 {
		maxNodes = 400
	}
	registry := newA11yNode("org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root", -1, nil)
	describe(c, []*a11yNode{registry}, true)
	var apps []*a11yNode
	for _, ref := range registry.childRefs {
		apps = append(apps, newA11yNode(ref[0], ref[1], 0, nil))
	}
	// Application process ids, for matching windows from desktop_state.
	var pidCalls []dbusCall
	for _, app := range apps {
		pidCalls = append(pidCalls, dbusCall{"org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "GetConnectionUnixProcessID", "s", []any{app.bus}})
	}
	for i, reply := range c.batch(pidCalls) {
		apps[i].PID, _ = firstValue(reply.values).(uint32)
	}
	find := args.Name != "" || args.Role != ""
	visited, truncated := 0, ""
	level := apps
	var roots []*a11yNode
	for depth := 0; len(level) > 0; depth++ {
		if visited+len(level) > a11yMaxVisited {
			level, truncated = level[:max(0, a11yMaxVisited-visited)], "visited"
		}
		visited += len(level)
		describe(c, level, depth < maxDepth)
		var kept []*a11yNode
		for _, n := range level {
			if n.Role == "" || slices.Contains(n.States, "defunct") {
				continue
			}
			if depth == 0 {
				if args.App != "" && !strings.Contains(lower(n.Name), lower(args.App)) && args.App != strconv.Itoa(int(n.PID)) {
					continue
				}
			} else if !args.IncludeHidden && !n.showing {
				continue
			}
			kept = append(kept, n)
		}
		detail(c, kept)
		var next []*a11yNode
		for _, n := range kept {
			if n.parent == nil {
				roots = append(roots, n)
			} else {
				n.parent.kids = append(n.parent.kids, n)
			}
			refs := n.childRefs
			if len(refs) > a11yMaxChildren {
				refs, truncated = refs[:a11yMaxChildren], "children"
			}
			for _, ref := range refs {
				next = append(next, newA11yNode(ref[0], ref[1], depth+1, n))
			}
		}
		level = next
		if time.Until(c.deadline) < time.Second && len(level) > 0 {
			truncated = "deadline"
			break
		}
	}
	var out []*a11yNode
	var walk func(*a11yNode)
	walk = func(n *a11yNode) {
		if len(out) >= maxNodes {
			truncated = "max_nodes"
			return
		}
		if !find || a11yMatches(n, args) {
			if find {
				n.Context = a11yContext(n)
			}
			out = append(out, n)
		}
		for _, kid := range n.kids {
			walk(kid)
		}
	}
	for _, root := range roots {
		walk(root)
	}
	result := map[string]any{"nodes": out, "count": len(out), "visited": visited, "coordinate_space": "screen-pixels"}
	if out == nil {
		result["nodes"] = []*a11yNode{}
	}
	if truncated != "" {
		result["truncated"] = truncated
	}
	return result, nil
}

func a11yMatches(n *a11yNode, args a11yArgs) bool {
	if args.Role != "" && lower(n.Role) != lower(args.Role) {
		return false
	}
	if args.Name != "" {
		want := lower(args.Name)
		return strings.Contains(lower(n.Name), want) || strings.Contains(lower(n.Text), want)
	}
	return true
}

// a11yContext names the element's application, window and nearest named
// ancestors, for telling search results apart.
func a11yContext(n *a11yNode) string {
	var names []string
	for p := n.parent; p != nil; p = p.parent {
		if p.Name != "" && (p.parent == nil || p.parent.parent == nil || len(names) < 2) {
			names = append(names, p.Name)
		}
	}
	slices.Reverse(names)
	return strings.Join(names, " > ")
}

func a11yAction(c *dbusConn, args a11yArgs) (map[string]any, error) {
	match := a11yRef.FindStringSubmatch(args.Ref)
	if match == nil {
		return nil, observeFail("invalid_request", "ref must come from accessibility_tree")
	}
	n := newA11yNode(match[1], match[2], 0, nil)
	describe(c, []*a11yNode{n}, false)
	if n.Role == "" {
		return nil, observeFail("invalid_request", "element %s is gone; read the tree again", args.Ref)
	}
	detail(c, []*a11yNode{n})
	var call dbusCall
	result := map[string]any{"ref": args.Ref, "action": args.Action}
	switch args.Action {
	case "do":
		if len(n.Actions) == 0 {
			return nil, observeFail("invalid_request", "%s %q has no actions", n.Role, n.Name)
		}
		index := -1
		for i, name := range n.Actions {
			if args.ActionName != "" && lower(name) == lower(args.ActionName) {
				index = i
			}
		}
		if args.ActionName != "" && index < 0 {
			return nil, observeFail("invalid_request", "no action %q; it has %s", args.ActionName, strings.Join(n.Actions, ", "))
		}
		if index < 0 {
			index = 0
			for _, preferred := range []string{"click", "activate", "press", "toggle", "jump"} {
				if i := slices.IndexFunc(n.Actions, func(name string) bool { return lower(name) == preferred }); i >= 0 {
					index = i
					break
				}
			}
		}
		result["action_name"] = n.Actions[index]
		call = n.call("org.a11y.atspi.Action", "DoAction", "i", int32(index))
	case "focus":
		if !n.has("Component") {
			return nil, observeFail("invalid_request", "%s %q cannot take focus", n.Role, n.Name)
		}
		call = n.call("org.a11y.atspi.Component", "GrabFocus", "")
	case "set_text":
		if args.Text == nil || !n.has("EditableText") {
			return nil, observeFail("invalid_request", "set_text needs text and an editable text element")
		}
		call = n.call("org.a11y.atspi.EditableText", "SetTextContents", "s", *args.Text)
	case "set_value":
		if args.Value == nil || !n.has("Value") {
			return nil, observeFail("invalid_request", "set_value needs value and an element with a value")
		}
		call = n.call(a11yProperties, "Set", "ssv", "org.a11y.atspi.Value", "CurrentValue", dbusVariant{"d", *args.Value})
	default:
		return nil, observeFail("invalid_request", "action must be do, focus, set_text or set_value")
	}
	values, err := c.call(call)
	if err != nil {
		return nil, observeFail("accessibility_action_failed", "%v", err)
	}
	if done, ok := firstValue(values).(bool); ok {
		result["done"] = done
	} else {
		result["done"] = true
	}
	// Report the element as the application left it.
	time.Sleep(100 * time.Millisecond)
	after := newA11yNode(n.bus, n.path, 0, nil)
	describe(c, []*a11yNode{after}, false)
	if after.Role != "" {
		detail(c, []*a11yNode{after})
		result["element_after"] = after
	} else {
		result["element_after"] = nil
	}
	return result, nil
}

// runA11y does the work as the current user.
func runA11y(job a11yJob) (map[string]any, error) {
	deadline := time.Now().Add(a11yTreeDeadline)
	if job.Method == "accessibility_action" {
		deadline = time.Now().Add(a11yActionDeadline)
	}
	c, enabledNow, err := a11yBus(job, deadline)
	if err != nil {
		return nil, err
	}
	defer c.close()
	var result map[string]any
	if job.Method == "accessibility_action" {
		result, err = a11yAction(c, job.Args)
	} else {
		result, err = a11yTree(c, job.Args)
	}
	if err == nil && enabledNow {
		result["accessibility_enabled_now"] = true
		result["note"] = "accessibility was just turned on for the session; applications that were already running without it (Qt, Chromium) appear once restarted"
	}
	return result, err
}

// a11yHelperMain runs one job read from stdin as the user it was started as.
func a11yHelperMain() {
	var job a11yJob
	var reply struct {
		Result any    `json:"result,omitempty"`
		Code   string `json:"code,omitempty"`
		Error  string `json:"error,omitempty"`
	}
	data, err := io.ReadAll(io.LimitReader(os.Stdin, 1<<20))
	if err == nil {
		err = json.Unmarshal(data, &job)
	}
	if err == nil {
		reply.Result, err = runA11y(job)
	}
	if err != nil {
		reply.Code, reply.Error = "accessibility_failed", err.Error()
		var coded *observeError
		if errors.As(err, &coded) {
			reply.Code = coded.code
		}
	}
	_ = json.NewEncoder(os.Stdout).Encode(reply)
}

func (d desktopEnv) accessibility(method string, args a11yArgs) (any, error) {
	session, err := d.session()
	if err != nil {
		return nil, observeFail("desktop_unavailable", "%v", err)
	}
	job := a11yJob{Method: method, Root: d.root, Env: session.env, Args: args}
	if session.uid == uint32(os.Geteuid()) {
		return runA11y(job)
	}
	input, err := json.Marshal(job)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), a11yTreeDeadline+2*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "/proc/self/exe", a11yHelperArg)
	cmd.Env, cmd.Dir = []string{}, "/"
	cmd.Stdin = bytes.NewReader(input)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Credential: &syscall.Credential{Uid: session.uid, Gid: session.gid, Groups: []uint32{}}}
	if err := cmd.Start(); err != nil {
		return nil, observeFail("accessibility_failed", "accessibility helper: %v", err)
	}
	text, readErr := io.ReadAll(io.LimitReader(stdout, a11yMaxReply+1))
	if err := cmd.Wait(); err != nil || readErr != nil {
		return nil, observeFail("accessibility_failed", "accessibility helper: %v", errors.Join(err, readErr))
	}
	if len(text) > a11yMaxReply {
		return nil, observeFail("accessibility_failed", "accessibility reply exceeds bound")
	}
	var reply struct {
		Result json.RawMessage `json:"result"`
		Code   string          `json:"code"`
		Error  string          `json:"error"`
	}
	if err := json.Unmarshal(text, &reply); err != nil {
		return nil, observeFail("accessibility_failed", "accessibility helper reply: %v", err)
	}
	if reply.Error != "" {
		return nil, observeFail(reply.Code, "%s", reply.Error)
	}
	return reply.Result, nil
}
