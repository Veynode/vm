//go:build linux

package main

// Browser desktop for Apple Virtualization Linux guests. The host has no
// capture API for a Linux VZ display, so the guest polls the primary DRM
// scanout (every 8-33 ms, pollPacer in desktop.go), sends changed 64x64 tiles
// as BGRX rectangles over vsock 5001 and drives two uinput devices (keyboard,
// absolute tablet) from host input.
//
// host -> guest: JSON lines of at most 4 KiB. {"method":"start"} first, then
//   {"input":[{"t":"abs","x":0..32767,"y":0..32767} |
//             {"t":"key","code":evdev,"down":bool} | {"t":"wheel","dx","dy"}]}
//   {"audio":bool} and {"mic":"<base64>"}: Guest audio (desktop_audio_linux.go)
//   {"clipboard":"text"}, {"clipboard_watch":bool}, {"clipboard_get":true}:
//   the desktop's clipboard (clipboard_x11.go), answered by
//   {"event":"clipboard","text"} or {"event":"clipboard","error"}
// guest -> host: records of u32 big-endian length, u8 kind, body:
//   1 JSON event: {"event":"mode","width","height","format":"bgrx"}
//                 or {"event":"error","message"}
//   2 rectangle:  u16 BE x, y, w, h then w*h*4 bytes, rows packed
//   3 frame end:  u64 BE guest monotonic microseconds
//   4 audio:      u32 BE rate, u8 channels, s16le PCM

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"sync"
	"syscall"
	"time"
	"unsafe"
)

const (
	desktopPort    = 5001
	desktopMaxSide = 4096
	// A clipboard line carries up to 200 KiB of text, JSON-escaped.
	desktopMaxLine = 2 << 20
)

// DRM UAPI (include/uapi/drm/drm.h, drm_mode.h); fixed Linux ABI.
const (
	drmIoctlGetResources = 0xC04064A0
	drmIoctlGetCrtc      = 0xC06864A1
	drmIoctlMapDumb      = 0xC01064B3
	drmIoctlGetFB2       = 0xC06864CE
	drmIoctlGemClose     = 0x40086409
	drmFormatXRGB8888    = 0x34325258
	drmFormatARGB8888    = 0x34325241
	drmModeFBModifiers   = 2
)

type drmCardRes struct {
	FBs, Crtcs, Connectors, Encoders                     uint64
	CountFBs, CountCrtcs, CountConnectors, CountEncoders uint32
	MinWidth, MaxWidth, MinHeight, MaxHeight             uint32
}

type drmModeInfo struct {
	Clock                                         uint32
	HDisplay, HSyncStart, HSyncEnd, HTotal, HSkew uint16
	VDisplay, VSyncStart, VSyncEnd, VTotal, VScan uint16
	VRefresh, Flags, Type                         uint32
	Name                                          [32]byte
}

type drmCrtc struct {
	SetConnectors   uint64
	CountConnectors uint32
	ID, FB, X, Y    uint32
	GammaSize       uint32
	ModeValid       uint32
	Mode            drmModeInfo
}

type drmFBCmd2 struct {
	FB, Width, Height, Format, Flags uint32
	Handles, Pitches, Offsets        [4]uint32
	_                                uint32
	Modifier                         [4]uint64
}

type drmMapDumb struct {
	Handle, _ uint32
	Offset    uint64
}

func drmIoctl(fd uintptr, request uintptr, arg unsafe.Pointer) error {
	for {
		_, _, e := syscall.Syscall(syscall.SYS_IOCTL, fd, request, uintptr(arg))
		if e == syscall.EINTR || e == syscall.EAGAIN {
			continue
		}
		if e != 0 {
			return e
		}
		return nil
	}
}

// scanout maps the framebuffer of the first active CRTC and diffs it against
// a packed shadow copy.
type scanout struct {
	tileDiff
	card                    *os.File
	crtcs                   []uint32
	fb                      uint32
	mem                     []byte
	pitch, originX, originY int
}

