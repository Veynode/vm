package main

// cua-driver (trycua/cua, MIT) as the Guest's semantic computer-use helper
// (docs/computer-use.md): a stdio MCP server started on first use as the
// desktop session's user, kept running, and bridged to the host one JSON-RPC
// frame at a time. On Apple Virtualization the frame travels in the vsock
// method driver_rpc; on QEMU, newline frames on the dedicated virtio port
// veynode.driver (serveDriverPort), outside the fenced serial exec protocol.
//
// The supervisor owns the MCP handshake, so the host only sends tools/*,
// resources/* and ping. Request ids are rewritten to the supervisor's own and
// restored on the reply, so concurrent callers never see each other's ids.
// The driver never runs as root, sends no telemetry, and shows no overlay.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

const (
	driverPath = "/usr/local/bin/cua-driver"
	driverPort = "/dev/virtio-ports/veynode.driver"
	// Frames either way; the host's reply limit is 1 MiB with the envelope.
	maxDriverFrame    = 1 << 20
	maxDriverReply    = maxDriverFrame - 4096
	maxDriverRequest  = maxDriverFrame + 4096
	maxDriverDeadline = 10 * time.Minute
	driverStartWait   = 15 * time.Second
	driverStderrTail  = 4096
	driverMaxInFlight = 16
	// Crash loop guard: more starts than this within the window are refused.
	driverMaxStarts   = 5
	driverStartWindow = time.Minute
)

// The driver's opt-outs; set on every start, so not even a first-run event
// leaves the Guest.
var driverEnv = []string{"DO_NOT_TRACK=1", "CUA_TELEMETRY=0", "CUA_DRIVER_RS_TELEMETRY_ENABLED=0", "CUA_DRIVER_RS_UPDATE_CHECK=0"}

// The host may send only these; the handshake is the supervisor's.
var driverMethods = []string{"tools/list", "tools/call", "resources/list", "resources/read", "ping"}

type driverError struct{ code, message string }

func (e *driverError) Error() string { return e.message }

func driverFail(code, format string, args ...any) error {
	return &driverError{code, fmt.Sprintf(format, args...)}
}

type driverArgs struct {
	Frame      json.RawMessage `json:"frame"`
	DeadlineMS int             `json:"deadline_ms"`
}

// driverFrame is the part of a JSON-RPC message the supervisor reads.
type driverFrame struct {
	ID     json.RawMessage `json:"id,omitempty"`
	Method string          `json:"method,omitempty"`
}

// driverProcess is one running driver and its pending calls.
type driverProcess struct {
	stdin   io.WriteCloser
	cmd     *exec.Cmd
	session int // the desktop session process it was started for
	writeMu sync.Mutex
	mu      sync.Mutex
	pending map[string]chan []byte
	next    uint64
	done    chan struct{}
	stderr  *tailBuffer
	// Notifications the driver sent; dropped until events carry them.
	notifications atomic.Int64
}

type driverSupervisor struct {
	mu     sync.Mutex
	proc   *driverProcess
	starts []time.Time
	// start launches the driver; tests replace it with a fake.
	start func(root string) (*driverProcess, error)
	root  func() string
}

var drivers = &driverSupervisor{start: startDriver, root: func() string {
	if commands.identity.Chroot != "" {
		return commands.identity.Chroot
	}
	return "/"
}}

// call sends one host request frame and returns the driver's reply frame.
func (s *driverSupervisor) call(frame []byte, deadline time.Duration) ([]byte, error) {
	if len(frame) > maxDriverFrame {
		return nil, driverFail("invalid_request", "frame exceeds 1 MiB")
	}
	var head driverFrame
	if json.Unmarshal(frame, &head) != nil || bytes.HasPrefix(bytes.TrimSpace(frame), []byte("[")) {
		return nil, driverFail("invalid_request", "frame must be one JSON-RPC request object")
	}
	if len(head.ID) == 0 || string(head.ID) == "null" {
		return nil, driverFail("invalid_request", "frame needs an id; notifications are not forwarded")
	}
	if !slices.Contains(driverMethods, head.Method) {
		return nil, driverFail("invalid_request", "method %q is not forwarded (allowed: %s)", head.Method, strings.Join(driverMethods, ", "))
	}
	if deadline <= 0 || deadline > maxDriverDeadline {
		deadline = maxDriverDeadline
	}
	proc, err := s.running()
	if err != nil {
		return nil, err
	}
	reply, err := proc.request(frame, deadline)
	if err != nil {
		return nil, err
	}
	// The caller's id goes back on the reply.
	return withID(reply, head.ID)
}

