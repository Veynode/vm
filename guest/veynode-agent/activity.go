package main

import (
	"os"
	"strconv"
	"strings"
)

// activity reports how busy the Guest is, from /proc/loadavg: the host's idle
// monitor asks before it pauses or hibernates the VM, so work it cannot see
// (a build or server started in the background) keeps the VM awake.
func activity(req request) response {
	data, err := os.ReadFile("/proc/loadavg")
	if err != nil {
		return failure(req.ID, "unsupported", err.Error())
	}
	load1, running, err := parseLoadavg(string(data))
	if err != nil {
		return failure(req.ID, "activity_failed", err.Error())
	}
	return response{ID: req.ID, Result: map[string]any{"load1": load1, "running": running}}
}

// parseLoadavg reads "0.52 0.58 0.59 3/123 4567": the one-minute load and
// the runnable tasks, not counting the reader itself.
func parseLoadavg(text string) (float64, int, error) {
	fields := strings.Fields(text)
	if len(fields) < 4 {
		return 0, 0, strconv.ErrSyntax
	}
	load1, err := strconv.ParseFloat(fields[0], 64)
	if err != nil {
		return 0, 0, err
	}
	runnable, _, ok := strings.Cut(fields[3], "/")
	if !ok {
		return 0, 0, strconv.ErrSyntax
	}
	running, err := strconv.Atoi(runnable)
	if err != nil {
		return 0, 0, err
	}
	return load1, max(running-1, 0), nil
}
