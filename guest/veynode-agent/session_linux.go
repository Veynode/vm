//go:build linux

// Session server (protocol in session.go). container-init starts it as a
// separate process ("session-server") so its children are reaped here, not
// by PID 1.
package main

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"sync"
	"syscall"
	"time"
	"unsafe"
)

const sessionScrollback = 64 << 10

// mainTerminal is the entrypoint's pty master (`run -t`) or the write end of
// its stdin pipe (`run -i`), handed over by container-init as fd 3.
type mainTerminal struct {
	file *os.File
	tty  bool
	mu   sync.Mutex
	// Recent output, replayed to each new attach so early output and the
	// current screen are not lost.
	scrollback []byte
	clients    map[*sessionConn]struct{}
	stdinOpen  bool
}

// openPTY allocates a pair in the container's devpts instance. The slave path
// is returned as the container sees it.
func openPTY(root string) (*os.File, *os.File, error) {
	master, err := os.OpenFile(filepath.Join(root, "dev/pts/ptmx"), os.O_RDWR|syscall.O_NOCTTY, 0)
	if err != nil {
		return nil, nil, err
	}
	unlock := 0
	if err = ioctl(master.Fd(), syscall.TIOCSPTLCK, uintptr(unsafe.Pointer(&unlock))); err != nil {
		master.Close()
		return nil, nil, err
	}
	var number uint32
	if err = ioctl(master.Fd(), syscall.TIOCGPTN, uintptr(unsafe.Pointer(&number))); err != nil {
		master.Close()
		return nil, nil, err
	}
	slave, err := os.OpenFile(filepath.Join(root, "dev/pts", strconv.Itoa(int(number))), os.O_RDWR|syscall.O_NOCTTY, 0)
	if err != nil {
		master.Close()
		return nil, nil, err
	}
	return master, slave, nil
}

func ioctl(fd, request, arg uintptr) error {
	if _, _, errno := syscall.Syscall(syscall.SYS_IOCTL, fd, request, arg); errno != 0 {
		return errno
	}
	return nil
}

func setWindowSize(f *os.File, rows, cols uint16) {
	if rows == 0 || cols == 0 {
		return
	}
	size := [4]uint16{rows, cols, 0, 0}
	_ = ioctl(f.Fd(), syscall.TIOCSWINSZ, uintptr(unsafe.Pointer(&size)))
}

func sessionServerMain(args []string) int {
	if len(args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: session-server CONTAINER_ROOT ENV_FILE")
		return 2
	}
	root, envFile := args[0], args[1]
	token := os.Getenv("VEYNODEVM_SESSION_TOKEN")
	_ = os.Unsetenv("VEYNODEVM_SESSION_TOKEN")
	if len(token) < 32 {
		fmt.Fprintln(os.Stderr, "session-server: no session token")
		return 2
	}
	var primary *mainTerminal
	if mode := os.Getenv("VEYNODEVM_MAIN_STDIO"); mode != "" {
		primary = &mainTerminal{file: os.NewFile(3, "main"), tty: mode == "tty", clients: map[*sessionConn]struct{}{}, stdinOpen: true}
		if primary.tty {
			go primary.pump()
		}
	}
	listener, err := net.Listen("tcp", fmt.Sprintf(":%d", sessionPort))
	if err != nil {
		fmt.Fprintln(os.Stderr, "session-server:", err)
		return 1
	}
	for {
		conn, err := listener.Accept()
		if err != nil {
			continue
		}
		go serveSession(conn, root, envFile, token, primary)
	}
}

// pump copies the entrypoint's terminal output to the console (for logs) and
// to every attached client.
func (m *mainTerminal) pump() {
	buffer := make([]byte, 32<<10)
	for {
		n, err := m.file.Read(buffer)
		if n > 0 {
			chunk := append([]byte(nil), buffer[:n]...)
			_, _ = os.Stdout.Write(chunk)
			m.mu.Lock()
			m.scrollback = append(m.scrollback, chunk...)
			if len(m.scrollback) > sessionScrollback {
				m.scrollback = append([]byte(nil), m.scrollback[len(m.scrollback)-sessionScrollback:]...)
			}
			for client := range m.clients {
				if client.send('o', chunk) != nil {
					delete(m.clients, client)
					client.conn.Close()
				}
			}
			m.mu.Unlock()
		}
		if err != nil {
			// EIO once the entrypoint and everything holding the slave exit.
			m.mu.Lock()
			for client := range m.clients {
				client.conn.Close()
			}
			m.clients = map[*sessionConn]struct{}{}
			m.mu.Unlock()
			return
		}
	}
}

