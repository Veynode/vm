// H.264 SPS rewrite for zero-latency decode. VideoToolbox writes no VUI
// bitstream_restriction, so a decoder must assume frames may be reordered and
// holds output until its DPB is full (5-8 frames at desktop sizes): on a
// mostly still desktop that is seconds of lag. Like WebRTC's SpsVuiRewriter,
// rewrite the SPS to declare max_num_reorder_frames = 0. Constrained Baseline
// has no B frames, so the declaration is true.

/** Remove emulation-prevention bytes (00 00 03 -> 00 00). */
export function unescapeRbsp(nal: Uint8Array): Uint8Array {
  const out: number[] = []
  let zeros = 0
  for (const byte of nal) {
    if (zeros >= 2 && byte === 3) {
      zeros = 0
      continue
    }
    out.push(byte)
    zeros = byte === 0 ? zeros + 1 : 0
  }
  return Uint8Array.from(out)
}

/** Insert emulation-prevention bytes. */
export function escapeRbsp(rbsp: Uint8Array): Uint8Array {
  const out: number[] = []
  let zeros = 0
  for (const byte of rbsp) {
    if (zeros >= 2 && byte <= 3) {
      out.push(3)
      zeros = 0
    }
    out.push(byte)
    zeros = byte === 0 ? zeros + 1 : 0
  }
  return Uint8Array.from(out)
}

class Bits {
  private bit = 0
  private readonly data: Uint8Array
  private readonly out: number[] = []
  private outBits = 0
  constructor(data: Uint8Array) {
    this.data = data
  }
  read(n: number): number {
    let value = 0
    for (let i = 0; i < n; i++) {
      if (this.bit >= this.data.length * 8) throw new Error('SPS truncated')
      value = value * 2 + ((this.data[this.bit >> 3] >> (7 - (this.bit & 7))) & 1)
      this.bit++
    }
    return value
  }
  readUe(): number {
    let zeros = 0
    while (this.read(1) === 0) if (++zeros > 31) throw new Error('SPS Exp-Golomb overflow')
    return 2 ** zeros - 1 + this.read(zeros)
  }
  write(value: number, n: number) {
    for (let i = n - 1; i >= 0; i--) {
      if ((this.outBits & 7) === 0) this.out.push(0)
      if (Math.floor(value / 2 ** i) % 2) this.out[this.out.length - 1] |= 1 << (7 - (this.outBits & 7))
      this.outBits++
    }
  }
  writeUe(value: number) {
    const coded = value + 1
    const length = Math.floor(Math.log2(coded))
    this.write(0, length)
    this.write(coded, length + 1)
  }
  /** Read and copy n bits / one ue / one se to the output. */
  copy(n: number): number {
    const value = this.read(n)
    this.write(value, n)
    return value
  }
  copyUe(): number {
    const value = this.readUe()
    this.writeUe(value)
    return value
  }
  finish(): Uint8Array {
    this.write(1, 1) // rbsp_stop_one_bit
    while (this.outBits & 7) this.write(0, 1)
    return Uint8Array.from(this.out)
  }
}

const HIGH_PROFILES = new Set([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135])

function copyHrd(bits: Bits) {
  const count = bits.copyUe() + 1
  if (count > 32) throw new Error('SPS HRD outside bound')
  bits.copy(8) // bit_rate_scale, cpb_size_scale
  for (let i = 0; i < count; i++) {
    bits.copyUe()
    bits.copyUe()
    bits.copy(1)
  }
  bits.copy(20) // four 5-bit delay/length fields
}

/**
 * Return the SPS NAL (header included, escaped) with a VUI that declares no
 * frame reordering, or null if it cannot be rewritten (left unchanged).
 */
