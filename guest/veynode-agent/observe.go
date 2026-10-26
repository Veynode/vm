package main

// MCP observation and input for desktop guests (Apple Virtualization Linux,
// --desktop-stream): screenshots of the scanout, frame-bound physical input
// through the uinput devices, and waiting for the screen to change. Replies
// match the macOS Screen Sharing service (native/worker/ScreenSharing.swift),
// so the daemon handles both guests alike. The daemon translates planned
// input into evdev events (crates/control/src/desktop_input.rs).

import (
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash/maphash"
	"image"
	"image/jpeg"
	"image/png"
	"slices"
	"sync"
	"time"
)

const (
	observeFrameTTL    = 5 * time.Second
	observeOperation   = 4 * time.Second
	observeMaxFrames   = 16
	observeMaxImage    = 700 * 1024 // fits the worker's 1 MiB reply once base64 encoded
	observeMaxStep     = 16
	observeWaitPoll    = 30 * time.Millisecond
	observeMaxWait     = 25 * time.Second
	observeMaxSettle   = 5 * time.Second
	observeMaxRegions  = 32
	observeHashedTiles = desktopTile
)

// screenImage is a packed BGRX copy of the visible scanout.
type screenImage struct {
	pix           []byte
	width, height int
}

type screenSource interface {
	grab() (screenImage, error)
	size() (int, int, error)
	apply([]desktopEvent) error
}

type observeError struct{ code, message string }

func (e *observeError) Error() string { return e.message }

func observeFail(code, format string, args ...any) error {
	return &observeError{code, fmt.Sprintf(format, args...)}
}

type frameRecord struct {
	id            string
	generation    uint64
	width, height int
	captured      time.Time
	tiles         []uint64
}

type observer struct {
	source screenSource
	now    func() time.Time
	seed   maphash.Seed

	mu            sync.Mutex
	frames        []frameRecord
	generation    uint64
	width, height int
	// The one active input operation and the keys/buttons it holds.
	operation  string
	deadline   time.Time
	held       []int
	positioned bool
}

func newObserver(source screenSource) *observer {
	return &observer{source: source, now: time.Now, seed: maphash.MakeSeed()}
}

// desktop is set by --desktop-stream.
var desktop *observer

