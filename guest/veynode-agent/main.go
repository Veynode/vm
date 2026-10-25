// Guest integration service. The vsock transport is restricted to host CID 2.
// No TCP listener; execution is opt-in. This is a headless Linux service, not a desktop agent.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
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
	ID        string       `json:"id"`
	Scope     *serialScope `json:"scope,omitempty"`
	Method    string       `json:"method"`
	Argv      []string     `json:"argv,omitempty"`
	TimeoutMS int          `json:"timeout_ms,omitempty"`
	JobID     string       `json:"job_id,omitempty"`
	Owner     string       `json:"owner,omitempty"`
	Path      string       `json:"path,omitempty"`
	Token     string       `json:"token,omitempty"`
	Data      string       `json:"data,omitempty"`
	Size      int64        `json:"size,omitempty"`
	Offset    int64        `json:"offset,omitempty"`
	SHA256    string       `json:"sha256,omitempty"`
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
		return response{ID: req.ID, Result: map[string]any{"protocol_version": 1, "os": runtime.GOOS, "arch": runtime.GOARCH, "os_release": string(release), "transport": "vsock-host-only", "execution_identity": manager.identity, "execution_containment": manager.containmentMode, "service_uid": os.Geteuid(), "file_policy": filePolicy, "capabilities": map[string]bool{"exec": allowExec, "async_exec": allowExec, "descendant_containment": allowExec && manager.containmentMode == "cgroup-v2-clone3-kill", "shutdown": shutdownEnabled, "desktop": false, "files": files != nil}, "max_output_bytes_per_stream": maxOutput, "max_timeout_ms": 30000, "max_active_commands": maxActiveCommands, "max_finished_jobs": maxFinishedJobs, "finished_job_ttl_seconds": int(finishedJobTTL.Seconds())}}
	case "file_upload_begin", "file_upload_write", "file_upload_commit", "file_download_begin", "file_download_read", "file_cancel", "file_session_end":
		if files == nil {
			return failure(req.ID, "unsupported", "file integration disabled; requires Linux files worker and explicit directory")
		}
		return files.call(req)
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
	reader := bufio.NewReader(io.LimitReader(stream, maxRequest+1))
	line, err := reader.ReadBytes('\n')
	var reply response
	if err != nil || len(line) > maxRequest {
		reply = failure("", "invalid_request", "expected newline JSON within 64KiB")
	} else {
		var req request
		decoder := json.NewDecoder(bytes.NewReader(line))
		decoder.DisallowUnknownFields()
		if err = decoder.Decode(&req); err != nil {
			reply = failure("", "invalid_request", "invalid JSON request")
		} else if decoder.Decode(new(any)) != io.EOF {
			reply = failure(req.ID, "invalid_request", "trailing JSON")
		} else {
			reply = handle(req, allowExec, allowShutdown...)
		}
	}
	_ = json.NewEncoder(stream).Encode(reply)
}
func main() {
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
	if len(os.Args) > 1 && os.Args[1] == "container-init" {
		os.Exit(containerInit())
	}
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
		if *serialExec || *allowExec || *allowShutdown || *allowFiles || *legacyExec || *execUser != "" || *filesUser != "" || *filesRoot != "" || *fileWorker {
			fmt.Fprintln(os.Stderr, "virtio-serial is observer-only; privilege options rejected")
			os.Exit(1)
		}
		if err := listenSerial(); err != nil {
			fmt.Fprintln(os.Stderr, "virtio-serial integration fenced:", err)
			os.Exit(1)
		}
		return
	}
	if *serialExec {
		if !*allowExec || (*execUser == "" && *containerRoot == "") || *allowShutdown || *allowFiles || *legacyExec || *filesUser != "" || *filesRoot != "" || *fileWorker {
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
		if !manager.stopAll(time.Second) {
			fmt.Fprintln(os.Stderr, "active command cleanup failed or exceeded 1 second; containment cleanup was not confirmed")
			os.Exit(1)
		}
		os.Exit(0)
	}()
}
