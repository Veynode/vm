package main

import (
	"os"
	"testing"
)

func TestClockSyncValidates(t *testing.T) {
	if r := clockSync(request{ID: "1"}); r.Error == nil || r.Error.Code != "invalid_request" {
		t.Fatalf("zero time accepted: %+v", r)
	}
	if os.Geteuid() != 0 {
		if r := clockSync(request{ID: "1", UnixNS: 1}); r.Error == nil || r.Error.Code != "forbidden" {
			t.Fatalf("unprivileged step accepted: %+v", r)
		}
	}
}