func openScanout() (*scanout, error) {
	card, err := os.OpenFile("/dev/dri/card0", os.O_RDWR|syscall.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	var res drmCardRes
	if err = drmIoctl(card.Fd(), drmIoctlGetResources, unsafe.Pointer(&res)); err != nil || res.CountCrtcs == 0 || res.CountCrtcs > 16 {
		card.Close()
		return nil, fmt.Errorf("DRM resources unavailable: %v", err)
	}
	crtcs := make([]uint32, res.CountCrtcs)
	res = drmCardRes{Crtcs: uint64(uintptr(unsafe.Pointer(&crtcs[0]))), CountCrtcs: uint32(len(crtcs))}
	if err = drmIoctl(card.Fd(), drmIoctlGetResources, unsafe.Pointer(&res)); err != nil {
		card.Close()
		return nil, err
	}
	return &scanout{card: card, crtcs: crtcs[:res.CountCrtcs]}, nil
}

func (s *scanout) unmap() {
	if s.mem != nil {
		_ = syscall.Munmap(s.mem)
		s.mem = nil
	}
	s.fb = 0
}

func (s *scanout) close() {
	s.unmap()
	s.card.Close()
}

// refresh follows the active framebuffer; it reports whether the visible
// geometry changed (and the shadow was reset).
func (s *scanout) refresh() (bool, error) {
	var active drmCrtc
	for _, id := range s.crtcs {
		crtc := drmCrtc{ID: id}
		if err := drmIoctl(s.card.Fd(), drmIoctlGetCrtc, unsafe.Pointer(&crtc)); err != nil {
			return false, err
		}
		if crtc.FB != 0 && crtc.ModeValid != 0 {
			active = crtc
			break
		}
	}
	if active.FB == 0 {
		s.unmap()
		return false, errors.New("no active scanout (is the desktop running?)")
	}
	width, height := int(active.Mode.HDisplay), int(active.Mode.VDisplay)
	if active.FB == s.fb && int(active.X) == s.originX && int(active.Y) == s.originY && width == s.width && height == s.height {
		return false, nil
	}
	s.unmap()
	fb := drmFBCmd2{FB: active.FB}
	if err := drmIoctl(s.card.Fd(), drmIoctlGetFB2, unsafe.Pointer(&fb)); err != nil {
		return false, fmt.Errorf("GETFB2: %w", err)
	}
	if fb.Handles[0] == 0 {
		return false, errors.New("scanout handle unavailable (needs CAP_SYS_ADMIN)")
	}
	defer drmIoctl(s.card.Fd(), drmIoctlGemClose, unsafe.Pointer(&[2]uint32{fb.Handles[0], 0}))
	if fb.Format != drmFormatXRGB8888 && fb.Format != drmFormatARGB8888 {
		return false, fmt.Errorf("scanout format %08x is not XRGB8888", fb.Format)
	}
	if fb.Flags&drmModeFBModifiers != 0 && fb.Modifier[0] != 0 {
		return false, fmt.Errorf("scanout is not linear (modifier %x)", fb.Modifier[0])
	}
	if width < 16 || height < 16 || width > desktopMaxSide || height > desktopMaxSide ||
		int(active.X)+width > int(fb.Width) || int(active.Y)+height > int(fb.Height) || int(fb.Pitches[0]) < int(fb.Width)*4 {
		return false, fmt.Errorf("scanout geometry %dx%d in %dx%d outside bound", width, height, fb.Width, fb.Height)
	}
	dumb := drmMapDumb{Handle: fb.Handles[0]}
	if err := drmIoctl(s.card.Fd(), drmIoctlMapDumb, unsafe.Pointer(&dumb)); err != nil {
		return false, fmt.Errorf("MAP_DUMB: %w", err)
	}
	size := int(fb.Offsets[0]) + int(fb.Pitches[0])*int(fb.Height)
	mem, err := syscall.Mmap(int(s.card.Fd()), int64(dumb.Offset), size, syscall.PROT_READ, syscall.MAP_SHARED)
	if err != nil {
		return false, fmt.Errorf("mmap scanout: %w", err)
	}
	s.mem = mem[fb.Offsets[0]:]
	s.fb, s.pitch, s.originX, s.originY = active.FB, int(fb.Pitches[0]), int(active.X), int(active.Y)
	changed := width != s.width || height != s.height
	s.width, s.height = width, height
	// A new framebuffer (page flip, resize) is fully resent.
	s.shadow = nil
	return changed, nil
}

// uinput UAPI (include/uapi/linux/uinput.h).
const (
	uiSetEvBit  = 0x40045564
	uiSetKeyBit = 0x40045565
	uiSetRelBit = 0x40045566
	uiSetAbsBit = 0x40045567
	uiDevCreate = 0x5501
	uiDevSetup  = 0x405c5503
	uiAbsSetup  = 0x401c5504
)

type uinputSetup struct {
	Bustype, Vendor, Product, Version uint16
	Name                              [80]byte
	FFEffectsMax                      uint32
}

type uinputAbsSetup struct {
	Code, _                                 uint16
	Value, Min, Max, Fuzz, Flat, Resolution int32
}

func uinputIoctl(f *os.File, request uintptr, arg uintptr) error {
	if _, _, e := syscall.Syscall(syscall.SYS_IOCTL, f.Fd(), request, arg); e != 0 {
		return e
	}
	return nil
}

func uinputDevice(name string, product uint16, configure func(*os.File) error) (*os.File, error) {
	f, err := os.OpenFile("/dev/uinput", os.O_WRONLY|syscall.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	setup := uinputSetup{Bustype: 0x06 /* BUS_VIRTUAL */, Vendor: 0x7665, Product: product, Version: 1}
	copy(setup.Name[:], name)
	if err = configure(f); err == nil {
		err = uinputIoctl(f, uiDevSetup, uintptr(unsafe.Pointer(&setup)))
	}
	if err == nil {
		err = uinputIoctl(f, uiDevCreate, 0)
	}
	if err != nil {
		f.Close()
		return nil, fmt.Errorf("uinput %s: %w", name, err)
	}
	return f, nil
}

type desktopInput struct {
	keyboard, tablet *os.File
}

var (
	desktopInputOnce sync.Once
	desktopInputDev  *desktopInput
	desktopInputErr  error
)

// Devices are created once and live as long as the agent, so the desktop
// session sees one stable keyboard and pointer across viewers.
func sharedDesktopInput() (*desktopInput, error) {
	desktopInputOnce.Do(func() {
		keyboard, err := uinputDevice("VeynodeVM keyboard", 1, func(f *os.File) error {
			if err := uinputIoctl(f, uiSetEvBit, evKey); err != nil {
				return err
			}
			for code := 1; code <= keyboardLast; code++ {
				if err := uinputIoctl(f, uiSetKeyBit, uintptr(code)); err != nil {
					return err
				}
			}
			return nil
		})
		if err != nil {
			desktopInputErr = err
			return
		}
		// Shaped like a USB tablet (absolute axes, mouse buttons, wheels) so
		// udev and libinput treat it as an absolute pointer.
		tablet, err := uinputDevice("VeynodeVM tablet", 2, func(f *os.File) error {
			for _, step := range [][2]uintptr{{uiSetEvBit, evKey}, {uiSetEvBit, evRel}, {uiSetEvBit, evAbs},
				{uiSetKeyBit, btnLeft}, {uiSetKeyBit, btnLeft + 1}, {uiSetKeyBit, btnMiddle},
				{uiSetRelBit, relWheel}, {uiSetRelBit, relHWheel}, {uiSetAbsBit, absX}, {uiSetAbsBit, absY}} {
				if err := uinputIoctl(f, step[0], step[1]); err != nil {
					return err
				}
			}
			for _, code := range []uint16{absX, absY} {
				abs := uinputAbsSetup{Code: code, Max: absExtent}
				if err := uinputIoctl(f, uiAbsSetup, uintptr(unsafe.Pointer(&abs))); err != nil {
					return err
				}
			}
			return nil
		})
		if err != nil {
			keyboard.Close()
			desktopInputErr = err
			return
		}
		desktopInputDev = &desktopInput{keyboard: keyboard, tablet: tablet}
	})
	return desktopInputDev, desktopInputErr
}

func (d *desktopInput) apply(events []desktopEvent) error {
	keyboard, tablet, err := encodeDesktopInput(events)
	if err != nil {
		return err
	}
	// Keyboard and pointer are separate devices; order across them only
	// matters for modifier+click, where the keyboard batch goes first.
	if len(keyboard) > 0 {
		if _, err := d.keyboard.Write(keyboard); err != nil {
			return err
		}
	}
	if len(tablet) > 0 {
		if _, err := d.tablet.Write(tablet); err != nil {
			return err
		}
	}
	return nil
}

// listenDesktop serves one viewer at a time; the host worker bridge already
// admits a single local client.
func listenDesktop() error {
	fd, err := vsockListen(desktopPort, 1)
	if err != nil {
		return err
	}
	defer syscall.Close(fd)
	for {
		conn, err := vsockAccept(fd)
		if err != nil {
			if errors.Is(err, syscall.EINTR) || errors.Is(err, errNotHost) {
				continue
			}
			return err
		}
		serveDesktop(conn)
		conn.Close()
	}
}

// desktopWriter is shared by the frame loop and the audio relay; each record
// and flush is whole under mu.
type desktopWriter struct {
	mu   sync.Mutex
	conn *os.File
	out  *bufio.Writer
}

func (w *desktopWriter) flush() error {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.out.Flush()
}

func (w *desktopWriter) record(kind byte, head []byte, body []byte) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	var prefix [5]byte
	binary.BigEndian.PutUint32(prefix[:], uint32(1+len(head)+len(body)))
	prefix[4] = kind
	_ = w.conn.SetWriteDeadline(time.Now().Add(2 * time.Second))
	for _, part := range [][]byte{prefix[:], head, body} {
		if _, err := w.out.Write(part); err != nil {
			return err
		}
	}
	return nil
}

func (w *desktopWriter) event(value map[string]any) error {
	body, _ := json.Marshal(value)
	if err := w.record(1, nil, body); err != nil {
		return err
	}
	return w.flush()
}

func serveDesktop(conn *os.File) {
	w := &desktopWriter{conn: conn, out: bufio.NewWriterSize(conn, 1<<20)}
	lines := bufio.NewScanner(conn)
	lines.Buffer(make([]byte, 64<<10), desktopMaxLine)
	fail := func(err error) { _ = w.event(map[string]any{"event": "error", "message": err.Error()}) }
	var start struct {
		Method string `json:"method"`
	}
	_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	if !lines.Scan() || json.Unmarshal(lines.Bytes(), &start) != nil || start.Method != "start" {
		fail(errors.New("expected start"))
		return
	}
	_ = conn.SetReadDeadline(time.Time{})
	screen, err := openScanout()
	if err != nil {
		fail(err)
		return
	}
	defer screen.close()
	input, inputErr := sharedDesktopInput()
	if inputErr != nil {
		// Video still works; every input batch reports the reason.
		fmt.Fprintln(os.Stderr, "desktop input unavailable:", inputErr)
	}
	audio := &desktopAudio{}
	defer audio.stop()
	clipboard := &desktopClipboard{}
	defer clipboard.close()
	done := make(chan struct{})
	reports := make(chan error, 1)
	// Host input wakes a backed-off poll loop.
	inputs := make(chan struct{}, 1)
	go func() {
		defer close(done)
		for lines.Scan() {
			var message struct {
				Input          []desktopEvent `json:"input"`
				Audio          *bool          `json:"audio"`
				Mic            []byte         `json:"mic"`
				Clipboard      *string        `json:"clipboard"`
				ClipboardWatch *bool          `json:"clipboard_watch"`
				ClipboardGet   bool           `json:"clipboard_get"`
			}
			err := json.Unmarshal(lines.Bytes(), &message)
			if err == nil && (message.Clipboard != nil || message.ClipboardWatch != nil || message.ClipboardGet) {
				clipboard.apply(w, message.Clipboard, message.ClipboardWatch, message.ClipboardGet)
				continue
			}
			if err == nil && (message.Audio != nil || message.Mic != nil) {
				if message.Audio != nil {
					audio.listen(w, *message.Audio)
				}
				if message.Mic != nil {
					audio.microphone(w, message.Mic)
				}
				continue
			}
			if err == nil && input == nil {
				err = inputErr
			}
			if err == nil {
				err = input.apply(message.Input)
			}
			select {
			case inputs <- struct{}{}:
			default:
			}
			if err != nil {
				select {
				case reports <- err:
				default:
				}
			}
		}
	}()
	var pacer pollPacer
	timer := time.NewTimer(pacer.next(true))
	defer timer.Stop()
	due := time.Now().Add(desktopPoll)
	schedule := func(from time.Time, wait time.Duration) {
		due = from.Add(wait)
		timer.Reset(max(time.Until(due), 0))
	}
	waiting := ""
	var head [8]byte
	// Every 5 s: poll count, frames sent, the capture (diff + copy) cost and
	// the poll interval in effect.
	var polls, frames int
	var spent, worst time.Duration
	reported := time.Now()
	for {
		select {
		case <-done:
			return
		case err := <-reports:
			if w.event(map[string]any{"event": "input_error", "message": err.Error()}) != nil {
				return
			}
			continue
		case <-inputs:
			// Poll within one fast interval of the input, not a backed-off one.
			if wait := pacer.input(); time.Until(due) > wait {
				schedule(time.Now(), wait)
			}
			continue
		case <-timer.C:
		}
		tick := time.Now()
		changed, err := screen.refresh()
		if err != nil {
			// The desktop may still be starting; report once and keep polling.
			schedule(tick, pacer.next(false))
			if err.Error() != waiting {
				waiting = err.Error()
				if w.event(map[string]any{"event": "waiting", "message": waiting}) != nil {
					return
				}
			}
			continue
		}
		waiting = ""
		if changed {
			if w.event(map[string]any{"event": "mode", "width": screen.width, "height": screen.height, "format": "bgrx"}) != nil {
				return
			}
		}
		began := time.Now()
		sent, err := screen.capture(screen.mem, screen.pitch, screen.originX, screen.originY, func(x, y, width, height int, rows []byte) error {
			binary.BigEndian.PutUint16(head[0:], uint16(x))
			binary.BigEndian.PutUint16(head[2:], uint16(y))
			binary.BigEndian.PutUint16(head[4:], uint16(width))
			binary.BigEndian.PutUint16(head[6:], uint16(height))
			return w.record(2, head[:], rows)
		})
		took := time.Since(began)
		polls, spent, worst = polls+1, spent+took, max(worst, took)
		if err == nil && sent {
			frames++
			binary.BigEndian.PutUint64(head[:], uint64(time.Since(desktopEpoch).Microseconds()))
			if err = w.record(3, head[:], nil); err == nil {
				err = w.flush()
			}
		}
		if err != nil {
			return
		}
		schedule(tick, pacer.next(sent))
		if time.Since(reported) >= 5*time.Second {
			if w.event(map[string]any{"event": "stats", "polls": polls, "frames": frames,
				"capture_ms": float64(spent.Microseconds()) / float64(polls) / 1000, "capture_max_ms": float64(worst.Microseconds()) / 1000,
				"interval_ms": float64(pacer.interval.Microseconds()) / 1000}) != nil {
				return
			}
			polls, frames, spent, worst, reported = 0, 0, 0, 0, time.Now()
		}
	}
}

var desktopEpoch = time.Now()
