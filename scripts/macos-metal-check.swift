// Metal compute check for macOS Guests (built on the host, run in the Guest
// over SSH by scripts/test-macos.py). The kernel is compiled at runtime by the
// OS Metal compiler, so the Guest needs no developer tools. Prints one JSON line.
import Foundation
import Metal

let source = """
#include <metal_stdlib>
using namespace metal;
kernel void square(device const float *input [[buffer(0)]], device float *output [[buffer(1)]], uint i [[thread_position_in_grid]]) {
    output[i] = input[i] * input[i] + 1.0;
}
"""
func fail(_ message: String) -> Never {
    print("{\"ok\":false,\"error\":\"\(message)\"}")
    exit(1)
}
guard let device = MTLCreateSystemDefaultDevice() else { fail("no Metal device") }
let count = 1 << 20
let input = (0..<count).map { Float($0 % 1024) / 8 }
guard let library = try? device.makeLibrary(source: source, options: nil),
      let function = library.makeFunction(name: "square"),
      let pipeline = try? device.makeComputePipelineState(function: function),
      let queue = device.makeCommandQueue(),
      let inputBuffer = device.makeBuffer(bytes: input, length: count * 4, options: .storageModeShared),
      let outputBuffer = device.makeBuffer(length: count * 4, options: .storageModeShared),
      let commands = queue.makeCommandBuffer(),
      let encoder = commands.makeComputeCommandEncoder() else { fail("Metal pipeline setup failed") }
encoder.setComputePipelineState(pipeline)
encoder.setBuffer(inputBuffer, offset: 0, index: 0)
encoder.setBuffer(outputBuffer, offset: 0, index: 1)
encoder.dispatchThreads(MTLSize(width: count, height: 1, depth: 1), threadsPerThreadgroup: MTLSize(width: pipeline.threadExecutionWidth, height: 1, depth: 1))
encoder.endEncoding()
let start = Date()
commands.commit()
commands.waitUntilCompleted()
let elapsed = Date().timeIntervalSince(start) * 1000
let output = outputBuffer.contents().bindMemory(to: Float.self, capacity: count)
let mismatches = (0..<count).filter { output[$0] != input[$0] * input[$0] + 1 }.count
print("{\"ok\":\(mismatches == 0 && commands.status == .completed),\"device\":\"\(device.name)\",\"elements\":\(count),\"mismatches\":\(mismatches),\"gpu_ms\":\(String(format: "%.2f", elapsed))}")
exit(mismatches == 0 ? 0 : 1)
