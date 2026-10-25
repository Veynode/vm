package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"strconv"
	"sync"
	"time"
	"unicode/utf8"
)

const serialHeartbeatTimeout = 2 * time.Second
const serialScopeCleanupTimeout = 3 * time.Second
const maxScopeEpoch = 9007199254740991

type serialScope struct {
	Owner string `json:"owner"`
	Epoch uint64 `json:"epoch"`
}

func (scope *serialScope) key() (string, error) {
	if scope == nil || !validSerialNonce(scope.Owner) || scope.Epoch == 0 || scope.Epoch > maxScopeEpoch {
		return "", fmt.Errorf("invalid serial scope")
	}
	return scope.Owner + ":" + strconv.FormatUint(scope.Epoch, 10), nil
}
func exactKeys(m map[string]json.RawMessage, keys ...string) bool {
	if len(m) != len(keys) {
		return false
	}
	for _, k := range keys {
		if _, ok := m[k]; !ok {
			return false
		}
	}
	return true
}

// encoding/json normally accepts duplicate keys and invalid UTF-8. Both are
// rejected before typed decoding can hide which authority fields were supplied.
func uniqueSerialJSON(data []byte) bool {
	if !utf8.Valid(data) {
		return false
	}
	d := json.NewDecoder(bytes.NewReader(data))
	d.UseNumber()
	var walk func(int) bool
	walk = func(depth int) bool {
		if depth > 16 {
			return false
		}
		token, err := d.Token()
		if err != nil {
			return false
		}
		delimiter, ok := token.(json.Delim)
		if !ok {
			return true
		}
		switch delimiter {
		case '{':
			seen := map[string]bool{}
			for d.More() {
				key, e := d.Token()
				if e != nil {
					return false
				}
				name, ok := key.(string)
				if !ok || seen[name] {
					return false
				}
				seen[name] = true
				if !walk(depth + 1) {
					return false
				}
			}
			end, e := d.Token()
			return e == nil && end == json.Delim('}')
		case '[':
			for d.More() {
				if !walk(depth + 1) {
					return false
				}
			}
			end, e := d.Token()
			return e == nil && end == json.Delim(']')
		default:
			return false
		}
	}
	if !walk(0) {
		return false
	}
	_, err := d.Token()
	return err == io.EOF
}
func validateSerialV2(e serialEnvelope, hello bool) error {
	if !uniqueSerialJSON(e.wire) {
		return fmt.Errorf("invalid or duplicate serial JSON")
	}
	var outer map[string]json.RawMessage
	if json.Unmarshal(e.wire, &outer) != nil {
		return fmt.Errorf("invalid envelope")
	}
	if hello {
		if !exactKeys(outer, "version", "type", "guest_nonce", "host_nonce") {
			return fmt.Errorf("invalid hello fields")
		}
		return nil
	}
	if !exactKeys(outer, "version", "type", "guest_nonce", "host_nonce", "sequence", "request") || e.Request == nil {
		return fmt.Errorf("invalid request fields")
	}
	var fields map[string]json.RawMessage
	if json.Unmarshal(outer["request"], &fields) != nil {
		return fmt.Errorf("invalid nested request")
	}
	req := e.Request
	if req.ID == "" || len(req.ID) > 128 {
		return fmt.Errorf("invalid nested request id")
	}
	keys := []string{"id", "method"}
	switch req.Method {
	case "info", "heartbeat":
	case "scope_revoke":
		keys = append(keys, "scope")
	case "exec_start":
		keys = append(keys, "scope", "argv", "timeout_ms")
		if validateCommand(*req) != nil {
			return fmt.Errorf("invalid start arguments")
		}
	case "exec_poll", "exec_cancel":
		keys = append(keys, "scope", "job_id")
		if !validJobID(req.JobID) || req.JobID != string(bytes.ToLower([]byte(req.JobID))) {
			return fmt.Errorf("invalid job id")
		}
	default:
		return fmt.Errorf("unsupported serial execution request")
	}
	if !exactKeys(fields, keys...) {
		return fmt.Errorf("invalid method fields")
	}
	if req.Scope != nil {
		if _, err := req.Scope.key(); err != nil {
			return err
		}
		var scope map[string]json.RawMessage
		if json.Unmarshal(fields["scope"], &scope) != nil || !exactKeys(scope, "owner", "epoch") {
			return fmt.Errorf("invalid scope fields")
		}
	}
	if len(keys) > 2 && req.Scope == nil {
		return fmt.Errorf("scope required")
	}
	return nil
}

