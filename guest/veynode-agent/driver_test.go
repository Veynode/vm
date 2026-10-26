package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"
)

// The fake driver is this test binary run as a child: an MCP server on stdio
// whose tools behave as their names say.
func TestMain(m *testing.M) {
	if os.Getenv("VEYNODE_FAKE_DRIVER") == "1" {
		fakeDriver()
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func fakeDriver() {
	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 64<<10), 4<<20)
	var mu sync.Mutex
	send := func(format string, args ...any) {
		mu.Lock()
		defer mu.Unlock()
		fmt.Printf(format+"\n", args...)
	}
	initialized := 0
	for in.Scan() {
		var msg struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params struct {
				Name      string          `json:"name"`
				Arguments json.RawMessage `json:"arguments"`
			} `json:"params"`
		}
		if json.Unmarshal(in.Bytes(), &msg) != nil {
			continue
		}
		switch {
		case msg.Method == "initialize":
			initialized++
			send(`{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}`, msg.ID)
		case msg.Method == "notifications/initialized":
		case msg.Method == "tools/list":
			send(`{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo"}],"initialized":%d}}`, msg.ID, initialized)
		case msg.Params.Name == "echo":
			text, _ := json.Marshal(string(msg.Params.Arguments))
			send(`{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":%s}]}}`, msg.ID, text)
		case msg.Params.Name == "slow":
			id := msg.ID
			go func() {
				time.Sleep(300 * time.Millisecond)
				send(`{"jsonrpc":"2.0","id":%s,"result":{"late":true}}`, id)
			}()
		case msg.Params.Name == "notify":
			send(`{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}`)
			send(`{"jsonrpc":"2.0","id":7,"method":"roots/list"}`)
			send(`{"jsonrpc":"2.0","id":%s,"result":{"after":"notification"}}`, msg.ID)
		case msg.Params.Name == "big":
			send(`{"jsonrpc":"2.0","id":%s,"result":{"blob":"%s"}}`, msg.ID, strings.Repeat("x", maxDriverFrame))
		case msg.Params.Name == "crash":
			fmt.Fprintln(os.Stderr, "fake driver crashed on purpose")
			os.Exit(3)
		}
	}
}

func fakeSupervisor(t *testing.T) *driverSupervisor {
	t.Helper()
	supervisor := &driverSupervisor{root: func() string { return "/" }}
	supervisor.start = func(string) (*driverProcess, error) {
		cmd := exec.Command(os.Args[0], "-test.run=^$")
		cmd.Env = append(os.Environ(), "VEYNODE_FAKE_DRIVER=1")
		return launchDriver(cmd)
	}
	t.Cleanup(func() {
		supervisor.mu.Lock()
		defer supervisor.mu.Unlock()
		if supervisor.proc != nil {
			supervisor.proc.stop()
		}
	})
	return supervisor
}

func callTool(name, arguments string) []byte {
	return fmt.Appendf(nil, `{"jsonrpc":"2.0","id":"c-%s","method":"tools/call","params":{"name":%q,"arguments":%s}}`, name, name, arguments)
}

func decodeFrame(t *testing.T, frame []byte) map[string]any {
	t.Helper()
	var value map[string]any
	if err := json.Unmarshal(frame, &value); err != nil {
		t.Fatalf("%v: %s", err, frame)
	}
	return value
}

func driverCode(err error) string {
	if coded, ok := err.(*driverError); ok {
		return coded.code
	}
	return ""
}

func TestDriverHandshakeOnceAndCallerIDsRestored(t *testing.T) {
	s := fakeSupervisor(t)
	var wg sync.WaitGroup
	for i := range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			id := fmt.Sprintf(`"caller-%d"`, i)
			reply, err := s.call(fmt.Appendf(nil, `{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"echo","arguments":{"n":%d}}}`, id, i), time.Second)
			if err != nil {
				t.Error(err)
				return
			}
			value := decodeFrame(t, reply)
			text := value["result"].(map[string]any)["content"].([]any)[0].(map[string]any)["text"]
			if value["id"] != fmt.Sprintf("caller-%d", i) || text != fmt.Sprintf(`{"n":%d}`, i) {
				t.Errorf("caller %d got %s", i, reply)
			}
		}()
	}
	wg.Wait()
	reply, err := s.call([]byte(`{"jsonrpc":"2.0","id":42,"method":"tools/list"}`), time.Second)
	if err != nil {
		t.Fatal(err)
	}
	value := decodeFrame(t, reply)
	if value["id"] != float64(42) || value["result"].(map[string]any)["initialized"] != float64(1) {
		t.Fatalf("handshake not exactly once, or id lost: %s", reply)
	}
}

func TestDriverForwardsOnlyHostRequests(t *testing.T) {
	s := fakeSupervisor(t)
	for _, frame := range []string{
		`{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}`,
		`{"jsonrpc":"2.0","method":"notifications/cancelled"}`,
		`[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]`,
		`{"jsonrpc":"2.0","id":null,"method":"tools/list"}`,
		`not json`,
	} {
		if _, err := s.call([]byte(frame), time.Second); driverCode(err) != "invalid_request" {
			t.Fatalf("%s: %v", frame, err)
		}
	}
	if s.proc != nil {
		t.Fatal("a refused frame started the driver")
	}
}

func TestDriverDeadlineDropsTheLateReply(t *testing.T) {
	s := fakeSupervisor(t)
	if _, err := s.call(callTool("slow", "{}"), 50*time.Millisecond); driverCode(err) != "deadline_exceeded" {
		t.Fatal(err)
	}
	time.Sleep(400 * time.Millisecond) // the late reply arrives and finds no waiter
	reply, err := s.call(callTool("echo", `"next"`), time.Second)
	if err != nil || decodeFrame(t, reply)["id"] != "c-echo" {
		t.Fatalf("%v %s", err, reply)
	}
	s.proc.mu.Lock()
	pending := len(s.proc.pending)
	s.proc.mu.Unlock()
	if pending != 0 {
		t.Fatalf("%d calls still pending", pending)
	}
}