// running returns the live driver, starting one if none runs or the desktop
// session it was started for has gone.
func (s *driverSupervisor) running() (*driverProcess, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.proc != nil {
		select {
		case <-s.proc.done:
			s.proc = nil
		default:
			if s.proc.session != 0 {
				if _, err := os.Stat("/proc/" + strconv.Itoa(s.proc.session)); err != nil {
					s.proc.stop()
					s.proc = nil
				}
			}
		}
	}
	if s.proc != nil {
		return s.proc, nil
	}
	now := time.Now()
	s.starts = slices.DeleteFunc(s.starts, func(t time.Time) bool { return now.Sub(t) > driverStartWindow })
	if len(s.starts) >= driverMaxStarts {
		return nil, driverFail("driver_unavailable", "cua-driver exited %d times within a minute; not restarting yet", len(s.starts))
	}
	s.starts = append(s.starts, now)
	proc, err := s.start(s.root())
	if err != nil {
		return nil, err
	}
	if err = proc.initialize(); err != nil {
		proc.stop()
		return nil, err
	}
	s.proc = proc
	return proc, nil
}

func startDriver(root string) (*driverProcess, error) {
	env := desktopEnv{root: root}
	session, err := env.session()
	if err != nil {
		return nil, driverFail("unsupported", "cua-driver needs a running desktop session: %v", err)
	}
	if session.uid == 0 {
		return nil, driverFail("unsupported", "the desktop session runs as root; cua-driver runs only as a desktop user")
	}
	identity := executionIdentity{Chroot: root, env: session.env}
	path, err := identity.executable(driverPath)
	if err != nil {
		return nil, driverFail("unsupported", "cua-driver not in this image (%s)", driverPath)
	}
	cmd := exec.Command(path, "mcp", "--no-overlay")
	cmd.Env = append(slices.Clone(session.env), driverEnv...)
	cmd.Dir = "/"
	if home := session.getenv("HOME"); home != "" {
		if info, err := os.Stat(env.path(home)); err == nil && info.IsDir() {
			cmd.Dir = home
		}
	}
	// The session user's own groups: input for uinput, video and render.
	credential := &syscall.Credential{Uid: session.uid, Gid: session.gid, Groups: processGroups("/proc/" + strconv.Itoa(session.pid) + "/status")}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Credential: credential}
	if root != "/" {
		cmd.SysProcAttr.Chroot = root
	}
	proc, err := launchDriver(cmd)
	if err != nil {
		return nil, driverFail("driver_unavailable", "start cua-driver: %v", err)
	}
	proc.session = session.pid
	return proc, nil
}

// launchDriver starts cmd and reads its replies. The driver is not in any
// command's cgroup, so exec timeouts never take it, or the apps it launched,
// with them.
func launchDriver(cmd *exec.Cmd) (*driverProcess, error) {
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	proc := &driverProcess{stdin: stdin, cmd: cmd, pending: map[string]chan []byte{}, done: make(chan struct{}), stderr: &tailBuffer{max: driverStderrTail}}
	cmd.Stderr = proc.stderr
	if err = cmd.Start(); err != nil {
		return nil, err
	}
	go proc.read(stdout)
	return proc, nil
}

func (p *driverProcess) read(stdout io.Reader) {
	defer func() {
		_ = p.cmd.Wait()
		p.mu.Lock()
		close(p.done)
		for id, waiter := range p.pending {
			close(waiter)
			delete(p.pending, id)
		}
		p.mu.Unlock()
	}()
	reader := bufio.NewReaderSize(stdout, 64<<10)
	for {
		line, err := readLine(reader, 4*maxDriverFrame)
		if err != nil {
			return
		}
		var head driverFrame
		if len(line) == 0 || json.Unmarshal(line, &head) != nil {
			continue
		}
		if head.Method != "" {
			if len(head.ID) > 0 {
				// A request from the driver (roots, sampling): the host has none to give.
				p.write(fmt.Appendf(nil, `{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"not supported by this client"}}`, head.ID))
			} else {
				p.notifications.Add(1)
			}
			continue
		}
		p.mu.Lock()
		waiter := p.pending[string(head.ID)]
		delete(p.pending, string(head.ID))
		p.mu.Unlock()
		if waiter != nil {
			waiter <- line
		}
	}
}

