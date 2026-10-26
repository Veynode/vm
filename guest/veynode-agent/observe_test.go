package main

import (
	"bytes"
	"encoding/json"
	"image/png"
	"strings"
	"sync"
	"testing"
	"time"
)

type fakeScreen struct {
	sync.Mutex
	img     screenImage
	applied [][]desktopEvent
	fail    bool
}

func newFakeScreen(width, height int) *fakeScreen {
	return &fakeScreen{img: screenImage{pix: make([]byte, width*height*4), width: width, height: height}}
}

func (f *fakeScreen) grab() (screenImage, error) {
	f.Lock()
	defer f.Unlock()
	return screenImage{pix: bytes.Clone(f.img.pix), width: f.img.width, height: f.img.height}, nil
}

func (f *fakeScreen) size() (int, int, error) {
	f.Lock()
	defer f.Unlock()
	return f.img.width, f.img.height, nil
}

func (f *fakeScreen) apply(events []desktopEvent) error {
	f.Lock()
	defer f.Unlock()
	if f.fail {
		return errTestApply
	}
	f.applied = append(f.applied, events)
	return nil
}

var errTestApply = observeFail("test", "apply failed")

func (f *fakeScreen) paint(x, y int, value byte) {
	f.Lock()
	defer f.Unlock()
	f.img.pix[(y*f.img.width+x)*4] = value
}

func call(t *testing.T, o *observer, method string, args any) map[string]any {
	t.Helper()
	raw, _ := json.Marshal(args)
	reply := o.handle(request{ID: "1", Method: method, Arguments: raw})
	if reply.Error != nil {
		t.Fatalf("%s: %s %s", method, reply.Error.Code, reply.Error.Message)
	}
	return reply.Result.(map[string]any)
}

func callError(o *observer, method string, args any) string {
	raw, _ := json.Marshal(args)
	reply := o.handle(request{ID: "1", Method: method, Arguments: raw})
	if reply.Error == nil {
		return ""
	}
	return reply.Error.Code
}

func TestScreenshotIsPNGWithFrameIdentity(t *testing.T) {
	screen := newFakeScreen(200, 100)
	screen.paint(10, 10, 0xff)
	o := newObserver(screen)
	shot := call(t, o, "screenshot", nil)
	data := shot["data_base64"].([]byte)
	if shot["mime_type"] != "image/png" || shot["byte_length"] != len(data) || len(shot["frame_id"].(string)) != 36 {
		t.Fatalf("unexpected screenshot %v", shot)
	}
	img, err := png.Decode(bytes.NewReader(data))
	if err != nil || img.Bounds().Dx() != 200 || img.Bounds().Dy() != 100 {
		t.Fatalf("decode %v %v", err, img.Bounds())
	}
	// BGRX blue byte 0xff becomes RGB blue.
	if r, g, b, a := img.At(10, 10).RGBA(); r != 0 || g != 0 || b != 0xffff || a != 0xffff {
		t.Fatalf("pixel %x %x %x %x", r, g, b, a)
	}
	if shot["source_generation"] != uint64(1) {
		t.Fatalf("generation %v", shot["source_generation"])
	}
	again := call(t, o, "screenshot", nil)
	if again["source_generation"] != uint64(1) || again["frame_id"] == shot["frame_id"] {
		t.Fatal("same geometry must keep the generation and get a new frame id")
	}
}

func TestBusyScreenFallsBackToJPEG(t *testing.T) {
	screen := newFakeScreen(800, 600)
	seed := uint32(1)
	for i := range screen.img.pix {
		seed = seed*1664525 + 1013904223
		screen.img.pix[i] = byte(seed >> 24)
	}
	shot := call(t, newObserver(screen), "screenshot", nil)
	if shot["mime_type"] != "image/jpeg" || shot["byte_length"].(int) > observeMaxImage {
		t.Fatalf("noise screenshot %v %v", shot["mime_type"], shot["byte_length"])
	}
}

func stepArgs(shot map[string]any, begin bool, events string) map[string]any {
	var parsed []desktopEvent
	if err := json.Unmarshal([]byte(events), &parsed); err != nil {
		panic(err)
	}
	return map[string]any{"operation_id": strings.Repeat("b", 32), "begin": begin,
		"frame_id": shot["frame_id"], "source_generation": shot["source_generation"], "width": shot["width"], "height": shot["height"],
		"events": parsed, "x": 5, "keys": []string{"a"}}
}

