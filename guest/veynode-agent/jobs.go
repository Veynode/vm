package main

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"os/exec"
	"strings"
	"sync"
	"time"
)

const maxActiveCommands = 4
const maxFinishedJobs = 32
const finishedJobTTL = 5 * time.Minute

type executionControl struct {
	cancel       chan struct{}
	once         sync.Once
	scope        string
	startPending bool
}

func (control *executionControl) requestCancel() { control.once.Do(func() { close(control.cancel) }) }

type commandJob struct {
	scope           string
	control         *executionControl
	cancelRequested bool
	finished        bool
	finishedAt      time.Time
	finishSequence  uint64
	result          map[string]any
	err             *rpcError
}
type commandScopeState struct {
	retired          bool
	active, starting int
}

const maxCommandScopes = 1024

var errStartContainment = errors.New("failed command start containment cleanup")

type jobManager struct {
	scopes          map[string]*commandScopeState
	mu              sync.Mutex
	active          int
	jobs            map[string]*commandJob
	now             func() time.Time
	finishSequence  uint64
	identity        executionIdentity
	controls        map[*executionControl]struct{}
	stopping        bool
	boundary        boundaryFactory
	containmentMode string
	cleanupFailed   bool
}

func newJobManager(now func() time.Time) *jobManager {
	return &jobManager{scopes: make(map[string]*commandScopeState), jobs: make(map[string]*commandJob), now: now, identity: currentExecutionIdentity(), controls: make(map[*executionControl]struct{}), boundary: legacyBoundary, containmentMode: "legacy-process-group-not-contained"}
}
func validateCommand(req request) *rpcError {
	if len(req.Argv) == 0 || len(req.Argv) > 128 || req.Argv[0] == "" || req.TimeoutMS < 1 || req.TimeoutMS > 30000 {
		return &rpcError{"invalid_request", "argv required; timeout_ms must be 1..30000"}
	}
	for _, argument := range req.Argv {
		if strings.ContainsRune(argument, 0) {
			return &rpcError{"invalid_request", "NUL in argv"}
		}
	}
	return nil
}
func validJobID(id string) bool {
	if len(id) != 32 {
		return false
	}
	_, err := hex.DecodeString(id)
	return err == nil
}

// pruneLocked never removes active jobs. Finished state is bounded even if no
// further client polls arrive; expiry is applied on every manager operation.
func (manager *jobManager) pruneLocked(now time.Time) {
	finished := 0
	for id, job := range manager.jobs {
		if !job.finished {
			continue
		}
		if !now.Before(job.finishedAt.Add(finishedJobTTL)) {
			delete(manager.jobs, id)
		} else {
			finished++
		}
	}
	for finished > maxFinishedJobs {
		oldestID := ""
		var oldestSequence uint64
		for id, job := range manager.jobs {
			if job.finished && (oldestID == "" || job.finishSequence < oldestSequence) {
				oldestID = id
				oldestSequence = job.finishSequence
			}
		}
		delete(manager.jobs, oldestID)
		finished--
	}
}
func (manager *jobManager) reserve() *executionControl {
	control, _ := manager.reserveScoped("")
	return control
}
func (manager *jobManager) reserveScoped(scope string) (*executionControl, *rpcError) {
	manager.mu.Lock()
	defer manager.mu.Unlock()
	manager.pruneLocked(manager.now())
	if manager.stopping || manager.active >= maxActiveCommands {
		return nil, &rpcError{"busy", "command limit reached or service is stopping"}
	}
	var state *commandScopeState
	if scope != "" {
		state = manager.scopes[scope]
		if state == nil {
			if len(manager.scopes) >= maxCommandScopes {
				return nil, &rpcError{"busy", "service scope lifetime capacity reached"}
			}
			state = &commandScopeState{}
			manager.scopes[scope] = state
		}
		if state.retired {
			return nil, &rpcError{"scope_retired", "command scope is permanently retired"}
		}
		state.active++
		state.starting++
	}
	control := &executionControl{cancel: make(chan struct{}), scope: scope, startPending: scope != ""}
	manager.active++
	manager.controls[control] = struct{}{}
	return control, nil
}
func (manager *jobManager) startedLocked(control *executionControl) {
	if control.startPending {
		manager.scopes[control.scope].starting--
		control.startPending = false
	}
}
func (manager *jobManager) releaseLocked(control *executionControl) {
	manager.startedLocked(control)
	manager.active--
	delete(manager.controls, control)
	if control.scope != "" {
		manager.scopes[control.scope].active--
	}
}
func (manager *jobManager) release(control *executionControl) {
	manager.mu.Lock()
	defer manager.mu.Unlock()
	manager.releaseLocked(control)
}