// readLine reads one newline-terminated line. A longer line is consumed and
// returned empty, so one oversized message cannot stop the reader; a partial
// line at EOF is dropped.
func readLine(reader *bufio.Reader, limit int) ([]byte, error) {
	var line []byte
	over := false
	for {
		chunk, err := reader.ReadSlice('\n')
		if !over && len(line)+len(chunk) > limit {
			over, line = true, nil
		} else if !over {
			line = append(line, chunk...)
		}
		if err == bufio.ErrBufferFull {
			continue
		}
		if err != nil {
			return nil, err
		}
		if over {
			return []byte{}, nil
		}
		return bytes.TrimRight(line, "\r\n"), nil
	}
}

func (p *driverProcess) write(frame []byte) error {
	p.writeMu.Lock()
	defer p.writeMu.Unlock()
	_, err := p.stdin.Write(append(slices.Clone(frame), '\n'))
	return err
}

// request sends frame under a fresh id and waits for the reply to it.
func (p *driverProcess) request(frame []byte, deadline time.Duration) ([]byte, error) {
	p.mu.Lock()
	select {
	case <-p.done:
		p.mu.Unlock()
		return nil, p.exited()
	default:
	}
	p.next++
	id := `"veynode-` + strconv.FormatUint(p.next, 10) + `"`
	waiter := make(chan []byte, 1)
	p.pending[id] = waiter
	p.mu.Unlock()
	forget := func() {
		p.mu.Lock()
		delete(p.pending, id)
		p.mu.Unlock()
	}
	framed, err := withID(frame, json.RawMessage(id))
	if err != nil {
		forget()
		return nil, err
	}
	if err = p.write(framed); err != nil {
		forget()
		return nil, p.exited()
	}
	timer := time.NewTimer(deadline)
	defer timer.Stop()
	select {
	case reply, ok := <-waiter:
		if !ok {
			return nil, p.exited()
		}
		if len(reply) > maxDriverReply {
			return nil, driverFail("reply_too_large", "cua-driver reply of %d bytes exceeds 1 MiB", len(reply))
		}
		return reply, nil
	case <-timer.C:
		// A late reply finds no waiter and is dropped; the driver keeps running.
		forget()
		return nil, driverFail("deadline_exceeded", "cua-driver did not answer within %s", deadline)
	}
}

func (p *driverProcess) exited() error {
	tail := strings.TrimSpace(p.stderr.String())
	if tail != "" {
		return driverFail("driver_exited", "cua-driver exited: %s", tail)
	}
	return driverFail("driver_exited", "cua-driver exited")
}

