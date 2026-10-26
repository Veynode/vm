package main

// Platform-neutral parts of the browser desktop stream (desktop_linux.go):
// scanout tile diffing and host input validation/encoding.

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
)

const (
	desktopTile  = 64
	desktopMaxIn = 256
)

// Linux input event codes (include/uapi/linux/input-event-codes.h).
const (
	evSyn        = 0
	evKey        = 1
	evRel        = 2
	evAbs        = 3
	relHWheel    = 6
	relWheel     = 8
	absX         = 0
	absY         = 1
	btnLeft      = 0x110
	btnMiddle    = 0x112
	absExtent    = 32767
	keyboardLast = 248
)

// tileDiff keeps a packed copy of the visible scanout.
type tileDiff struct {
	width, height int
	shadow        []byte
}

// capture appends one rectangle per 64-row band spanning its changed tiles.
func (s *tileDiff) capture(mem []byte, pitch, originX, originY int, emit func(x, y, w, h int, rows []byte) error) (bool, error) {
	stride := s.width * 4
	full := s.shadow == nil
	if full {
		s.shadow = make([]byte, stride*s.height)
	}
	tiles := (s.width + desktopTile - 1) / desktopTile
	dirty := make([]bool, tiles)
	sent := false
	for top := 0; top < s.height; top += desktopTile {
		rows := min(desktopTile, s.height-top)
		first, last := tiles, -1
		for t := range dirty {
			dirty[t] = full
		}
		if full {
			first, last = 0, tiles-1
		} else {
			for y := top; y < top+rows; y++ {
				src := mem[(originY+y)*pitch+originX*4:]
				dst := s.shadow[y*stride:]
				for t := 0; t < tiles; t++ {
					if dirty[t] {
						continue
					}
					a, b := t*desktopTile*4, min((t+1)*desktopTile, s.width)*4
					if !bytes.Equal(src[a:b], dst[a:b]) {
						dirty[t] = true
						first, last = min(first, t), max(last, t)
					}
				}
			}
		}
		if last < 0 {
			continue
		}
		x, w := first*desktopTile, min((last+1)*desktopTile, s.width)-first*desktopTile
		body := make([]byte, 0, w*rows*4)
		for y := top; y < top+rows; y++ {
			src := mem[(originY+y)*pitch+(originX+x)*4:][:w*4]
			copy(s.shadow[y*stride+x*4:], src)
			body = append(body, s.shadow[y*stride+x*4:][:w*4]...)
		}
		if err := emit(x, top, w, rows, body); err != nil {
			return sent, err
		}
		sent = true
	}
	return sent, nil
}

type desktopEvent struct {
	T    string `json:"t"`
	X    *int   `json:"x"`
	Y    *int   `json:"y"`
	Code int    `json:"code"`
	Down bool   `json:"down"`
	DX   int    `json:"dx"`
	DY   int    `json:"dy"`
}

func inputRecord(buf []byte, kind, code uint16, value int32) []byte {
	var event [24]byte // struct input_event on 64-bit: timeval, type, code, value
	binary.LittleEndian.PutUint16(event[16:], kind)
	binary.LittleEndian.PutUint16(event[18:], code)
	binary.LittleEndian.PutUint32(event[20:], uint32(value))
	return append(buf, event[:]...)
}

// encodeDesktopInput validates host input and turns it into input_event
// records for the keyboard and tablet devices.
func encodeDesktopInput(events []desktopEvent) (keyboard, tablet []byte, err error) {
	if len(events) == 0 || len(events) > desktopMaxIn {
		return nil, nil, errors.New("input batch outside bound")
	}
	for _, e := range events {
		switch e.T {
		case "abs":
			if e.X == nil || e.Y == nil || *e.X < 0 || *e.X > absExtent || *e.Y < 0 || *e.Y > absExtent {
				return nil, nil, errors.New("abs outside 0..32767")
			}
			tablet = inputRecord(tablet, evAbs, absX, int32(*e.X))
			tablet = inputRecord(tablet, evAbs, absY, int32(*e.Y))
			tablet = inputRecord(tablet, evSyn, 0, 0)
		case "key":
			value := int32(0)
			if e.Down {
				value = 1
			}
			switch {
			case e.Code >= btnLeft && e.Code <= btnMiddle:
				tablet = inputRecord(tablet, evKey, uint16(e.Code), value)
				tablet = inputRecord(tablet, evSyn, 0, 0)
			case e.Code >= 1 && e.Code <= keyboardLast:
				keyboard = inputRecord(keyboard, evKey, uint16(e.Code), value)
				keyboard = inputRecord(keyboard, evSyn, 0, 0)
			default:
				return nil, nil, fmt.Errorf("key code %d outside bound", e.Code)
			}
		case "wheel":
			if e.DX < -10 || e.DX > 10 || e.DY < -10 || e.DY > 10 {
				return nil, nil, errors.New("wheel outside bound")
			}
			// Browser dy > 0 scrolls down; evdev REL_WHEEL > 0 scrolls up.
			if e.DY != 0 {
				tablet = inputRecord(tablet, evRel, relWheel, int32(-e.DY))
			}
			if e.DX != 0 {
				tablet = inputRecord(tablet, evRel, relHWheel, int32(e.DX))
			}
			tablet = inputRecord(tablet, evSyn, 0, 0)
		default:
			return nil, nil, fmt.Errorf("unknown input type %q", e.T)
		}
	}
	return keyboard, tablet, nil
}
