package main

import "testing"

func TestParseLoadavg(t *testing.T) {
	load1, running, err := parseLoadavg("0.52 0.58 0.59 3/123 4567\n")
	if err != nil || load1 != 0.52 || running != 2 {
		t.Fatalf("got %v %v %v", load1, running, err)
	}
	if _, running, _ := parseLoadavg("0.00 0.00 0.00 1/80 9"); running != 0 {
		t.Fatalf("the reader is not running work: %v", running)
	}
	for _, bad := range []string{"", "0.1 0.2 0.3", "x 0 0 1/2 3", "0.1 0.2 0.3 12 3"} {
		if _, _, err := parseLoadavg(bad); err == nil {
			t.Fatalf("accepted %q", bad)
		}
	}
}