func (o *observer) handle(req request) response {
	var result any
	var err error
	switch req.Method {
	case "screenshot":
		result, err = o.screenshot()
	case "screen_wait", "input_step", "input_cleanup":
		var args observeArgs
		if err = json.Unmarshal(req.Arguments, &args); err != nil {
			err = observeFail("invalid_request", "invalid arguments")
			break
		}
		switch req.Method {
		case "screen_wait":
			result, err = o.wait(args)
		case "input_step":
			result, err = o.step(args)
		default:
			result, err = o.cleanup(args)
		}
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

// observeArgs are the daemon's arguments; unknown fields (x, y, keys, ...)
// are the original MCP request and are ignored here.
type observeArgs struct {
	OperationID      string         `json:"operation_id"`
	Begin            *bool          `json:"begin"`
	FrameID          string         `json:"frame_id"`
	SourceGeneration *uint64        `json:"source_generation"`
	Width            *int           `json:"width"`
	Height           *int           `json:"height"`
	Events           []desktopEvent `json:"events"`
	TimeoutMS        int            `json:"timeout_ms"`
	SettleMS         int            `json:"settle_ms"`
	Region           *screenRect    `json:"region"`
}

type screenRect struct {
	X int `json:"x"`
	Y int `json:"y"`
	W int `json:"width"`
	H int `json:"height"`
}

func newUUID() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	b[6] = b[6]&0x0f | 0x40
	b[8] = b[8]&0x3f | 0x80
	h := hex.EncodeToString(b[:])
	return h[0:8] + "-" + h[8:12] + "-" + h[12:16] + "-" + h[16:20] + "-" + h[20:32]
}

// encodeScreen prefers lossless PNG (best for reading text) and falls back to
// JPEG when a busy screen would exceed the reply bound.
func encodeScreen(img screenImage) (data []byte, mime string, quality int, err error) {
	rgba := image.NewRGBA(image.Rect(0, 0, img.width, img.height))
	for i := 0; i < len(img.pix); i += 4 {
		rgba.Pix[i], rgba.Pix[i+1], rgba.Pix[i+2], rgba.Pix[i+3] = img.pix[i+2], img.pix[i+1], img.pix[i], 0xff
	}
	var out bytes.Buffer
	encoder := png.Encoder{CompressionLevel: png.BestSpeed}
	if err = encoder.Encode(&out, rgba); err != nil {
		return nil, "", 0, err
	}
	if out.Len() <= observeMaxImage {
		return out.Bytes(), "image/png", 0, nil
	}
	for _, quality = range []int{85, 70, 55, 40} {
		out.Reset()
		if err = jpeg.Encode(&out, rgba, &jpeg.Options{Quality: quality}); err != nil {
			return nil, "", 0, err
		}
		if out.Len() <= observeMaxImage {
			return out.Bytes(), "image/jpeg", quality, nil
		}
	}
	return nil, "", 0, observeFail("screenshot_failed", "screenshot exceeds %d KiB even as JPEG", observeMaxImage/1024)
}

// tileHashes hashes each 64x64 tile, row-major.
func (o *observer) tileHashes(img screenImage) []uint64 {
	cols, rows := (img.width+observeHashedTiles-1)/observeHashedTiles, (img.height+observeHashedTiles-1)/observeHashedTiles
	out := make([]uint64, cols*rows)
	stride := img.width * 4
	var h maphash.Hash
	h.SetSeed(o.seed)
	for ty := range rows {
		for tx := range cols {
			h.Reset()
			x0, x1 := tx*observeHashedTiles*4, min((tx+1)*observeHashedTiles, img.width)*4
			for y := ty * observeHashedTiles; y < min((ty+1)*observeHashedTiles, img.height); y++ {
				_, _ = h.Write(img.pix[y*stride+x0 : y*stride+x1])
			}
			out[ty*cols+tx] = h.Sum64()
		}
	}
	return out
}

// observe records a grabbed image as a frame; the generation advances when
// the display geometry changes, which invalidates earlier frames for input.
func (o *observer) observe(img screenImage, captured time.Time) frameRecord {
	frame := frameRecord{id: newUUID(), width: img.width, height: img.height, captured: captured, tiles: o.tileHashes(img)}
	o.mu.Lock()
	defer o.mu.Unlock()
	if img.width != o.width || img.height != o.height {
		o.generation++
		o.width, o.height = img.width, img.height
	}
	frame.generation = o.generation
	o.frames = append(o.frames, frame)
	if len(o.frames) > observeMaxFrames {
		o.frames = slices.Delete(o.frames, 0, len(o.frames)-observeMaxFrames)
	}
	return frame
}

func (o *observer) screenshot() (map[string]any, error) {
	requested := o.now()
	img, err := o.source.grab()
	if err != nil {
		return nil, observeFail("screenshot_failed", "%v", err)
	}
	captured := o.now()
	data, mime, quality, err := encodeScreen(img)
	if err != nil {
		return nil, err
	}
	frame := o.observe(img, captured)
	sum := sha256.Sum256(data)
	result := map[string]any{"frame_id": frame.id, "mime_type": mime, "data_base64": data, "byte_length": len(data),
		"sha256": hex.EncodeToString(sum[:]), "width": img.width, "height": img.height,
		"source_width": img.width, "source_height": img.height, "image_width": img.width, "image_height": img.height,
		"coordinate_space": "guest-display-physical-pixels", "source_generation": frame.generation, "input_anchor_available": true,
		"capture_requested_at_us": requested.UnixMicro(), "captured_at_us": captured.UnixMicro(), "includes_cursor": false,
		"capture_duration_ms": float64(o.now().Sub(requested).Microseconds()) / 1000, "capture_source": "guest-drm-scanout", "resized": false}
	if quality > 0 {
		result["jpeg_quality"] = float64(quality) / 100
	}
	return result, nil
}

func isHex(s string, n int) bool {
	if len(s) != n {
		return false
	}
	_, err := hex.DecodeString(s)
	return err == nil
}

// release lifts whatever the operation holds and ends it.
func (o *observer) release() bool {
	held := o.held
	o.operation, o.held, o.positioned = "", nil, false
	if len(held) == 0 {
		return true
	}
	events := make([]desktopEvent, 0, len(held))
	for i := len(held) - 1; i >= 0; i-- {
		events = append(events, desktopEvent{T: "key", Code: held[i]})
	}
	return o.source.apply(events) == nil
}

func (o *observer) step(args observeArgs) (map[string]any, error) {
	if !isHex(args.OperationID, 32) || args.Begin == nil ||
		args.SourceGeneration == nil || args.Width == nil || args.Height == nil || len(args.Events) == 0 || len(args.Events) > observeMaxStep {
		return nil, observeFail("invalid_request", "invalid bounded physical input request")
	}
	width, height, err := o.source.size()
	if err != nil {
		return nil, observeFail("stale_frame", "display unavailable: %v", err)
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	now := o.now()
	i := slices.IndexFunc(o.frames, func(f frameRecord) bool { return f.id == args.FrameID })
	if i < 0 || now.Sub(o.frames[i].captured) > observeFrameTTL || o.frames[i].generation != *args.SourceGeneration ||
		o.frames[i].width != *args.Width || o.frames[i].height != *args.Height || width != o.frames[i].width || height != o.frames[i].height {
		return nil, observeFail("stale_frame", "capture a fresh screenshot after display or geometry changes")
	}
	frame := o.frames[i]
	if *args.Begin && o.operation != "" && now.After(o.deadline) {
		o.release() // an abandoned operation; its owner never cleaned up
	}
	if *args.Begin && o.operation != "" ||
		!*args.Begin && (o.operation != args.OperationID || now.After(o.deadline)) {
		return nil, observeFail("input_busy_or_expired", "input operation is active, replaced, or expired")
	}
	// Validate the whole batch against the held state before sending any of it.
	held, positioned := slices.Clone(o.held), !*args.Begin && o.positioned
	for _, e := range args.Events {
		switch e.T {
		case "abs":
			if len(args.Events) != 1 {
				return nil, observeFail("invalid_request", "a pointer move is sent on its own")
			}
			positioned = true
		case "key":
			if e.Code >= btnLeft && !positioned {
				return nil, observeFail("invalid_request", "mouse action requires an absolute position in this operation")
			}
			at := slices.Index(held, e.Code)
			if e.Down == (at >= 0) {
				return nil, observeFail("invalid_request", "key or button %d already in that state", e.Code)
			}
			if e.Down {
				held = append(held, e.Code)
			} else {
				held = slices.Delete(held, at, at+1)
			}
		case "wheel":
			if !positioned {
				return nil, observeFail("invalid_request", "wheel requires an absolute position in this operation")
			}
		}
	}
	if _, _, err := encodeDesktopInput(args.Events); err != nil {
		return nil, observeFail("invalid_request", "%v", err)
	}
	if *args.Begin {
		o.operation, o.deadline, o.held, o.positioned = args.OperationID, now.Add(observeOperation), nil, false
	}
	if err := o.source.apply(args.Events); err != nil {
		if !o.release() {
			return nil, observeFail("input_cleanup_unconfirmed", "input was interrupted and release could not be sent: %v", err)
		}
		return nil, observeFail("input_failed", "%v", err)
	}
	o.held, o.positioned = held, positioned
	return map[string]any{"dispatched": true, "source_generation": frame.generation, "guest_delivery_confirmed": false, "coordinate_space": "captured-display-pixels"}, nil
}

func (o *observer) cleanup(args observeArgs) (map[string]any, error) {
	if !isHex(args.OperationID, 32) {
		return nil, observeFail("invalid_request", "operation_id required for cleanup")
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.operation != args.OperationID {
		return map[string]any{"released": false, "reason": "operation is no longer active"}, nil
	}
	if !o.release() {
		return nil, observeFail("input_cleanup_unconfirmed", "synthetic release could not be sent")
	}
	return map[string]any{"released": true, "synthetic_release_dispatched": true, "guest_delivery_confirmed": false}, nil
}

// wait blocks until the screen (or a region of it) differs from a baseline
// frame, then optionally until it has been still for settle_ms. The baseline
// is the given screenshot, or the screen when the call starts.
func (o *observer) wait(args observeArgs) (map[string]any, error) {
	timeout, settle := time.Duration(args.TimeoutMS)*time.Millisecond, time.Duration(args.SettleMS)*time.Millisecond
	if timeout <= 0 || timeout > observeMaxWait || settle < 0 || settle > observeMaxSettle {
		return nil, observeFail("invalid_request", "timeout_ms must be 1..25000 and settle_ms 0..5000")
	}
	start := o.now()
	var base frameRecord
	if args.FrameID != "" {
		o.mu.Lock()
		i := slices.IndexFunc(o.frames, func(f frameRecord) bool { return f.id == args.FrameID })
		if i >= 0 {
			base = o.frames[i]
		}
		o.mu.Unlock()
		if i < 0 {
			return nil, observeFail("stale_frame", "unknown frame_id; take a new screenshot")
		}
	} else {
		img, err := o.source.grab()
		if err != nil {
			return nil, observeFail("screenshot_failed", "%v", err)
		}
		base = frameRecord{width: img.width, height: img.height, tiles: o.tileHashes(img)}
	}
	if r := args.Region; r != nil && (r.X < 0 || r.Y < 0 || r.W <= 0 || r.H <= 0 || r.X+r.W > base.width || r.Y+r.H > base.height) {
		return nil, observeFail("invalid_request", "region outside the %dx%d screen", base.width, base.height)
	}
	var changed, previous []uint64
	var changedAt, stillSince time.Time
	resized := false
	// A blinking caret toggles one or two tiles between states already seen
	// during this wait; that does not keep the screen from settling.
	seen := map[int][]uint64{}
	blinking := func(tiles []uint64) bool {
		var differ []int
		for i := range tiles {
			if tiles[i] != previous[i] {
				differ = append(differ, i)
			}
		}
		blink := len(differ) <= 2
		for _, i := range differ {
			if !slices.Contains(seen[i], previous[i]) {
				seen[i] = append(seen[i], previous[i])
			}
			blink = blink && slices.Contains(seen[i], tiles[i])
			if !slices.Contains(seen[i], tiles[i]) && len(seen[i]) < 4 {
				seen[i] = append(seen[i], tiles[i])
			}
		}
		return blink
	}
	for {
		img, err := o.source.grab()
		if err != nil {
			return nil, observeFail("screenshot_failed", "%v", err)
		}
		now := o.now()
		tiles := o.tileHashes(img)
		if img.width != base.width || img.height != base.height {
			resized, changed, changedAt = true, tiles, now
			break
		}
		if changed == nil && len(changedTiles(base, tiles, args.Region)) > 0 {
			changed, changedAt, stillSince = tiles, now, now
		} else if changed != nil {
			if !slices.Equal(tiles, previous) && !blinking(tiles) {
				stillSince = now
			}
			changed = tiles
		}
		previous = tiles
		if changed != nil && now.Sub(stillSince) >= settle {
			break
		}
		if now.Sub(start) >= timeout {
			break
		}
		time.Sleep(observeWaitPoll)
	}
	result := map[string]any{"changed": changed != nil, "elapsed_ms": o.now().Sub(start).Milliseconds(), "width": base.width, "height": base.height}
	if changed == nil {
		return result, nil
	}
	result["first_change_ms"] = changedAt.Sub(start).Milliseconds()
	if resized {
		result["resized"] = true
		return result, nil
	}
	result["settled"] = o.now().Sub(stillSince) >= settle
	result["regions"] = tileRegions(base.width, base.height, changedTiles(base, changed, args.Region))
	return result, nil
}

// changedTiles lists the tile indexes that differ, limited to a region.
func changedTiles(base frameRecord, tiles []uint64, region *screenRect) []int {
	cols := (base.width + observeHashedTiles - 1) / observeHashedTiles
	var out []int
	for i := range tiles {
		if tiles[i] == base.tiles[i] {
			continue
		}
		if r := region; r != nil {
			x, y := i%cols*observeHashedTiles, i/cols*observeHashedTiles
			if x+observeHashedTiles <= r.X || y+observeHashedTiles <= r.Y || x >= r.X+r.W || y >= r.Y+r.H {
				continue
			}
		}
		out = append(out, i)
	}
	return out
}

// tileRegions merges changed tiles into the bounding boxes of their
// 8-connected groups, largest first.
func tileRegions(width, height int, tiles []int) []screenRect {
	cols := (width + observeHashedTiles - 1) / observeHashedTiles
	pending := map[int]bool{}
	for _, t := range tiles {
		pending[t] = true
	}
	var out []screenRect
	for _, t := range tiles {
		if !pending[t] {
			continue
		}
		delete(pending, t)
		minX, minY, maxX, maxY := t%cols, t/cols, t%cols, t/cols
		for queue := []int{t}; len(queue) > 0; {
			c := queue[0]
			queue = queue[1:]
			cx, cy := c%cols, c/cols
			minX, minY, maxX, maxY = min(minX, cx), min(minY, cy), max(maxX, cx), max(maxY, cy)
			for dy := -1; dy <= 1; dy++ {
				for dx := -1; dx <= 1; dx++ {
					nx, ny := cx+dx, cy+dy
					if n := ny*cols + nx; nx >= 0 && nx < cols && ny >= 0 && pending[n] {
						delete(pending, n)
						queue = append(queue, n)
					}
				}
			}
		}
		x, y := minX*observeHashedTiles, minY*observeHashedTiles
		out = append(out, screenRect{x, y, min((maxX+1)*observeHashedTiles, width) - x, min((maxY+1)*observeHashedTiles, height) - y})
	}
	slices.SortStableFunc(out, func(a, b screenRect) int { return b.W*b.H - a.W*a.H })
	if len(out) > observeMaxRegions {
		out = out[:observeMaxRegions]
	}
	return out
}
