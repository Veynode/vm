package main

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"testing"
)

type rect struct{ x, y, w, h int }

func captureRects(t *testing.T, d *tileDiff, mem []byte, pitch, ox, oy int) ([]rect, [][]byte) {
	t.Helper()
	var rects []rect
	var bodies [][]byte
	if _, err := d.capture(mem, pitch, ox, oy, func(x, y, w, h int, rows []byte) error {
		if len(rows) != w*h*4 {
			t.Fatalf("rect %dx%d carries %d bytes", w, h, len(rows))
		}
		rects = append(rects, rect{x, y, w, h})
		bodies = append(bodies, append([]byte(nil), rows...))
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	return rects, bodies
}

func TestTileDiffSendsFullFrameThenOnlyChangedBands(t *testing.T) {
	// 200x130 visible at (8, 2) inside a padded 256-pixel-pitch framebuffer.
	const width, height, ox, oy, pitch = 200, 130, 8, 2, 256 * 4
	mem := make([]byte, pitch*(oy+height))
	d := &tileDiff{width: width, height: height}
	rects, _ := captureRects(t, d, mem, pitch, ox, oy)
	want := []rect{{0, 0, 200, 64}, {0, 64, 200, 64}, {0, 128, 200, 2}}
	if len(rects) != len(want) {
		t.Fatalf("first capture %v, want %v", rects, want)
	}
	for i := range want {
		if rects[i] != want[i] {
			t.Fatalf("first capture %v, want %v", rects, want)
		}
	}
	if rects, _ = captureRects(t, d, mem, pitch, ox, oy); len(rects) != 0 {
		t.Fatalf("unchanged frame sent %v", rects)
	}
	// Change one pixel in tile column 1 and one in column 3 of the middle band.
	pixel := func(x, y int) []byte { return mem[(oy+y)*pitch+(ox+x)*4:][:4] }
	copy(pixel(70, 100), []byte{1, 2, 3, 4})
	copy(pixel(199, 127), []byte{5, 6, 7, 8})
	rects, bodies := captureRects(t, d, mem, pitch, ox, oy)
	if len(rects) != 1 || rects[0] != (rect{64, 64, 136, 64}) {
		t.Fatalf("changed capture %v", rects)
	}
	at := func(x, y int) []byte { return bodies[0][((y-64)*136+(x-64))*4:][:4] }
	if !bytes.Equal(at(70, 100), []byte{1, 2, 3, 4}) || !bytes.Equal(at(199, 127), []byte{5, 6, 7, 8}) {
		t.Fatal("changed pixels missing from rectangle body")
	}
	if rects, _ = captureRects(t, d, mem, pitch, ox, oy); len(rects) != 0 {
		t.Fatalf("shadow not updated: %v", rects)
	}
}

func decodeEvents(t *testing.T, raw []byte) [][3]int {
	t.Helper()
	if len(raw)%24 != 0 {
		t.Fatalf("input_event stream of %d bytes", len(raw))
	}
	var out [][3]int
	for i := 0; i < len(raw); i += 24 {
		out = append(out, [3]int{int(binary.LittleEndian.Uint16(raw[i+16:])), int(binary.LittleEndian.Uint16(raw[i+18:])), int(int32(binary.LittleEndian.Uint32(raw[i+20:])))})
	}
	return out
}

func TestDesktopInputEncoding(t *testing.T) {
	var events []desktopEvent
	if err := json.Unmarshal([]byte(`[{"t":"abs","x":100,"y":32767},{"t":"key","code":272,"down":true},{"t":"key","code":30,"down":true},{"t":"wheel","dx":1,"dy":2}]`), &events); err != nil {
		t.Fatal(err)
	}
	keyboard, tablet, err := encodeDesktopInput(events)
	if err != nil {
		t.Fatal(err)
	}
	if got := decodeEvents(t, keyboard); len(got) != 2 || got[0] != [3]int{evKey, 30, 1} || got[1] != [3]int{evSyn, 0, 0} {
		t.Fatalf("keyboard %v", got)
	}
	want := [][3]int{{evAbs, absX, 100}, {evAbs, absY, 32767}, {evSyn, 0, 0}, {evKey, btnLeft, 1}, {evSyn, 0, 0},
		{evRel, relWheel, -2}, {evRel, relHWheel, 1}, {evSyn, 0, 0}}
	got := decodeEvents(t, tablet)
	if len(got) != len(want) {
		t.Fatalf("tablet %v", got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("tablet %v, want %v", got, want)
		}
	}
}

func TestDesktopInputRejectsOutOfBound(t *testing.T) {
	for _, raw := range []string{
		`[]`,
		`[{"t":"abs","x":-1,"y":0}]`,
		`[{"t":"abs","x":0}]`,
		`[{"t":"abs","x":0,"y":32768}]`,
		`[{"t":"key","code":0,"down":true}]`,
		`[{"t":"key","code":249,"down":true}]`,
		`[{"t":"key","code":275,"down":true}]`,
		`[{"t":"wheel","dx":0,"dy":11}]`,
		`[{"t":"exec"}]`,
	} {
		var events []desktopEvent
		if err := json.Unmarshal([]byte(raw), &events); err != nil {
			t.Fatal(err)
		}
		if _, _, err := encodeDesktopInput(events); err == nil {
			t.Errorf("accepted %s", raw)
		}
	}
	many := make([]desktopEvent, desktopMaxIn+1)
	for i := range many {
		many[i] = desktopEvent{T: "key", Code: 30}
	}
	if _, _, err := encodeDesktopInput(many); err == nil {
		t.Error("accepted oversized batch")
	}
}
