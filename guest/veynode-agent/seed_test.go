package main

import (
	"bytes"
	"testing"
)

func TestSeedFromConfigDiskTrailer(t *testing.T) {
	disk := make([]byte, 1<<20)
	copy(disk, "VEYNODEVM-CONFIG-V1\n{}\n")
	if seedFrom(disk) != nil {
		t.Fatal("a disk without the seed trailer has no seed")
	}
	want := bytes.Repeat([]byte{0xa5}, seedBytes)
	copy(disk[seedOffset:], seedMagic)
	copy(disk[seedOffset+len(seedMagic):], want)
	if got := seedFrom(disk); !bytes.Equal(got, want) {
		t.Fatalf("seed = %x, want %x", got, want)
	}
	if seedFrom(disk[:seedOffset+len(seedMagic)+seedBytes-1]) != nil {
		t.Fatal("a short read has no seed")
	}
}