func TestInputIsFrameBoundAndReleasedOnCleanup(t *testing.T) {
	screen := newFakeScreen(320, 200)
	o := newObserver(screen)
	shot := call(t, o, "screenshot", nil)
	if code := callError(o, "input_step", stepArgs(shot, true, `[{"t":"key","code":272,"down":true}]`)); code != "invalid_request" {
		t.Fatalf("button before a position: %q", code)
	}
	call(t, o, "input_step", stepArgs(shot, true, `[{"t":"abs","x":100,"y":200}]`))
	if code := callError(o, "input_step", stepArgs(shot, true, `[{"t":"abs","x":1,"y":2}]`)); code != "input_busy_or_expired" {
		t.Fatalf("second begin: %q", code)
	}
	call(t, o, "input_step", stepArgs(shot, false, `[{"t":"key","code":272,"down":true}]`))
	call(t, o, "input_step", stepArgs(shot, false, `[{"t":"key","code":29,"down":true}]`))
	if code := callError(o, "input_step", stepArgs(shot, false, `[{"t":"key","code":29,"down":true}]`)); code != "invalid_request" {
		t.Fatalf("double press: %q", code)
	}
	cleanup := call(t, o, "input_cleanup", map[string]any{"operation_id": strings.Repeat("b", 32)})
	if cleanup["released"] != true {
		t.Fatalf("cleanup %v", cleanup)
	}
	last := screen.applied[len(screen.applied)-1]
	if len(last) != 2 || last[0].Code != 29 || last[1].Code != 272 || last[0].Down || last[1].Down {
		t.Fatalf("release order %+v", last)
	}
	if again := call(t, o, "input_cleanup", map[string]any{"operation_id": strings.Repeat("b", 32)}); again["released"] != false {
		t.Fatalf("second cleanup %v", again)
	}
	// A geometry change makes the frame stale.
	screen.img = screenImage{pix: make([]byte, 640*400*4), width: 640, height: 400}
	if code := callError(o, "input_step", stepArgs(shot, true, `[{"t":"abs","x":1,"y":2}]`)); code != "stale_frame" {
		t.Fatalf("stale frame accepted: %q", code)
	}
	fresh := call(t, o, "screenshot", nil)
	if fresh["source_generation"] != uint64(2) {
		t.Fatalf("generation after resize %v", fresh["source_generation"])
	}
	expired := stepArgs(fresh, true, `[{"t":"abs","x":1,"y":2}]`)
	o.now = func() time.Time { return time.Now().Add(observeFrameTTL + time.Second) }
	if code := callError(o, "input_step", expired); code != "stale_frame" {
		t.Fatalf("expired frame accepted: %q", code)
	}
}

func TestFailedInputReleasesHeldKeys(t *testing.T) {
	screen := newFakeScreen(320, 200)
	o := newObserver(screen)
	shot := call(t, o, "screenshot", nil)
	call(t, o, "input_step", stepArgs(shot, true, `[{"t":"key","code":42,"down":true}]`))
	screen.fail = true
	if code := callError(o, "input_step", stepArgs(shot, false, `[{"t":"key","code":30,"down":true}]`)); code != "input_cleanup_unconfirmed" {
		t.Fatalf("failed step: %q", code)
	}
	screen.fail = false
	if cleanup := call(t, o, "input_cleanup", map[string]any{"operation_id": strings.Repeat("b", 32)}); cleanup["released"] != false {
		t.Fatalf("operation survived a failed step: %v", cleanup)
	}
}

func TestWaitReportsChangedRegionsAfterSettling(t *testing.T) {
	screen := newFakeScreen(300, 200)
	o := newObserver(screen)
	shot := call(t, o, "screenshot", nil)
	quiet := call(t, o, "screen_wait", map[string]any{"frame_id": shot["frame_id"], "timeout_ms": 60})
	if quiet["changed"] != false {
		t.Fatalf("still screen reported change: %v", quiet)
	}
	go func() {
		time.Sleep(40 * time.Millisecond)
		screen.paint(10, 10, 1)
		screen.paint(140, 150, 1)
		time.Sleep(40 * time.Millisecond)
		screen.paint(70, 10, 1) // next to the first change: same group
	}()
	result := call(t, o, "screen_wait", map[string]any{"frame_id": shot["frame_id"], "timeout_ms": 2000, "settle_ms": 150})
	regions := result["regions"].([]screenRect)
	if result["changed"] != true || result["settled"] != true || len(regions) != 2 {
		t.Fatalf("wait %v", result)
	}
	if regions[0] != (screenRect{0, 0, 128, 64}) || regions[1] != (screenRect{128, 128, 64, 64}) {
		t.Fatalf("regions %v", regions)
	}
	// A region filter ignores changes elsewhere.
	go func() {
		time.Sleep(40 * time.Millisecond)
		screen.paint(10, 10, 2)
	}()
	filtered := call(t, o, "screen_wait", map[string]any{"timeout_ms": 200, "region": map[string]int{"x": 200, "y": 100, "width": 100, "height": 100}})
	if filtered["changed"] != false {
		t.Fatalf("change outside region reported: %v", filtered)
	}
	if code := callError(o, "screen_wait", map[string]any{"timeout_ms": 30000}); code != "invalid_request" {
		t.Fatalf("unbounded wait accepted: %q", code)
	}
}

func TestWaitSettlesThroughCaretBlink(t *testing.T) {
	screen := newFakeScreen(300, 200)
	o := newObserver(screen)
	shot := call(t, o, "screenshot", nil)
	stop := make(chan struct{})
	done := make(chan struct{})
	go func() {
		defer close(done)
		time.Sleep(30 * time.Millisecond)
		screen.paint(150, 100, 9) // a window appears
		for on := byte(1); ; on ^= 1 {
			select {
			case <-stop:
				return
			case <-time.After(40 * time.Millisecond):
				screen.paint(20, 20, on) // the caret blinks
			}
		}
	}()
	result := call(t, o, "screen_wait", map[string]any{"frame_id": shot["frame_id"], "timeout_ms": 2000, "settle_ms": 200})
	close(stop)
	<-done
	if result["changed"] != true || result["settled"] != true || result["elapsed_ms"].(int64) > 1500 {
		t.Fatalf("blinking caret kept the wait from settling: %v", result)
	}
}

func TestDesktopMethodsNeedDesktopStream(t *testing.T) {
	if reply := handle(request{ID: "1", Method: "screenshot"}, false); reply.Error == nil || reply.Error.Code != "unsupported" {
		t.Fatalf("screenshot without desktop: %+v", reply)
	}
}