func serveSession(conn net.Conn, root, envFile, token string, primary *mainTerminal) {
	defer conn.Close()
	reader := bufio.NewReaderSize(conn, 64<<10)
	line, err := reader.ReadSlice('\n')
	if err != nil {
		return
	}
	req, ok := authorizeSession(line, token)
	if !ok {
		_, _ = conn.Write([]byte(`{"error":"unauthorized"}` + "\n"))
		return
	}
	client := &sessionConn{conn: conn}
	reply := func(err error) bool {
		if err != nil {
			text, _ := json.Marshal(map[string]string{"error": err.Error()})
			_, _ = conn.Write(append(text, '\n'))
			return false
		}
		_, _ = conn.Write([]byte(`{"ok":true}` + "\n"))
		return true
	}
	switch req.Op {
	case "attach":
		if primary == nil {
			reply(errors.New("the container was not started with -i or -t"))
			return
		}
		if !reply(nil) {
			return
		}
		primary.attach(client, reader, req)
	case "exec":
		cmd, terminal, err := startSession(root, envFile, req)
		if !reply(err) {
			return
		}
		runSession(client, reader, cmd, terminal, req)
	default:
		reply(fmt.Errorf("unknown op %q", req.Op))
	}
}

func (m *mainTerminal) attach(client *sessionConn, reader io.Reader, req sessionRequest) {
	if m.tty {
		setWindowSize(m.file, req.Rows, req.Cols)
		m.mu.Lock()
		if len(m.scrollback) > 0 && client.send('o', m.scrollback) != nil {
			m.mu.Unlock()
			return
		}
		m.clients[client] = struct{}{}
		m.mu.Unlock()
		defer func() {
			m.mu.Lock()
			delete(m.clients, client)
			m.mu.Unlock()
		}()
	}
	for {
		kind, payload, err := readFrame(reader)
		if err != nil {
			return
		}
		switch kind {
		case 'i':
			if _, err = m.file.Write(payload); err != nil && !m.tty {
				return
			}
		case 'e':
			// A pty has no EOF; for a pipe, the first client's EOF closes stdin.
			if !m.tty {
				m.mu.Lock()
				if m.stdinOpen {
					m.stdinOpen = false
					m.file.Close()
				}
				m.mu.Unlock()
			}
		case 'r':
			if m.tty && len(payload) == 4 {
				setWindowSize(m.file, binary.BigEndian.Uint16(payload), binary.BigEndian.Uint16(payload[2:]))
			}
		}
	}
}

// startSession starts an exec session's process with the container's
// environment, like `docker exec`: the image env, then the request's.
func startSession(root, envFile string, req sessionRequest) (*exec.Cmd, *os.File, error) {
	if len(req.Argv) == 0 {
		return nil, nil, errors.New("no command")
	}
	identity, err := loadContainerIdentity(root, envFile)
	if err != nil {
		return nil, nil, err
	}
	env := append([]string(nil), identity.env...)
	for _, entry := range req.Env {
		key, _, _ := cutEnv(entry)
		filtered := env[:0]
		for _, existing := range env {
			if k, _, _ := cutEnv(existing); k != key {
				filtered = append(filtered, existing)
			}
		}
		env = append(filtered, entry)
	}
	identity.env = env
	if req.Workdir != "" {
		identity.CWD = req.Workdir
	}
	attributes := &syscall.SysProcAttr{Chroot: root, Setsid: true}
	if req.User != "" {
		uid, gid, home, err := lookupUser(root, req.User)
		if err != nil {
			return nil, nil, err
		}
		attributes.Credential = &syscall.Credential{Uid: uid, Gid: gid}
		identity.env = withDefaultEnv(identity.env, home)
	}
	executable, err := identity.executable(req.Argv[0])
	if err != nil {
		return nil, nil, fmt.Errorf("%s: %w", req.Argv[0], err)
	}
	cmd := exec.Command(executable, req.Argv[1:]...)
	cmd.Env = identity.env
	cmd.Dir = identity.CWD
	cmd.SysProcAttr = attributes
	if !req.TTY {
		return cmd, nil, nil
	}
	master, slave, err := openPTY(root)
	if err != nil {
		return nil, nil, fmt.Errorf("pty: %w", err)
	}
	setWindowSize(master, req.Rows, req.Cols)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = slave, slave, slave
	attributes.Setctty = true
	attributes.Ctty = 0
	if err = cmd.Start(); err != nil {
		master.Close()
		slave.Close()
		return nil, nil, err
	}
	slave.Close()
	return cmd, master, nil
}

