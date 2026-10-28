//go:build linux

package main

// Guest audio for Apple Virtualization desktops, carried on the desktop
// stream (vsock 5001). Apple Virtualization can only play a Guest sound card
// on the Mac's own speakers, so the image's PipeWire has two virtual devices
// instead (guest/images/desktop/veynodevm-audio.conf) and this relays them to
// the viewer, which plays the sound and owns the microphone.
//
// host -> guest: {"audio":true|false} starts or stops the speaker relay;
//   {"mic":"<base64>"} is up to 20 ms of 48 kHz mono s16le microphone PCM.
// guest -> host: record kind 4, u32 BE rate, u8 channels, s16le PCM
//   (48 kHz stereo, 20 ms per record), or the event
//   {"event":"audio_unavailable","message"} once when there is no relay.

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"sync"
	"syscall"
	"time"
)

const (
	audioRate     = 48000
	audioSpeakers = "veynodevm-speakers"
	audioMic      = "veynodevm-microphone-feed" // the microphone loopback's sink
	audioChunk    = audioRate / 50 * 2 * 2      // 20 ms of stereo s16le
	audioMaxMic   = audioRate / 50 * 2          // 20 ms of mono s16le
	audioTool     = "/usr/bin/pw-cat"
	// The viewer sends no "off": frames just stop. The loopback still holds
	// the last few milliseconds then, and would play them into the next
	// recording, so a pause this long is followed by silence that flushes them.
	audioMicIdle    = 100 * time.Millisecond
	audioMicSilence = audioMaxMic * 3
)

type desktopAudio struct {
	mu       sync.Mutex
	speakers *exec.Cmd
	mic      *exec.Cmd
	micIn    io.WriteCloser
	micIdle  *time.Timer
	reported bool
}

// audioCommand runs pw-cat in the desktop session, as its user, so it reaches
// the session's PipeWire.
func audioCommand(args ...string) (*exec.Cmd, error) {
	root := drivers.root()
	session, err := desktopEnv{root: root}.session()
	if err != nil {
		return nil, fmt.Errorf("no desktop session: %v", err)
	}
	path, err := executionIdentity{Chroot: root, env: session.env}.executable(audioTool)
	if err != nil {
		return nil, errors.New("PipeWire's pw-cat is not in this image")
	}
	cmd := exec.Command(path, args...)
	cmd.Env = session.env
	cmd.Dir = "/"
	credential := &syscall.Credential{Uid: session.uid, Gid: session.gid, Groups: processGroups("/proc/" + strconv.Itoa(session.pid) + "/status")}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Credential: credential, Pdeathsig: syscall.SIGKILL}
	if root != "/" {
		cmd.SysProcAttr.Chroot = root
	}
	return cmd, nil
}

// audioFormat is raw PCM on a pipe: without --raw, pw-cat reads and writes a
// sound file and refuses headerless input.
func audioFormat(channels int) []string {
	return []string{"--raw", "--rate", strconv.Itoa(audioRate), "--channels", strconv.Itoa(channels), "--format", "s16"}
}

func (a *desktopAudio) unavailable(w *desktopWriter, err error) {
	if !a.reported {
		a.reported = true
		_ = w.event(map[string]any{"event": "audio_unavailable", "message": err.Error()})
	}
}

// listen starts or stops sending the speakers' sound to the host.
func (a *desktopAudio) listen(w *desktopWriter, on bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if !on || a.speakers != nil {
		if !on {
			stopAudioCommand(&a.speakers)
		}
		return
	}
	// Recording a sink records its monitor.
	cmd, err := audioCommand(append([]string{"--record", "--target", audioSpeakers, "-P", "{ stream.capture.sink = true }"}, append(audioFormat(2), "-")...)...)
	var out io.ReadCloser
	if err == nil {
		out, err = cmd.StdoutPipe()
	}
	if err == nil {
		err = cmd.Start()
	}
	if err != nil {
		a.unavailable(w, err)
		return
	}
	a.speakers = cmd
	go func() {
		var head [5]byte
		binary.BigEndian.PutUint32(head[:], audioRate)
		head[4] = 2
		pcm := make([]byte, audioChunk)
		for {
			if _, err := io.ReadFull(out, pcm); err != nil {
				return
			}
			if w.record(4, head[:], pcm) != nil || w.flush() != nil {
				return
			}
		}
	}()
}

// microphone plays one host microphone frame into the virtual microphone.
func (a *desktopAudio) microphone(w *desktopWriter, pcm []byte) {
	if len(pcm) == 0 || len(pcm) > audioMaxMic || len(pcm)%2 != 0 {
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.mic == nil {
		cmd, err := audioCommand(append([]string{"--playback", "--target", audioMic}, append(audioFormat(1), "-")...)...)
		var in io.WriteCloser
		if err == nil {
			in, err = cmd.StdinPipe()
		}
		if err == nil {
			err = cmd.Start()
		}
		if err != nil {
			a.unavailable(w, err)
			return
		}
		a.mic, a.micIn = cmd, in
	}
	if _, err := a.micIn.Write(pcm); err != nil {
		_ = a.micIn.Close()
		stopAudioCommand(&a.mic)
		return
	}
	if a.micIdle == nil {
		a.micIdle = time.AfterFunc(audioMicIdle, a.flushMicrophone)
	} else {
		a.micIdle.Reset(audioMicIdle)
	}
}

// flushMicrophone plays silence after the last frame (see audioMicIdle).
func (a *desktopAudio) flushMicrophone() {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.mic != nil {
		_, _ = a.micIn.Write(make([]byte, audioMicSilence))
	}
}

func (a *desktopAudio) stop() {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.micIdle != nil {
		a.micIdle.Stop()
	}
	if a.micIn != nil {
		_ = a.micIn.Close()
	}
	stopAudioCommand(&a.mic)
	stopAudioCommand(&a.speakers)
}

func stopAudioCommand(cmd **exec.Cmd) {
	if *cmd == nil {
		return
	}
	_ = (*cmd).Process.Signal(os.Kill)
	_ = (*cmd).Wait()
	*cmd = nil
}

// desktopClipboard opens the display's clipboard on first use and reports
// its events on the stream.
type desktopClipboard struct {
	x *clipboardX11
}

func (d *desktopClipboard) apply(w *desktopWriter, text *string, watch *bool, get bool) {
	if d.x == nil || d.x.closed() {
		events := make(chan clipboardEvent, 4)
		x, err := openClipboard(desktopEnv{root: drivers.root()}, events)
		if err != nil {
			_ = w.event(map[string]any{"event": "clipboard", "error": err.Error()})
			return
		}
		d.x = x
		go func() {
			for {
				var event clipboardEvent
				select {
				case event = <-events:
				case <-x.done:
					return
				}
				value := map[string]any{"event": "clipboard", "text": event.Text}
				if event.Error != "" {
					value = map[string]any{"event": "clipboard", "error": event.Error}
				}
				if w.event(value) != nil {
					return
				}
			}
		}()
	}
	if text != nil {
		d.x.set(*text)
	}
	if watch != nil {
		d.x.setWatch(*watch)
	}
	if get {
		d.x.get()
	}
}

func (d *desktopClipboard) close() {
	if d.x != nil {
		d.x.close()
	}
}
