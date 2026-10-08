//go:build !linux

package main

import "fmt"

func configureCommandContainment(identity executionIdentity, legacy bool) (boundaryFactory, string, error) {
	if legacy {
		return legacyBoundary, "legacy-process-group-not-contained", nil
	}
	return nil, "unavailable", fmt.Errorf("command descendant containment requires Linux cgroup v2")
}
