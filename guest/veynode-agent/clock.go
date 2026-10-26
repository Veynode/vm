package main

import (
	"os"
	"syscall"
	"time"
)

// clockSync steps the Guest wall clock to the host's time. The host sends it
// after resuming a saved machine state (warm checkpoint rollback), whose clock
// otherwise continues from the moment the state was saved.
func clockSync(req request) response {
	if req.UnixNS <= 0 {
		return failure(req.ID, "invalid_request", "unix_ns must be positive")
	}
	if os.Geteuid() != 0 {
		return failure(req.ID, "forbidden", "setting the clock needs the root Guest service")
	}
	before := time.Now()
	tv := syscall.NsecToTimeval(req.UnixNS)
	if err := syscall.Settimeofday(&tv); err != nil {
		return failure(req.ID, "clock_sync_failed", err.Error())
	}
	return response{ID: req.ID, Result: map[string]any{"stepped_ms": (req.UnixNS - before.UnixNano()) / int64(time.Millisecond)}}
}
