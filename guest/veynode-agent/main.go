// Guest integration service. The vsock transport is restricted to host CID 2.
// No TCP listener; execution is opt-in. This is a headless Linux service, not a desktop agent.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"sync"
	"syscall"
	"time"
)

const maxRequest = 65536
const maxOutput = 65536

type request struct {
	ID        string   `json:"id"`
	Method    string   `json:"method"`
	Argv      []string `json:"argv,omitempty"`
	TimeoutMS int      `json:"timeout_ms,omitempty"`
	JobID     string   `json:"job_id,omitempty"`
	Owner     string   `json:"owner,omitempty"`
	Path      string   `json:"path,omitempty"`
	Token     string   `json:"token,omitempty"`
	Data      string   `json:"data,omitempty"`
	Size      int64    `json:"size,omitempty"`
	Offset    int64    `json:"offset,omitempty"`
	SHA256    string   `json:"sha256,omitempty"`
	TextHex   string   `json:"text_hex,omitempty"`
	UnixNS    int64    `json:"unix_ns,omitempty"`
	// User, Env and Workdir are `exec -u -e -w` for container commands.
	User    string   `json:"user,omitempty"`
	Env     []string `json:"env,omitempty"`
	Workdir string   `json:"workdir,omitempty"`
	// Arguments carries the daemon's desktop input and wait arguments (observe.go).
	Arguments json.RawMessage `json:"arguments,omitempty"`
}
type response struct {
	ID     string    `json:"id"`
	Result any       `json:"result,omitempty"`
	Error  *rpcError `json:"error,omitempty"`
}
type rpcError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}
type limitedBuffer struct {
	sync.Mutex
	b         bytes.Buffer
	truncated bool
}

func (b *limitedBuffer) Write(p []byte) (int, error) {
	b.Lock()
	defer b.Unlock()
	n := len(p)
	remaining := maxOutput - b.b.Len()
	if n > remaining {
		b.truncated = true
		p = p[:remaining]
	}
	b.b.Write(p)
	return n, nil
}
func (b *limitedBuffer) result() (string, bool) {
	b.Lock()
	defer b.Unlock()
	return b.b.String(), b.truncated
}
func failure(id, code, message string) response {
	return response{ID: id, Error: &rpcError{code, message}}
}

var commands = newJobManager(time.Now)

