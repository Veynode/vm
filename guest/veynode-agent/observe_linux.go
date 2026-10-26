//go:build linux

package main

// drmScreen backs MCP observation with the DRM scanout and the shared uinput
// devices of the browser desktop (desktop_linux.go).
type drmScreen struct{}

func (drmScreen) grab() (screenImage, error) {
	s, err := openScanout()
	if err != nil {
		return screenImage{}, err
	}
	defer s.close()
	if _, err = s.refresh(); err != nil {
		return screenImage{}, err
	}
	stride := s.width * 4
	pix := make([]byte, stride*s.height)
	for y := range s.height {
		copy(pix[y*stride:], s.mem[(s.originY+y)*s.pitch+s.originX*4:][:stride])
	}
	return screenImage{pix: pix, width: s.width, height: s.height}, nil
}

func (drmScreen) size() (int, int, error) {
	s, err := openScanout()
	if err != nil {
		return 0, 0, err
	}
	defer s.close()
	if _, err = s.refresh(); err != nil {
		return 0, 0, err
	}
	return s.width, s.height, nil
}

func (drmScreen) apply(events []desktopEvent) error {
	input, err := sharedDesktopInput()
	if err != nil {
		return err
	}
	return input.apply(events)
}

func newDesktopObserver() *observer {
	// Create the input devices now so the display server has picked them up
	// before the first click arrives.
	go func() { _, _ = sharedDesktopInput() }()
	return newObserver(drmScreen{})
}
