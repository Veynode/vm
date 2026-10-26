//go:build !linux

package main

import (
	"fmt"
	"os"
)

func sessionServerMain(args []string) int {
	fmt.Fprintln(os.Stderr, "session-server requires Linux")
	return 1
}