func handle(req request, allowExec bool, allowShutdown ...bool) response {
	return handleWithManager(commands, req, allowExec, allowShutdown...)
}
func handleWithManager(manager *jobManager, req request, allowExec bool, allowShutdown ...bool) response {
	shutdownEnabled := len(allowShutdown) > 0 && allowShutdown[0] && runtime.GOOS == "linux" && os.Geteuid() == 0
	if req.ID == "" || len(req.ID) > 128 {
		return failure(req.ID, "invalid_request", "id must be 1..128 bytes")
	}
	switch req.Method {
	case "info":
		release, _ := os.ReadFile("/etc/os-release")
		var filePolicy any
		if files != nil {
			reply := files.call(request{ID: req.ID + "-files", Method: "file_worker_info"})
			if reply.Error != nil {
				filePolicy = map[string]any{"error": reply.Error}
			} else {
				filePolicy = reply.Result
			}
		}
		textAvailable, textStatus := textInfo()
		return response{ID: req.ID, Result: map[string]any{"text_input": textStatus, "protocol_version": 1, "os": runtime.GOOS, "arch": runtime.GOARCH, "os_release": string(release), "transport": "vsock-host-only", "execution_identity": manager.identity, "execution_containment": manager.containmentMode, "service_uid": os.Geteuid(), "file_policy": filePolicy, "capabilities": map[string]bool{"exec": allowExec, "async_exec": allowExec, "descendant_containment": allowExec && manager.containmentMode == "cgroup-v2-clone3-kill", "shutdown": shutdownEnabled, "desktop": desktop != nil, "screenshot": desktop != nil, "input": desktop != nil, "screen_wait": desktop != nil, "desktop_windows": desktop != nil, "app_launch": desktop != nil, "accessibility": desktop != nil, "files": files != nil, "text_input": textAvailable}, "max_output_bytes_per_stream": maxOutput, "max_timeout_ms": maxCommandTimeoutMS, "max_async_timeout_ms": manager.maxAsyncTimeoutMS(), "max_active_commands": maxActiveCommands, "max_finished_jobs": maxFinishedJobs, "finished_job_ttl_seconds": int(finishedJobTTL.Seconds())}}
	case "file_upload_begin", "file_upload_write", "file_upload_commit", "file_download_begin", "file_download_read", "file_cancel", "file_session_end":
		if files == nil {
			return failure(req.ID, "unsupported", "file integration disabled; requires Linux files worker and explicit directory")
		}
		return files.call(req)
	case "screenshot", "screen_wait", "input_step", "input_cleanup":
		if desktop == nil {
			return failure(req.ID, "unsupported", "desktop observation requires a desktop container (--desktop-stream)")
		}
		return desktop.handle(req)
	case "desktop_state", "desktop_apps", "window_action", "app_launch":
		if desktop == nil {
			return failure(req.ID, "unsupported", "desktop tools require a desktop container (--desktop-stream)")
		}
		root := manager.identity.Chroot
		if root == "" {
			root = "/"
		}
		return desktopEnv{root: root}.handle(req)
	case "accessibility_tree", "accessibility_action":
		if desktop == nil {
			return failure(req.ID, "unsupported", "accessibility requires a desktop container (--desktop-stream)")
		}
		var args a11yArgs
		if len(req.Arguments) > 0 && json.Unmarshal(req.Arguments, &args) != nil {
			return failure(req.ID, "invalid_request", "invalid arguments")
		}
		root := manager.identity.Chroot
		if root == "" {
			root = "/"
		}
		result, err := desktopEnv{root: root}.accessibility(req.Method, args)
		if err != nil {
			var coded *observeError
			if errors.As(err, &coded) {
				return failure(req.ID, coded.code, coded.message)
			}
			return failure(req.ID, "accessibility_failed", err.Error())
		}
		return response{ID: req.ID, Result: result}
	case "driver_rpc":
		// Independent of --desktop-stream: the driver needs a desktop session, which it finds itself.
		return handleDriverRPC(req)
	case "clock_sync":
		return clockSync(req)
	case "activity":
		return activity(req)
	case "text_input":
		// Independent of execute/shutdown/files permission; the host applies the input permission.
		return handleTextInput(req)
	case "shutdown":
		if !shutdownEnabled {
			return failure(req.ID, "forbidden", "normal shutdown disabled or service lacks privilege")
		}
		// The platform utility requests the installed init system's clean shutdown.
		// This does not use -f, syscall reboot, or report the VM as already stopped.
		cmd := exec.Command("/sbin/poweroff")
		if err := cmd.Start(); err != nil {
			return failure(req.ID, "shutdown_failed", err.Error())
		}
		go func() { _ = cmd.Wait() }()
		return response{ID: req.ID, Result: map[string]any{"requested": true, "mechanism": "guest-init-poweroff", "completion": "poll VM state"}}
	case "exec", "exec_start", "exec_poll", "exec_cancel":
		if !allowExec {
			return failure(req.ID, "forbidden", "command execution disabled at guest service launch")
		}
		switch req.Method {
		case "exec":
			return manager.execute(req)
		case "exec_start":
			return manager.start(req)
		case "exec_poll":
			return manager.poll(req)
		case "exec_cancel":
			return manager.cancel(req)
		}
		panic("unreachable command method")
	default:
		return failure(req.ID, "unsupported", "unsupported guest method")
	}
}
func serve(stream io.ReadWriteCloser, allowExec bool, allowShutdown ...bool) {
	defer stream.Close()
	if f, ok := stream.(*os.File); ok {
		_ = f.SetDeadline(time.Now().Add(35 * time.Second))
	}
	// driver_rpc alone may carry up to 1 MiB (a driver frame with user text).
	reader := bufio.NewReader(io.LimitReader(stream, maxDriverRequest+1))
	line, err := reader.ReadBytes('\n')
	var reply response
	if err != nil || len(line) > maxDriverRequest {
		reply = failure("", "invalid_request", "expected newline JSON within 64KiB")
	} else {
		var req request
		decoder := json.NewDecoder(bytes.NewReader(line))
		decoder.DisallowUnknownFields()
		if err = decoder.Decode(&req); err != nil {
			reply = failure("", "invalid_request", "invalid JSON request")
		} else if decoder.Decode(new(any)) != io.EOF {
			reply = failure(req.ID, "invalid_request", "trailing JSON")
		} else if len(line) > maxRequest && req.Method != "driver_rpc" {
			reply = failure(req.ID, "invalid_request", "expected newline JSON within 64KiB")
		} else {
			if f, ok := stream.(*os.File); ok && req.Method == "driver_rpc" {
				// The call's own deadline, plus time to write the reply.
				_ = f.SetDeadline(time.Now().Add(driverCallDeadline(req) + 5*time.Second))
			}
			reply = handle(req, allowExec, allowShutdown...)
		}
	}
	_ = json.NewEncoder(stream).Encode(reply)
}
func main() {
	if len(os.Args) == 2 && os.Args[1] == a11yHelperArg {
		a11yHelperMain()
		return
	}
	serialExec := flag.Bool("virtio-serial-exec", false, "private v2 rootless cgroup execution; requires host incarnation guardian, no reconnect")
	serial := flag.Bool("virtio-serial", false, "observer-only private fixed virtio port; no reconnect or mutating capabilities")
	legacyExec := flag.Bool("legacy-exec-process-groups", false, "DEVELOPMENT ONLY: allow uncontained process-group execution; setsid descendants can escape")
	allowExec := flag.Bool("allow-execute", false, "allow host-authorized execution inside guest")
	allowShutdown := flag.Bool("allow-shutdown", false, "allow host-authorized normal guest OS shutdown (requires root)")
	execUser := flag.String("exec-user", "", "run commands as this fixed non-root account; privileged service retains independent shutdown permission")
	allowFiles := flag.Bool("allow-files", false, "enable bounded files in explicit directory as dedicated non-root account")
	filesUser := flag.String("files-user", "", "fixed non-root file worker account")
	filesRoot := flag.String("files-root", "", "explicit file transfer directory")
	fileWorker := flag.Bool("file-worker", false, "internal non-root worker")
	containerRoot := flag.String("container-root", "", "OCI micro-VM (set by container-init): run commands as root inside this root")
	containerEnv := flag.String("container-env", "", "OCI micro-VM: JSON file with the image env and workdir")
	desktopStream := flag.Bool("desktop-stream", false, "serve the DRM scanout and uinput input to the host on vsock 5001 (Apple Virtualization desktop)")
	if len(os.Args) > 1 && os.Args[1] == "container-init" {
		os.Exit(containerInit())
	}
	if len(os.Args) > 1 && os.Args[1] == "session-server" {
		os.Exit(sessionServerMain(os.Args[2:]))
	}
	// `marker KIND VALUE` prints a host-visible console event; systemd units
	// of --init systemd containers use it (stdout is /dev/console there).
	if len(os.Args) == 4 && os.Args[1] == "marker" {
		fmt.Printf("@@veynodevm %s %s\n", os.Args[2], os.Args[3])
		return
	}
	allowText := flag.Bool("allow-text-input", false, "allow host-authorized Unicode text input into the focused X11 application (XTEST with temporary keycodes)")
	textDisplay := flag.String("text-input-display", ":0", "local X display for --allow-text-input")
	textAuthority := flag.String("text-input-xauthority", "", "Xauthority file holding the display's MIT-MAGIC-COOKIE-1; empty sends no authorization")
	textOnce := flag.String("text-input-once-hex", "", "DIAGNOSTIC: inject this lowercase-hex UTF-8 text once through the local display, print the service status and reply, then exit; opens no listener")
	flag.Parse()
	resolveIdentity := func() (executionIdentity, error) {
		if *containerRoot != "" {
			if *execUser != "" {
				return executionIdentity{}, fmt.Errorf("--container-root and --exec-user are exclusive")
			}
			return loadContainerIdentity(*containerRoot, *containerEnv)
		}
		return resolveExecutionIdentity(*execUser)
	}
	if *serial {
		if *desktopStream || *serialExec || *allowExec || *allowShutdown || *allowFiles || *legacyExec || *execUser != "" || *filesUser != "" || *filesRoot != "" || *fileWorker || *allowText || *textOnce != "" || *textDisplay != ":0" || *textAuthority != "" {
			fmt.Fprintln(os.Stderr, "virtio-serial is observer-only; privilege options rejected")
			os.Exit(1)
		}
		if err := listenSerial(); err != nil {
			fmt.Fprintln(os.Stderr, "virtio-serial integration fenced:", err)
			os.Exit(1)
		}
		return
	}
	if *allowText {
		if *fileWorker {
			fmt.Fprintln(os.Stderr, "text input is not available to the files worker")
			os.Exit(1)
		}
		service, err := newX11TextService(*textDisplay, *textAuthority)
		if err != nil {
			fmt.Fprintln(os.Stderr, "text input display rejected:", err)
			os.Exit(1)
		}
		textInput = service
	} else if *textDisplay != ":0" || *textAuthority != "" {
		fmt.Fprintln(os.Stderr, "text input settings require --allow-text-input")
		os.Exit(1)
	}
	if *textOnce != "" {
		// Local diagnostic for real-Guest acceptance: same validation and injector as the RPC.
		if textInput != nil {
			textInput.diagnostic = true // print the bindings; the operator typed the text on this command line
		}
		_, status := textInfo()
		reply := handleTextInput(request{ID: "once", Method: "text_input", TextHex: *textOnce})
		for _, record := range []struct {
			label string
			value any
		}{{"TEXT_INPUT_STATUS", status}, {"TEXT_INPUT_ONCE", reply}} {
			line, err := json.Marshal(record.value)
			if err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(1)
			}
			fmt.Printf("%s %s\n", record.label, line)
		}
		restoreFailed := false
		if textInput != nil {
			// This process is about to exit, so the deferred restore happens here, after the
			// quiet period that lets a slow client finish resolving the key events.
			waited := time.Duration(0)
			if textInput.boundCount() != 0 {
				waited = textQuietPeriod
				time.Sleep(waited)
			}
			restored, remaining, err := textInput.restoreAll()
			message := ""
			if err != nil {
				message, restoreFailed = err.Error(), true
			}
			line, _ := json.Marshal(map[string]any{"restored": restored, "remaining": remaining, "waited_ms": waited.Milliseconds(), "error": message})
			fmt.Printf("TEXT_INPUT_RESTORE %s\n", line)
		}
		if reply.Error != nil {
			os.Exit(3)
		}
		if restoreFailed {
			os.Exit(4)
		}
		return
	}
	if *serialExec {
		if !*allowExec || (*execUser == "" && *containerRoot == "") || *desktopStream || *allowShutdown || *allowFiles || *legacyExec || *filesUser != "" || *filesRoot != "" || *fileWorker {
			fmt.Fprintln(os.Stderr, "serial v2 requires --allow-execute and fixed --exec-user or --container-root; files/shutdown/legacy forbidden")
			os.Exit(1)
		}
		identity, err := resolveIdentity()
		if err != nil {
			fmt.Fprintln(os.Stderr, "serial v2 execution identity unavailable:", err)
			os.Exit(1)
		}
		commands.identity = identity
		commands.boundary, commands.containmentMode, err = configureCommandContainment(identity, false)
		if err != nil {
			fmt.Fprintln(os.Stderr, "serial v2 cgroup containment unavailable:", err)
			os.Exit(1)
		}
		// configureCommandContainment has already killed/drained prior service groups
		// and verified real clone3 credentials before any fresh challenge is emitted.
		installSignalCleanup(commands)
		go serveDriverPort()
		if err = listenSerialExec(); err != nil {
			fmt.Fprintln(os.Stderr, "serial v2 incarnation fenced:", err)
			os.Exit(1)
		}
		return
	}
	if *fileWorker {
		if err := runFileWorker(*filesRoot); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if *allowFiles {
		var err error
		files, err = startFileBridge(*filesUser, *filesRoot)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		defer files.stop()
	} else if *filesUser != "" || *filesRoot != "" {
		fmt.Fprintln(os.Stderr, "files settings require --allow-files")
		os.Exit(1)
	}
	identity, err := resolveIdentity()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	commands.identity = identity
	if *allowExec {
		commands.boundary, commands.containmentMode, err = configureCommandContainment(identity, *legacyExec)
		if err != nil {
			fmt.Fprintln(os.Stderr, "execution containment unavailable:", err)
			if files != nil {
				files.stop()
			}
			os.Exit(1)
		}
	} else if *legacyExec {
		fmt.Fprintln(os.Stderr, "legacy execution option requires --allow-execute")
		os.Exit(1)
	}
	installSignalCleanup(commands)
	if *desktopStream {
		desktop = newDesktopObserver()
		go func() {
			if err := listenDesktop(); err != nil {
				fmt.Fprintln(os.Stderr, "desktop stream unavailable:", err)
			}
		}()
	}
	fmt.Fprintln(os.Stderr, "veynode guest agent protocol=1 port=5000 hostCID=2")
	if err := listenVsock(*allowExec, *allowShutdown); err != nil {
		commands.stopAll(time.Second)
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func installSignalCleanup(manager *jobManager) {
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-signals
		if files != nil {
			files.stop()
		}
		if textInput != nil {
			// Best effort: temporary keycodes are harmless if this fails, and info reports them.
			if textInput.mu.TryLock() {
				_, _, _ = textInput.restoreLocked(true)
				textInput.mu.Unlock()
			}
		}
		if !manager.stopAll(time.Second) {
			fmt.Fprintln(os.Stderr, "active command cleanup failed or exceeded 1 second; containment cleanup was not confirmed")
			os.Exit(1)
		}
		os.Exit(0)
	}()
}