// The one lifetime never reconnects. Reader, writer and watchdog stay independent
// of cmd.Start and revocation. A fatal path seals admission immediately; its final
// return is delayed until scope descendants are confirmed gone or cleanup fails.
// Guest time pauses with QEMU: host guardian termination remains mandatory.
func serveSerialExec(s deadlineStream, manager *jobManager) (returned error) {
	// OCI micro-VMs run commands as the image user inside the container root; the
	// VM itself is the isolation boundary, as on the vsock transport.
	containerRoot := manager.identity.Mode == "container-root" && manager.identity.Chroot != ""
	if manager.containmentMode != "cgroup-v2-clone3-kill" || (!containerRoot && (!manager.identity.Rootless || manager.identity.UID == 0)) {
		s.Close()
		return fmt.Errorf("serial execution requires verified rootless cgroup containment")
	}
	var once sync.Once
	done := make(chan struct{})
	fatal := make(chan error, 1)
	trip := func(err error) {
		once.Do(func() {
			manager.mu.Lock()
			manager.stopping = true
			for control := range manager.controls {
				control.requestCancel()
			}
			manager.mu.Unlock()
			fatal <- err
			close(done)
			s.Close()
		})
	}
	defer func() {
		trip(fmt.Errorf("serial incarnation ended"))
		if !manager.stopAll(serialScopeCleanupTimeout) {
			returned = fmt.Errorf("serial execution cleanup unconfirmed")
		}
	}()
	nonce, err := serialNonce()
	if err != nil {
		return err
	}
	if err = serialWrite(s, serialEnvelope{Version: 2, Type: "challenge", GuestNonce: nonce}); err != nil {
		return err
	}
	hello, err := serialRead(s, false)
	if err != nil {
		return err
	}
	if validateSerialV2(hello, true) != nil || hello.Version != 2 || hello.Type != "hello" || hello.GuestNonce != nonce || !validSerialNonce(hello.HostNonce) || hello.Sequence != 0 || hello.Request != nil || hello.Response != nil {
		return fmt.Errorf("serial v2 handshake rejected")
	}
	if err = serialWrite(s, serialEnvelope{Version: 2, Type: "ready", GuestNonce: nonce, HostNonce: hello.HostNonce}); err != nil {
		return err
	}
	replies := make(chan serialEnvelope, 8)
	ordinary, revoke := make(chan struct{}, 1), make(chan struct{}, 1)
	var heartbeatMu sync.Mutex
	heartbeat := time.Now()
	enqueue := func(seq uint64, reply response) bool {
		select {
		case <-done:
			return false
		default:
		}
		e := serialEnvelope{Version: 2, Type: "response", GuestNonce: nonce, HostNonce: hello.HostNonce, Sequence: seq, Response: &reply}
		select {
		case replies <- e:
			return true
		default:
			trip(fmt.Errorf("serial reply capacity exceeded"))
			return false
		}
	}
	var ioWorkers sync.WaitGroup
	ioWorkers.Add(3)
	go func() {
		defer ioWorkers.Done()
		for {
			select {
			case <-done:
				return
			case reply := <-replies:
				select {
				case <-done:
					return
				default:
				}
				if err := serialWrite(s, reply); err != nil {
					trip(fmt.Errorf("serial response write failed"))
					return
				}
			}
		}
	}()
	go func() {
		defer ioWorkers.Done()
		timer := time.NewTicker(50 * time.Millisecond)
		defer timer.Stop()
		for {
			select {
			case <-done:
				return
			case <-timer.C:
				manager.mu.Lock()
				containmentFailed := manager.cleanupFailed
				manager.mu.Unlock()
				if containmentFailed {
					trip(fmt.Errorf("serial asynchronous containment failed"))
					return
				}
				heartbeatMu.Lock()
				expired := time.Since(heartbeat) > serialHeartbeatTimeout
				heartbeatMu.Unlock()
				if expired {
					trip(fmt.Errorf("serial heartbeat expired"))
					return
				}
			}
		}
	}()
	go func() {
		defer ioWorkers.Done()
		for seq := uint64(1); seq > 0; seq++ {
			e, err := serialRead(s, true)
			if err != nil {
				trip(fmt.Errorf("serial request read failed"))
				return
			}
			if validateSerialV2(e, false) != nil || e.Version != 2 || e.Type != "request" || e.GuestNonce != nonce || e.HostNonce != hello.HostNonce || e.Sequence != seq || e.Response != nil {
				trip(fmt.Errorf("serial request binding rejected"))
				return
			}
			req := *e.Request
			if req.Method == "heartbeat" {
				heartbeatMu.Lock()
				heartbeat = time.Now()
				heartbeatMu.Unlock()
				if !enqueue(seq, response{ID: req.ID, Result: map[string]any{"alive": true}}) {
					return
				}
				continue
			}
			lane := ordinary
			if req.Method == "scope_revoke" {
				lane = revoke
			}
			select {
			case lane <- struct{}{}:
			default:
				trip(fmt.Errorf("serial operation lane already occupied"))
				return
			}
			go func(sequence uint64, req request, lane chan struct{}) {
				laneReleased := false
				defer func() {
					if !laneReleased {
						<-lane
					}
				}()
				select {
				case <-done:
					return
				default:
				}
				var reply response
				if req.Method == "info" {
					reply = handleWithManager(manager, req, true, false)
					if value, ok := reply.Result.(map[string]any); ok {
						value["transport"] = "virtio-serial-private-exec-v2"
						value["transport_reconnect"] = false
						value["transport_heartbeat_timeout_ms"] = 2000
						value["transport_max_scopes"] = maxCommandScopes
					}
				}
				if req.Scope != nil {
					scope, _ := req.Scope.key()
					switch req.Method {
					case "exec_start":
						reply = manager.startScoped(req, scope)
					case "exec_poll":
						reply = manager.pollScoped(req, scope)
					case "exec_cancel":
						reply = manager.cancelScoped(req, scope)
					case "scope_revoke":
						if manager.revokeScope(scope, serialScopeCleanupTimeout) {
							reply = response{ID: req.ID, Result: map[string]any{"revoked": true, "pending_starts": 0, "cgroup_empty": true}}
						} else {
							reply = failure(req.ID, "containment_failed", "scope cleanup unconfirmed")
						}
					}
				}
				// Release the completed operation before publishing its reply. A fast
				// client may submit its next request as soon as the writer sends it.
				<-lane
				laneReleased = true
				enqueue(sequence, reply)
				if reply.Error != nil && reply.Error.Code == "containment_failed" {
					trip(fmt.Errorf("serial command containment failed"))
				}
			}(seq, req, lane)
		}
		trip(fmt.Errorf("serial sequence exhausted"))
	}()
	returned = <-fatal
	ioWorkers.Wait()
	return returned
}