func TestDriverNotificationsAndDriverRequestsDoNotReachTheCaller(t *testing.T) {
	s := fakeSupervisor(t)
	reply, err := s.call(callTool("notify", "{}"), time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if decodeFrame(t, reply)["result"].(map[string]any)["after"] != "notification" {
		t.Fatalf("got %s", reply)
	}
	if s.proc.notifications.Load() != 1 {
		t.Fatalf("notifications %d", s.proc.notifications.Load())
	}
}

func TestDriverReplyOverOneMiBIsRefused(t *testing.T) {
	s := fakeSupervisor(t)
	if _, err := s.call(callTool("big", "{}"), 2*time.Second); driverCode(err) != "reply_too_large" {
		t.Fatal(err)
	}
	if _, err := s.call(callTool("echo", "1"), time.Second); err != nil {
		t.Fatal("driver unusable after an oversized reply:", err)
	}
	if _, err := s.call([]byte(`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":"`+strings.Repeat("y", maxDriverFrame)+`"}}`), time.Second); driverCode(err) != "invalid_request" {
		t.Fatal("frame over 1 MiB accepted:", err)
	}
}

func TestDriverExitIsReportedRestartedAndLoopGuarded(t *testing.T) {
	s := fakeSupervisor(t)
	_, err := s.call(callTool("crash", "{}"), 2*time.Second)
	if driverCode(err) != "driver_exited" || !strings.Contains(err.Error(), "crashed on purpose") {
		t.Fatal(err)
	}
	if _, err = s.call(callTool("echo", "1"), time.Second); err != nil {
		t.Fatal("not restarted:", err)
	}
	for range driverMaxStarts {
		_, err = s.call(callTool("crash", "{}"), 2*time.Second)
	}
	if driverCode(err) != "driver_unavailable" {
		t.Fatal("crash loop not guarded:", err)
	}
}

func TestDriverRPCOverVsockCarriesLargeFramesOnly(t *testing.T) {
	saved := drivers
	drivers = fakeSupervisor(t)
	t.Cleanup(func() { drivers = saved })
	text := strings.Repeat("é", 50000) // 100 KB of user text: over 64 KiB
	arguments, _ := json.Marshal(map[string]any{"frame": json.RawMessage(callTool("echo", fmt.Sprintf("%q", text))), "deadline_ms": 2000})
	roundTrip := func(req request) response {
		line, _ := json.Marshal(req)
		a, b := net.Pipe()
		go serve(a, false)
		go func() { _, _ = b.Write(append(line, '\n')) }()
		var reply response
		if err := json.NewDecoder(b).Decode(&reply); err != nil {
			t.Fatal(err)
		}
		b.Close()
		return reply
	}
	reply := roundTrip(request{ID: "d1", Method: "driver_rpc", Arguments: arguments})
	if reply.Error != nil {
		t.Fatal(reply.Error)
	}
	frame, _ := json.Marshal(reply.Result.(map[string]any)["frame"])
	if !strings.Contains(string(frame), text) {
		t.Fatal("text not echoed")
	}
	reply = roundTrip(request{ID: "big-info", Method: "info", Data: strings.Repeat("x", maxRequest)})
	if reply.Error == nil || reply.Error.Code != "invalid_request" {
		t.Fatal("other methods must keep the 64 KiB limit", reply)
	}
	reply = roundTrip(request{ID: "d2", Method: "driver_rpc", Arguments: json.RawMessage(`{"frame":{"jsonrpc":"2.0","id":1,"method":"tools/list"},"deadline_ms":700000}`)})
	if reply.Error == nil || reply.Error.Code != "invalid_request" {
		t.Fatal("deadline over 10 minutes accepted", reply)
	}
}

func TestDriverPortAnswersOutOfOrderWithCallerIDs(t *testing.T) {
	saved := drivers
	drivers = fakeSupervisor(t)
	t.Cleanup(func() { drivers = saved })
	host, guest := net.Pipe()
	go func() { _ = serveDriverFrames(guest) }()
	go func() {
		_, _ = io.WriteString(host, string(callTool("slow", "{}"))+"\n")
		_, _ = io.WriteString(host, string(callTool("echo", `"fast"`))+"\n")
		_, _ = io.WriteString(host, `{"jsonrpc":"2.0","id":"bad","method":"initialize"}`+"\n")
	}()
	replies := bufio.NewScanner(host)
	var ids []string
	for len(ids) < 3 && replies.Scan() {
		value := decodeFrame(t, replies.Bytes())
		ids = append(ids, value["id"].(string))
		if value["id"] == "bad" && value["error"].(map[string]any)["data"].(map[string]any)["veynode_code"] != "invalid_request" {
			t.Fatalf("got %s", replies.Bytes())
		}
	}
	host.Close()
	if strings.Join(ids, ",") != "bad,c-echo,c-slow" && strings.Join(ids, ",") != "c-echo,bad,c-slow" {
		t.Fatalf("order %v", ids)
	}
}

func TestProcessGroupsReadsTheSessionUsersGroups(t *testing.T) {
	status := t.TempDir() + "/status"
	_ = os.WriteFile(status, []byte("Name:\txfce4-session\nUid:\t1000\t1000\t1000\t1000\nGroups:\t44 105 993 1000 \n"), 0o600)
	if got := fmt.Sprint(processGroups(status)); got != "[44 105 993 1000]" {
		t.Fatal(got)
	}
}