// initialize performs the MCP handshake once per driver process.
func (p *driverProcess) initialize() error {
	reply, err := p.request([]byte(`{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"veynode-agent","version":"1"}}}`), driverStartWait)
	if err != nil {
		return err
	}
	var answer struct {
		Error *struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(reply, &answer) != nil || answer.Error != nil {
		return driverFail("driver_unavailable", "cua-driver refused initialize: %s", reply)
	}
	return p.write([]byte(`{"jsonrpc":"2.0","method":"notifications/initialized"}`))
}

func (p *driverProcess) stop() {
	_ = p.stdin.Close()
	if p.cmd.Process != nil {
		// The driver leads its own session; its group is the apps it launched.
		_ = syscall.Kill(-p.cmd.Process.Pid, syscall.SIGTERM)
		_ = p.cmd.Process.Signal(syscall.SIGTERM)
	}
}

// withID replaces the top-level id of a JSON-RPC object.
func withID(frame []byte, id json.RawMessage) ([]byte, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(frame, &fields); err != nil {
		return nil, driverFail("invalid_reply", "cua-driver sent a frame that is not a JSON object")
	}
	fields["id"] = id
	return json.Marshal(fields)
}

func processGroups(status string) []uint32 {
	data, err := os.ReadFile(status)
	if err != nil {
		return []uint32{}
	}
	groups := []uint32{}
	for _, line := range strings.Split(string(data), "\n") {
		if rest, ok := strings.CutPrefix(line, "Groups:"); ok {
			for _, field := range strings.Fields(rest) {
				if gid, err := strconv.ParseUint(field, 10, 32); err == nil {
					groups = append(groups, uint32(gid))
				}
			}
		}
	}
	return groups
}

// tailBuffer keeps the last bytes written (the driver's stderr).
type tailBuffer struct {
	mu  sync.Mutex
	max int
	b   []byte
}

func (t *tailBuffer) Write(p []byte) (int, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.b = append(t.b, p...)
	if len(t.b) > t.max {
		t.b = slices.Clone(t.b[len(t.b)-t.max:])
	}
	return len(p), nil
}

func (t *tailBuffer) String() string {
	t.mu.Lock()
	defer t.mu.Unlock()
	return string(t.b)
}

// driverCallDeadline is the deadline a driver_rpc request asks for, bounded.
func driverCallDeadline(req request) time.Duration {
	var args driverArgs
	_ = json.Unmarshal(req.Arguments, &args)
	deadline := time.Duration(args.DeadlineMS) * time.Millisecond
	if deadline <= 0 || deadline > maxDriverDeadline {
		return maxDriverDeadline
	}
	return deadline
}

// handleDriverRPC answers the vsock method driver_rpc.
func handleDriverRPC(req request) response {
	var args driverArgs
	if len(req.Arguments) == 0 || json.Unmarshal(req.Arguments, &args) != nil || len(args.Frame) == 0 {
		return failure(req.ID, "invalid_request", "driver_rpc needs arguments {frame, deadline_ms}")
	}
	if args.DeadlineMS < 0 || time.Duration(args.DeadlineMS)*time.Millisecond > maxDriverDeadline {
		return failure(req.ID, "invalid_request", "deadline_ms must be 0..600000")
	}
	reply, err := drivers.call(args.Frame, time.Duration(args.DeadlineMS)*time.Millisecond)
	if err != nil {
		var coded *driverError
		if errors.As(err, &coded) {
			return failure(req.ID, coded.code, coded.message)
		}
		return failure(req.ID, "driver_failed", err.Error())
	}
	return response{ID: req.ID, Result: map[string]json.RawMessage{"frame": reply}}
}

// serveDriverFrames serves newline JSON-RPC frames from the host (the QEMU
// port): each request is answered when the driver answers it, in any order;
// a failure is a JSON-RPC error reply with the request's id.
func serveDriverFrames(port io.ReadWriter) error {
	var writeMu sync.Mutex
	reply := func(frame []byte) {
		writeMu.Lock()
		defer writeMu.Unlock()
		_, _ = port.Write(append(frame, '\n'))
	}
	fail := func(line []byte, code string, err error) {
		var head driverFrame
		_ = json.Unmarshal(line, &head)
		if len(head.ID) == 0 {
			return
		}
		message, _ := json.Marshal(err.Error())
		reply(fmt.Appendf(nil, `{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":%s,"data":{"veynode_code":%q}}}`, head.ID, message, code))
	}
	slots := make(chan struct{}, driverMaxInFlight)
	reader := bufio.NewReaderSize(port, 64<<10)
	for {
		line, err := readLine(reader, maxDriverFrame)
		if err != nil {
			return err
		}
		if len(line) == 0 {
			continue
		}
		select {
		case slots <- struct{}{}:
		default:
			fail(line, "busy", fmt.Errorf("%d driver calls in flight", driverMaxInFlight))
			continue
		}
		go func() {
			defer func() { <-slots }()
			answer, err := drivers.call(line, maxDriverDeadline)
			if err == nil {
				reply(answer)
				return
			}
			code := "driver_failed"
			var coded *driverError
			if errors.As(err, &coded) {
				code = coded.code
			}
			fail(line, code, err)
		}()
	}
}