// Retirement precedes cancellation. Active includes pending starts until their
// real start outcome and descendant cleanup finish; lost start replies cannot
// hide a job from this scope-level barrier. Failed cleanup remains sticky.
func (manager *jobManager) revokeScope(scope string, timeout time.Duration) bool {
	manager.mu.Lock()
	state := manager.scopes[scope]
	if state == nil {
		if len(manager.scopes) >= maxCommandScopes {
			manager.noteCleanupFailureLocked(&rpcError{"containment_failed", "scope retirement capacity exhausted"})
			manager.mu.Unlock()
			return false
		}
		state = &commandScopeState{}
		manager.scopes[scope] = state
	}
	state.retired = true
	for control := range manager.controls {
		if control.scope == scope {
			control.requestCancel()
		}
	}
	manager.mu.Unlock()
	deadline := time.Now().Add(timeout)
	for {
		manager.mu.Lock()
		done := state.active == 0 && state.starting == 0
		failed := manager.cleanupFailed
		manager.mu.Unlock()
		if done {
			return !failed
		}
		if time.Now().After(deadline) {
			manager.mu.Lock()
			manager.noteCleanupFailureLocked(&rpcError{"containment_failed", "scope cleanup deadline"})
			manager.mu.Unlock()
			return false
		}
		time.Sleep(time.Millisecond)
	}
}
func (manager *jobManager) stopAll(timeout time.Duration) bool {
	manager.mu.Lock()
	manager.stopping = true
	for control := range manager.controls {
		control.requestCancel()
	}
	manager.mu.Unlock()
	deadline := time.Now().Add(timeout)
	for {
		manager.mu.Lock()
		active := manager.active
		failed := manager.cleanupFailed
		manager.mu.Unlock()
		if active == 0 {
			return !failed
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(time.Millisecond)
	}
}

// Once descendant cleanup is unconfirmed, stop admission and cancel peers.
// A drained active count alone must not turn a failed cleanup into service success.
func (manager *jobManager) noteCleanupFailureLocked(err *rpcError) {
	if err == nil || err.Code != "containment_failed" {
		return
	}
	manager.cleanupFailed = true
	manager.stopping = true
	for control := range manager.controls {
		control.requestCancel()
	}
}
func (manager *jobManager) execute(req request) response {
	if err := validateCommand(req); err != nil {
		return response{ID: req.ID, Error: err}
	}
	control := manager.reserve()
	if control == nil {
		return failure(req.ID, "busy", "command limit reached or service is stopping")
	}
	defer manager.release(control)
	running, err := startCommand(req, manager.identity, manager.boundary)
	if err != nil {
		if errors.Is(err, errStartContainment) {
			manager.mu.Lock()
			manager.noteCleanupFailureLocked(&rpcError{"containment_failed", "failed start cleanup unconfirmed"})
			manager.mu.Unlock()
			return failure(req.ID, "containment_failed", "failed start cleanup unconfirmed")
		}
		return failure(req.ID, "exec_failed", err.Error())
	}
	result, rpcErr := running.wait(control.cancel)
	manager.mu.Lock()
	manager.noteCleanupFailureLocked(rpcErr)
	manager.mu.Unlock()
	if rpcErr != nil {
		return response{ID: req.ID, Error: rpcErr}
	}
	return response{ID: req.ID, Result: result}
}
func (manager *jobManager) start(req request) response { return manager.startScoped(req, "") }
func (manager *jobManager) startScoped(req request, scope string) response {
	if err := validateCommand(req); err != nil {
		return response{ID: req.ID, Error: err}
	}
	control, denied := manager.reserveScoped(scope)
	if denied != nil {
		return response{ID: req.ID, Error: denied}
	}
	var random [16]byte
	if _, err := rand.Read(random[:]); err != nil {
		manager.release(control)
		return failure(req.ID, "exec_failed", "could not allocate unpredictable job identifier")
	}
	id := hex.EncodeToString(random[:])
	running, err := startCommand(req, manager.identity, manager.boundary)
	if err != nil {
		manager.mu.Lock()
		if errors.Is(err, errStartContainment) {
			manager.noteCleanupFailureLocked(&rpcError{"containment_failed", "failed start cleanup unconfirmed"})
		}
		manager.releaseLocked(control)
		manager.mu.Unlock()
		if errors.Is(err, errStartContainment) {
			return failure(req.ID, "containment_failed", "failed start cleanup unconfirmed")
		}
		return failure(req.ID, "exec_failed", err.Error())
	}
	job := &commandJob{control: control, scope: scope}
	manager.mu.Lock()
	manager.startedLocked(control)
	manager.jobs[id] = job
	manager.mu.Unlock()
	go func() {
		result, rpcErr := running.wait(job.control.cancel)
		manager.mu.Lock()
		defer manager.mu.Unlock()
		manager.noteCleanupFailureLocked(rpcErr)
		manager.releaseLocked(control)
		manager.finishSequence++
		job.finished = true
		job.finishedAt = manager.now()
		job.finishSequence = manager.finishSequence
		job.result = result
		job.err = rpcErr
		manager.pruneLocked(manager.now())
	}()
	return response{ID: req.ID, Result: map[string]any{"job_id": id, "state": "running"}}
}
func (manager *jobManager) poll(req request) response { return manager.pollScoped(req, "") }
func (manager *jobManager) pollScoped(req request, scope string) response {
	if !validJobID(req.JobID) {
		return failure(req.ID, "invalid_request", "job_id must be 32 hexadecimal characters")
	}
	manager.mu.Lock()
	defer manager.mu.Unlock()
	manager.pruneLocked(manager.now())
	job, ok := manager.jobs[req.JobID]
	if !ok || job.scope != scope {
		return failure(req.ID, "not_found", "job is unknown or its finished result expired")
	}
	result := map[string]any{"job_id": req.JobID, "state": "running"}
	if job.finished {
		result["state"] = "finished"
		if job.err != nil {
			result["error"] = job.err
		} else {
			result["result"] = job.result
		}
	}
	return response{ID: req.ID, Result: result}
}
func (manager *jobManager) cancel(req request) response { return manager.cancelScoped(req, "") }
func (manager *jobManager) cancelScoped(req request, scope string) response {
	if !validJobID(req.JobID) {
		return failure(req.ID, "invalid_request", "job_id must be 32 hexadecimal characters")
	}
	manager.mu.Lock()
	defer manager.mu.Unlock()
	manager.pruneLocked(manager.now())
	job, ok := manager.jobs[req.JobID]
	if !ok || job.scope != scope {
		return failure(req.ID, "not_found", "job is unknown or its finished result expired")
	}
	if job.finished {
		return response{ID: req.ID, Result: map[string]any{"cancel_requested": false}}
	}
	if !job.cancelRequested {
		job.cancelRequested = true
		job.control.requestCancel()
	}
	return response{ID: req.ID, Result: map[string]any{"cancel_requested": true}}
}

type runningCommand struct {
	cmd            *exec.Cmd
	stdout, stderr *limitedBuffer
	started        time.Time
	timeout        time.Duration
	done           chan error
	boundary       jobBoundary
}

func startCommand(req request, identity executionIdentity, factory boundaryFactory) (*runningCommand, error) {
	executable, err := commandExecutable(req.Argv[0])
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(executable, req.Argv[1:]...)
	identity.configure(cmd)
	stdout, stderr := new(limitedBuffer), new(limitedBuffer)
	cmd.Stdout = stdout
	cmd.Stderr = stderr
	cmd.WaitDelay = 250 * time.Millisecond
	boundary, err := factory(cmd)
	if err != nil {
		return nil, fmt.Errorf("command containment unavailable: %w", err)
	}
	started := time.Now()
	if err := cmd.Start(); err != nil {
		if closeErr := boundary.close(); closeErr != nil {
			return nil, fmt.Errorf("%w: %v", errStartContainment, closeErr)
		}
		return nil, err
	}
	boundary.started()
	command := &runningCommand{cmd: cmd, stdout: stdout, stderr: stderr, started: started, timeout: time.Duration(req.TimeoutMS) * time.Millisecond, done: make(chan error, 1), boundary: boundary}
	go func() { command.done <- cmd.Wait() }()
	return command, nil
}
func (command *runningCommand) wait(cancel <-chan struct{}) (map[string]any, *rpcError) {
	remaining := command.timeout - time.Since(command.started)
	if remaining < 0 {
		remaining = 0
	}
	timer := time.NewTimer(remaining)
	defer timer.Stop()
	timedOut, cancelled := false, false
	var err error
	select {
	case err = <-command.done:
	case <-timer.C:
		timedOut = true
	case <-cancel:
		cancelled = true
	}
	// Kill the entire descendant boundary even after the original parent exits.
	// clone3 places the child in its cgroup before any user code can execute.
	containmentErr := command.boundary.terminate()
	if timedOut || cancelled {
		if containmentErr != nil {
			_ = command.cmd.Process.Kill()
		}
		select {
		case err = <-command.done:
		case <-time.After(time.Second):
			return nil, &rpcError{"containment_failed", "command cleanup exceeded bounded wait"}
		}
	}
	closeErr := command.boundary.close()
	if containmentErr != nil || closeErr != nil {
		return nil, &rpcError{"containment_failed", fmt.Sprintf("descendant cleanup failed: %v / %v", containmentErr, closeErr)}
	}
	code := 0
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			code = exitErr.ExitCode()
		} else if !timedOut && !cancelled {
			return nil, &rpcError{"exec_failed", err.Error()}
		}
	}
	out, truncatedOut := command.stdout.result()
	stderr, truncatedErr := command.stderr.result()
	return map[string]any{"exit_code": code, "stdout": out, "stderr": stderr, "stdout_truncated": truncatedOut, "stderr_truncated": truncatedErr, "timed_out": timedOut, "cancelled": cancelled, "duration_ms": time.Since(command.started).Milliseconds()}, nil
}