func cutEnv(entry string) (string, string, bool) {
	for i := 0; i < len(entry); i++ {
		if entry[i] == '=' {
			return entry[:i], entry[i+1:], true
		}
	}
	return entry, "", false
}

func runSession(client *sessionConn, reader io.Reader, cmd *exec.Cmd, terminal *os.File, req sessionRequest) {
	var stdin io.WriteCloser
	var outputs sync.WaitGroup
	copyOut := func(kind byte, from io.Reader) {
		defer outputs.Done()
		buffer := make([]byte, 32<<10)
		for {
			n, err := from.Read(buffer)
			if n > 0 && client.send(kind, buffer[:n]) != nil {
				return
			}
			if err != nil {
				return
			}
		}
	}
	if terminal != nil {
		defer terminal.Close()
		stdin = terminal
		outputs.Add(1)
		go copyOut('o', terminal)
	} else {
		var err error
		if req.Stdin {
			if stdin, err = cmd.StdinPipe(); err != nil {
				_ = client.send('q', []byte("126"))
				return
			}
		}
		stdout, errOut := cmd.StdoutPipe()
		stderr, errErr := cmd.StderrPipe()
		if errOut != nil || errErr != nil {
			_ = client.send('q', []byte("126"))
			return
		}
		if err = cmd.Start(); err != nil {
			_ = client.send('x', []byte(err.Error()+"\n"))
			_ = client.send('q', []byte("127"))
			return
		}
		outputs.Add(2)
		go copyOut('o', stdout)
		go copyOut('x', stderr)
	}
	go func() {
		// Input until the client leaves; a closed connection hangs up the
		// session like a dropped SSH connection.
		for {
			kind, payload, err := readFrame(reader)
			if err != nil {
				if cmd.Process != nil {
					_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGHUP)
				}
				return
			}
			switch kind {
			case 'i':
				if stdin != nil {
					_, _ = stdin.Write(payload)
				}
			case 'e':
				if stdin != nil && terminal == nil {
					stdin.Close()
					stdin = nil
				}
			case 'r':
				if terminal != nil && len(payload) == 4 {
					setWindowSize(terminal, binary.BigEndian.Uint16(payload), binary.BigEndian.Uint16(payload[2:]))
				}
			}
		}
	}()
	if terminal == nil {
		// Pipes reach EOF when the process and its descendants close them.
		outputs.Wait()
	}
	err := cmd.Wait()
	if terminal != nil {
		// The pty master reads EIO once no process holds the slave. Drain
		// what the process wrote last; a background process still holding
		// the slave is cut off after a grace period.
		drained := make(chan struct{})
		go func() { outputs.Wait(); close(drained) }()
		select {
		case <-drained:
		case <-time.After(2 * time.Second):
			terminal.Close()
			<-drained
		}
	}
	code := 0
	if err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) {
			if status, ok := exit.Sys().(syscall.WaitStatus); ok && status.Signaled() {
				code = 128 + int(status.Signal())
			} else {
				code = exit.ExitCode()
			}
		} else {
			code = 126
		}
	}
	_ = client.send('q', []byte(strconv.Itoa(code)))
	_ = client.conn.(*net.TCPConn).CloseWrite()
}
