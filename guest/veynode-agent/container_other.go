//go:build !linux

package main

import "errors"

func containerInit() int { return 1 }

func loadContainerIdentity(root, envFile string) (executionIdentity, error) {
	return executionIdentity{}, errors.New("container mode requires Linux")
}