export function rewriteSps(nal: Uint8Array): Uint8Array | null {
  if (nal.length < 4 || (nal[0] & 0x1f) !== 7) return null
  try {
    const bits = new Bits(unescapeRbsp(nal.subarray(1)))
    const profile = bits.copy(8)
    bits.copy(16) // constraint flags, level_idc
    bits.copyUe() // seq_parameter_set_id
    if (HIGH_PROFILES.has(profile)) {
      if (bits.copyUe() === 3) bits.copy(1) // chroma_format_idc, separate_colour_plane
      bits.copyUe()
      bits.copyUe()
      bits.copy(1)
      if (bits.copy(1)) return null // scaling matrices: not produced by VideoToolbox here
    }
    bits.copyUe() // log2_max_frame_num_minus4
    const pocType = bits.copyUe()
    if (pocType === 0) bits.copyUe()
    else if (pocType === 1) {
      bits.copy(1)
      bits.copyUe() // offset_for_non_ref_pic (se, same bit shape as ue)
      bits.copyUe()
      const cycle = bits.copyUe()
      if (cycle > 255) return null
      for (let i = 0; i < cycle; i++) bits.copyUe()
    }
    const refFrames = bits.copyUe()
    bits.copy(1) // gaps_in_frame_num_value_allowed
    bits.copyUe()
    bits.copyUe()
    if (!bits.copy(1)) bits.copy(1) // frame_mbs_only, mb_adaptive_frame_field
    bits.copy(1) // direct_8x8_inference
    if (bits.copy(1)) for (let i = 0; i < 4; i++) bits.copyUe() // frame cropping
    const vui = bits.read(1)
    bits.write(1, 1)
    if (vui) {
      if (bits.copy(1) && bits.copy(8) === 255) bits.copy(32) // aspect ratio, extended SAR
      if (bits.copy(1)) bits.copy(1) // overscan
      if (bits.copy(1) && (bits.copy(4) & 1)) bits.copy(24) // video signal, colour description
      if (bits.copy(1)) {
        bits.copyUe()
        bits.copyUe()
      }
      if (bits.copy(1)) {
        bits.copy(32) // num_units_in_tick
        bits.copy(32) // time_scale
        bits.copy(1) // fixed_frame_rate
      }
      const nal_hrd = bits.copy(1)
      if (nal_hrd) copyHrd(bits)
      const vcl_hrd = bits.copy(1)
      if (vcl_hrd) copyHrd(bits)
      if (nal_hrd || vcl_hrd) bits.copy(1) // low_delay_hrd
      bits.copy(1) // pic_struct_present
      if (bits.read(1)) {
        bits.read(1)
        for (let i = 0; i < 4; i++) bits.readUe()
        const reorder = bits.readUe()
        bits.readUe()
        if (reorder === 0) return null // already low latency
      }
    } else {
      bits.write(0, 8) // no aspect, overscan, signal, chroma loc, timing, two HRDs, pic_struct
    }
    bits.write(1, 1) // bitstream_restriction_flag
    bits.write(1, 1) // motion_vectors_over_pic_boundaries
    bits.writeUe(2) // max_bytes_per_pic_denom (spec default)
    bits.writeUe(1) // max_bits_per_mb_denom (spec default)
    bits.writeUe(16) // log2_max_mv_length_horizontal
    bits.writeUe(16) // log2_max_mv_length_vertical
    bits.writeUe(0) // max_num_reorder_frames
    bits.writeUe(Math.max(refFrames, 1)) // max_dec_frame_buffering
    const rbsp = bits.finish()
    return Uint8Array.from([nal[0], ...escapeRbsp(rbsp)])
  } catch {
    return null
  }
}

/** Rewrite every SPS in an Annex B access unit; returns the input if none changed. */
export function rewriteAccessUnit(au: Uint8Array, cache: Map<string, Uint8Array | null>): Uint8Array {
  const starts: number[] = []
  for (let i = 0; i + 3 <= au.length; i++) {
    if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1) {
      starts.push(i + 3)
      i += 2
    }
  }
  const parts: Uint8Array[] = []
  let changed = false
  starts.forEach((start, n) => {
    let end = n + 1 < starts.length ? starts[n + 1] - 3 : au.length
    if (n + 1 < starts.length && end > start && au[end - 1] === 0) end--
    const nal = au.subarray(start, end)
    let replacement: Uint8Array | null = null
    if ((nal[0] & 0x1f) === 7) {
      const key = nal.join(',')
      if (!cache.has(key)) cache.set(key, rewriteSps(nal))
      replacement = cache.get(key) ?? null
    }
    if (replacement) changed = true
    parts.push(Uint8Array.of(0, 0, 0, 1), replacement ?? nal)
  })
  if (!changed) return au
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let offset = 0
  for (const part of parts) {
    out.set(part, offset)
    offset += part.length
  }
  return out
}
