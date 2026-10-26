//go:build !linux

package main

import "errors"

func containerInit() int { return 1 }

func loadContainerIdentity(root, envFile string) (executionIdentity, error) {
	return executionIdentity{}, errors.New("container mode requires Linux")
}

func (identity executionIdentity) withRequest(req request) (executionIdentity, error) {
	if req.User == "" && len(req.Env) == 0 && req.Workdir == "" {
		return identity, nil
	}
	return identity, errors.New("user, env and workdir apply only to container commands")
}
