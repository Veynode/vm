import Metal

// Issues random direct indexed draws; the vertex shader records the lowest and highest
// vertex_id and instance_id each draw saw. Prints the draws whose ranges differ from what their
// parameters imply and exits 1 if there are any. Draws whose vertex ids would go negative are
// not checked, since that is undefined.
let device = MTLCreateSystemDefaultDevice()!
let queue = device.makeCommandQueue()!
let library = try! device.makeLibrary(source: """
#include <metal_stdlib>
using namespace metal;
vertex float4 vs(uint vid [[vertex_id]], uint iid [[instance_id]], device atomic_uint* out [[buffer(0)]]) {
    atomic_fetch_min_explicit(out + 0, vid, memory_order_relaxed); atomic_fetch_max_explicit(out + 1, vid, memory_order_relaxed);
    atomic_fetch_min_explicit(out + 2, iid, memory_order_relaxed); atomic_fetch_max_explicit(out + 3, iid, memory_order_relaxed);
    return float4(2, 2, 0, 1); }
fragment float4 fs() { return 1; }
""", options: nil)
let pipelineDescriptor = MTLRenderPipelineDescriptor()
pipelineDescriptor.vertexFunction = library.makeFunction(name: "vs")
pipelineDescriptor.fragmentFunction = library.makeFunction(name: "fs")
pipelineDescriptor.colorAttachments[0].pixelFormat = .rgba8Unorm
let pipeline = try! device.makeRenderPipelineState(descriptor: pipelineDescriptor)
let targetDescriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm, width: 4, height: 4, mipmapped: false)
targetDescriptor.usage = .renderTarget
let pass = MTLRenderPassDescriptor()
pass.colorAttachments[0].texture = device.makeTexture(descriptor: targetDescriptor)!

// Index i holds i % 3000.
let indexCount = 400_000
let indices16 = device.makeBuffer(bytes: (0..<indexCount).map { UInt16($0 % 3000) }, length: indexCount * 2)!
let indices32 = device.makeBuffer(bytes: (0..<indexCount).map { UInt32($0 % 3000) }, length: indexCount * 4)!

struct Draw { var baseVertex, baseInstance, instances, start, count: Int; var short: Bool }
func pick(_ ranges: [ClosedRange<Int>]) -> Int { Int.random(in: ranges.randomElement()!) }
let drawCount = Int(CommandLine.arguments.dropFirst().first ?? "3000")!
let out = device.makeBuffer(length: drawCount * 16, options: .storageModeShared)!
let seen = out.contents().assumingMemoryBound(to: UInt32.self)
for k in 0..<drawCount { seen[k * 4] = .max; seen[k * 4 + 1] = 0; seen[k * 4 + 2] = .max; seen[k * 4 + 3] = 0 }
var draws: [Draw] = []
var commands = queue.makeCommandBuffer()!
var encoder = commands.makeRenderCommandEncoder(descriptor: pass)!
for k in 0..<drawCount {
    let draw = Draw(baseVertex: pick([0...0, 1...1000, 1000...65535, 65536...1_000_000, -32768...(-1)]),
                    baseInstance: pick([0...0, 1...100, 100...70000]), instances: pick([1...1, 2...100]),
                    start: pick([0...0, 1...300_000]), count: pick([1...1, 1...2000]) * 3, short: Bool.random())
    draws.append(draw)
    encoder.setRenderPipelineState(pipeline)
    encoder.setVertexBuffer(out, offset: k * 16, index: 0)
    encoder.drawIndexedPrimitives(type: .triangle, indexCount: draw.count, indexType: draw.short ? .uint16 : .uint32,
                                  indexBuffer: draw.short ? indices16 : indices32, indexBufferOffset: draw.start * (draw.short ? 2 : 4),
                                  instanceCount: draw.instances, baseVertex: draw.baseVertex, baseInstance: draw.baseInstance)
    if k % 500 == 499 {
        encoder.endEncoding(); commands.commit()
        commands = queue.makeCommandBuffer()!; encoder = commands.makeRenderCommandEncoder(descriptor: pass)!
    }
}
encoder.endEncoding(); commands.commit(); commands.waitUntilCompleted()

var wrong = 0
for (k, draw) in draws.enumerated() {
    let values = (draw.start..<(draw.start + draw.count)).map { $0 % 3000 }
    let low = values.min()! + draw.baseVertex, high = values.max()! + draw.baseVertex
    guard low >= 0 else { continue }
    let got = (Int(seen[k * 4]), Int(seen[k * 4 + 1]), Int(seen[k * 4 + 2]), Int(seen[k * 4 + 3]))
    let want = (low, high, draw.baseInstance, draw.baseInstance + draw.instances - 1)
    if got != want {
        wrong += 1
        if wrong <= 10 { print("wrong:", draw, "saw", got, "expected", want) }
    }
}
print("\(device.name): \(drawCount) draws, \(wrong) wrong")
exit(wrong == 0 ? 0 : 1)
