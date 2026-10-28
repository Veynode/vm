package main

import "bytes"

// The host writes a fresh random seed into the configuration disk's last
// 4 KiB before every boot (crates/control/src/services/container.rs).
const (
	seedMagic  = "VEYNODEVM-SEED-V1\n"
	seedOffset = 1<<20 - 4096
	seedBytes  = 64
)

// seedFrom returns the host's seed from a configuration disk image, if any.
func seedFrom(raw []byte) []byte {
	if len(raw) < seedOffset+len(seedMagic)+seedBytes || !bytes.HasPrefix(raw[seedOffset:], []byte(seedMagic)) {
		return nil
	}
	start := seedOffset + len(seedMagic)
	return raw[start : start+seedBytes]
}
