// Game-like Metal benchmark for macOS Guests and the host (same binary, so the
// numbers compare directly). Each frame draws DRAWS lit, textured spheres
// (SEGMENTS controls triangles per sphere) into a 4x MSAA HDR target with
// depth, resolves it and runs a tonemap/vignette/blur post pass, like a game
// frame. Modes:
//   offscreen  render to a texture as fast as possible (GPU throughput)
//   window     borderless full-screen window with a CAMetalLayer; counts frames
//              that actually reach the display (presentedTime), so it includes
//              the Guest compositor and the host display path
// Prints one JSON line (and writes it to --output when given).
//   gpu-bench [--mode offscreen|window] [--width W --height H] [--seconds S]
//             [--draws N] [--segments N] [--lights N] [--vsync 0|1] [--output PATH]
import AppKit
import Metal
import QuartzCore
import simd

var options: [String: String] = ["mode": "offscreen", "width": "1920", "height": "1080", "seconds": "10",
                                 "draws": "2048", "segments": "24", "lights": "8", "vsync": "1"]
var arguments = CommandLine.arguments.dropFirst().makeIterator()
while let flag = arguments.next() {
    guard flag.hasPrefix("--"), let value = arguments.next() else { fatalError("usage: --name value ...") }
    options[String(flag.dropFirst(2))] = value
}
func int(_ key: String) -> Int { Int(options[key]!)! }
let mode = options["mode"]!
let seconds = Double(options["seconds"]!)!
let draws = int("draws"), segments = int("segments"), lightCount = min(int("lights"), 16)
var width = int("width"), height = int("height")

func fail(_ message: String) -> Never {
    let line = "{\"ok\":false,\"error\":\"\(message)\"}"
    print(line)
    if let path = options["output"] { try? line.write(toFile: path, atomically: true, encoding: .utf8) }
    exit(1)
}

let shaders = """
#include <metal_stdlib>
using namespace metal;
struct Vertex { packed_float3 p; packed_float3 n; packed_float2 uv; };
struct Frame { float4x4 viewProj; float4 eye; float4 lightPos[16]; float4 lightColor[16]; int lights; float time; };
struct Object { float4x4 model; float4 tint; };
struct Out { float4 position [[position]]; float3 world; float3 normal; float2 uv; float4 tint; };
vertex Out scene_vs(uint vid [[vertex_id]], const device Vertex *v [[buffer(0)]],
                    constant Frame &f [[buffer(1)]], constant Object &o [[buffer(2)]]) {
    Out out;
    float4 world = o.model * float4(float3(v[vid].p), 1);
    out.position = f.viewProj * world;
    out.world = world.xyz;
    out.normal = normalize((o.model * float4(float3(v[vid].n), 0)).xyz);
    out.uv = float2(v[vid].uv);
    out.tint = o.tint;
    return out;
}
fragment half4 scene_fs(Out in [[stage_in]], constant Frame &f [[buffer(1)]],
                        texture2d<half> albedo [[texture(0)]], sampler s [[sampler(0)]]) {
    half3 base = albedo.sample(s, in.uv * 4).rgb * half3(in.tint.rgb);
    float3 n = normalize(in.normal), view = normalize(f.eye.xyz - in.world);
    float3 color = float3(base) * 0.04;
    for (int i = 0; i < f.lights; i++) {
        float3 d = f.lightPos[i].xyz - in.world;
        float attenuation = 1.0 / (1.0 + dot(d, d) * 0.02);
        float3 l = normalize(d), h = normalize(l + view);
        color += (float3(base) * max(dot(n, l), 0.0) + pow(max(dot(n, h), 0.0), 48.0)) * f.lightColor[i].rgb * attenuation;
    }
    return half4(half3(color), 1);
}
struct Quad { float4 position [[position]]; float2 uv; };
vertex Quad post_vs(uint vid [[vertex_id]]) {
    float2 p = float2((vid << 1) & 2, vid & 2);
    Quad q; q.position = float4(p * 2 - 1, 0, 1); q.uv = float2(p.x, 1 - p.y); return q;
}
fragment half4 post_fs(Quad in [[stage_in]], texture2d<half> hdr [[texture(0)]], sampler s [[sampler(0)]]) {
    float2 texel = 1.0 / float2(hdr.get_width(), hdr.get_height());
    half3 c = hdr.sample(s, in.uv).rgb * 0.4h;
    for (int i = 0; i < 4; i++) {
        float2 o = float2((i & 1) ? 1.5 : -1.5, (i & 2) ? 1.5 : -1.5) * texel;
        c += hdr.sample(s, in.uv + o).rgb * 0.15h;
    }
    float3 x = float3(c);
    x = (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14);
    float vignette = 1.0 - 0.35 * length(in.uv - 0.5);
    return half4(half3(saturate(x * vignette)), 1);
}
"""

guard let device = MTLCreateSystemDefaultDevice() else { fail("no Metal device") }
guard let queue = device.makeCommandQueue() else { fail("no command queue") }
let library: MTLLibrary
do { library = try device.makeLibrary(source: shaders, options: nil) } catch { fail("shader compile: \(error)") }

// Sphere mesh: position, normal, uv (32 bytes per vertex), 32-bit indices.
var vertices: [Float] = []
var indices: [UInt32] = []
let rings = segments, sectors = segments * 2
for r in 0...rings {
    let phi = Float.pi * Float(r) / Float(rings)
    for s in 0...sectors {
        let theta = 2 * Float.pi * Float(s) / Float(sectors)
        let n = SIMD3<Float>(sin(phi) * cos(theta), cos(phi), sin(phi) * sin(theta))
        vertices += [n.x, n.y, n.z, n.x, n.y, n.z, Float(s) / Float(sectors), Float(r) / Float(rings)]
    }
}
for r in 0..<rings {
    for s in 0..<sectors {
        let a = UInt32(r * (sectors + 1) + s), b = a + UInt32(sectors + 1)
        indices += [a, b, a + 1, a + 1, b, b + 1]
    }
}
let trianglesPerFrame = indices.count / 3 * draws
let vertexBuffer = device.makeBuffer(bytes: vertices, length: vertices.count * 4, options: .storageModeShared)!
let indexBuffer = device.makeBuffer(bytes: indices, length: indices.count * 4, options: .storageModeShared)!

// Mipmapped procedural albedo texture.
let textureDescriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm, width: 1024, height: 1024, mipmapped: true)
textureDescriptor.usage = [.shaderRead]
let albedo = device.makeTexture(descriptor: textureDescriptor)!
var pixels = [UInt8](repeating: 0, count: 1024 * 1024 * 4)
for y in 0..<1024 {
    for x in 0..<1024 {
        let checker = ((x / 64) + (y / 64)) % 2 == 0
        let noise = UInt8((x * 7 + y * 13) % 32)
        let i = (y * 1024 + x) * 4
        pixels[i] = checker ? 200 : 90 + noise
        pixels[i + 1] = checker ? 180 : 110
        pixels[i + 2] = checker ? 150 : 140 + noise
        pixels[i + 3] = 255
    }
}
albedo.replace(region: MTLRegionMake2D(0, 0, 1024, 1024), mipmapLevel: 0, withBytes: pixels, bytesPerRow: 4096)
if let blit = queue.makeCommandBuffer(), let encoder = blit.makeBlitCommandEncoder() {
    encoder.generateMipmaps(for: albedo)
    encoder.endEncoding()
    blit.commit()
    blit.waitUntilCompleted()
}
let samplerDescriptor = MTLSamplerDescriptor()
samplerDescriptor.minFilter = .linear
samplerDescriptor.magFilter = .linear
samplerDescriptor.mipFilter = .linear
samplerDescriptor.sAddressMode = .repeat
samplerDescriptor.tAddressMode = .repeat
samplerDescriptor.maxAnisotropy = 8
let sampler = device.makeSamplerState(descriptor: samplerDescriptor)!

let scenePipeline: MTLRenderPipelineState
let postPipeline: MTLRenderPipelineState
do {
    let scene = MTLRenderPipelineDescriptor()
    scene.vertexFunction = library.makeFunction(name: "scene_vs")
    scene.fragmentFunction = library.makeFunction(name: "scene_fs")
    scene.colorAttachments[0].pixelFormat = .rgba16Float
    scene.depthAttachmentPixelFormat = .depth32Float
    scene.rasterSampleCount = 4
    scenePipeline = try device.makeRenderPipelineState(descriptor: scene)
    let post = MTLRenderPipelineDescriptor()
    post.vertexFunction = library.makeFunction(name: "post_vs")
    post.fragmentFunction = library.makeFunction(name: "post_fs")
    post.colorAttachments[0].pixelFormat = .bgra8Unorm
    postPipeline = try device.makeRenderPipelineState(descriptor: post)
} catch { fail("pipeline: \(error)") }
let depthDescriptor = MTLDepthStencilDescriptor()
depthDescriptor.depthCompareFunction = .less
depthDescriptor.isDepthWriteEnabled = true
let depthState = device.makeDepthStencilState(descriptor: depthDescriptor)!

var targets: (msaa: MTLTexture, depth: MTLTexture, hdr: MTLTexture)!
func makeTargets() {
    func texture(_ format: MTLPixelFormat, samples: Int, usage: MTLTextureUsage) -> MTLTexture {
        let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: width, height: height, mipmapped: false)
        d.textureType = samples > 1 ? .type2DMultisample : .type2D
        d.sampleCount = samples
        d.storageMode = .private
        d.usage = usage
        return device.makeTexture(descriptor: d)!
    }
    targets = (texture(.rgba16Float, samples: 4, usage: .renderTarget),
               texture(.depth32Float, samples: 4, usage: .renderTarget),
               texture(.rgba16Float, samples: 1, usage: [.renderTarget, .shaderRead]))
}
makeTargets()

// Frame constants: 64 (viewProj) + 16 (eye) + 2 * 16 * 16 (lights) + 16 = 608 bytes.
let frameSize = 608, objectStride = 256, inFlight = 3
let frameBuffers = (0..<inFlight).map { _ in device.makeBuffer(length: frameSize, options: .storageModeShared)! }
let objectBuffers = (0..<inFlight).map { _ in device.makeBuffer(length: draws * objectStride, options: .storageModeShared)! }
let side = Int(ceil(cbrt(Double(draws))))
let positions: [SIMD3<Float>] = (0..<draws).map { i in
    SIMD3(Float(i % side), Float((i / side) % side), Float(i / (side * side))) * 2.6 - Float(side) * 1.3
}

func perspective(_ fovy: Float, _ aspect: Float, _ near: Float, _ far: Float) -> simd_float4x4 {
    let y = 1 / tan(fovy / 2), x = y / aspect, z = far / (near - far)
    return simd_float4x4(SIMD4(x, 0, 0, 0), SIMD4(0, y, 0, 0), SIMD4(0, 0, z, -1), SIMD4(0, 0, z * near, 0))
}
func lookAt(_ eye: SIMD3<Float>, _ center: SIMD3<Float>) -> simd_float4x4 {
    let f = normalize(center - eye), s = normalize(cross(f, SIMD3(0, 1, 0))), u = cross(s, f)
    return simd_float4x4(SIMD4(s.x, u.x, -f.x, 0), SIMD4(s.y, u.y, -f.y, 0), SIMD4(s.z, u.z, -f.z, 0),
                         SIMD4(-dot(s, eye), -dot(u, eye), dot(f, eye), 1))
}

func update(_ slot: Int, time: Float) {
    let radius = Float(side) * 2.6
    let eye = SIMD3<Float>(sin(time * 0.3) * radius, Float(side) * 0.8, cos(time * 0.3) * radius)
    var viewProj = perspective(1.0, Float(width) / Float(height), 0.1, 500) * lookAt(eye, .zero)
    let frame = frameBuffers[slot].contents()
    memcpy(frame, &viewProj, 64)
    var eye4 = SIMD4<Float>(eye, 1)
    memcpy(frame + 64, &eye4, 16)
    for i in 0..<16 {
        let angle = time * 0.7 + Float(i) * 0.39
        var position = SIMD4<Float>(cos(angle) * Float(side) * 1.2, sin(Float(i)) * Float(side), sin(angle) * Float(side) * 1.2, 1)
        var color = SIMD4<Float>(0.6 + 0.4 * sin(Float(i)), 0.6 + 0.4 * cos(Float(i) * 1.3), 0.7, 1) * 3
        memcpy(frame + 80 + i * 16, &position, 16)
        memcpy(frame + 336 + i * 16, &color, 16)
    }
    var lights = Int32(lightCount), now = time
    memcpy(frame + 592, &lights, 4)
    memcpy(frame + 596, &now, 4)
    let objects = objectBuffers[slot].contents()
    for i in 0..<draws {
        let angle = time + Float(i) * 0.1
        let c = cos(angle), s = sin(angle), p = positions[i]
        var model = simd_float4x4(SIMD4(c, 0, -s, 0), SIMD4(0, 1, 0, 0), SIMD4(s, 0, c, 0), SIMD4(p.x, p.y, p.z, 1))
        var tint = SIMD4<Float>(0.5 + 0.5 * sin(Float(i)), 0.5 + 0.5 * cos(Float(i) * 0.7), 0.8, 1)
        memcpy(objects + i * objectStride, &model, 64)
        memcpy(objects + i * objectStride + 64, &tint, 16)
    }
}

func encode(_ commands: MTLCommandBuffer, slot: Int, output: MTLTexture) {
    let scene = MTLRenderPassDescriptor()
    scene.colorAttachments[0].texture = targets.msaa
    scene.colorAttachments[0].resolveTexture = targets.hdr
    scene.colorAttachments[0].loadAction = .clear
    scene.colorAttachments[0].clearColor = MTLClearColor(red: 0.02, green: 0.02, blue: 0.04, alpha: 1)
    scene.colorAttachments[0].storeAction = .multisampleResolve
    scene.depthAttachment.texture = targets.depth
    scene.depthAttachment.loadAction = .clear
    scene.depthAttachment.storeAction = .dontCare
    let encoder = commands.makeRenderCommandEncoder(descriptor: scene)!
    encoder.setRenderPipelineState(scenePipeline)
    encoder.setDepthStencilState(depthState)
    encoder.setCullMode(.back)
    encoder.setFrontFacing(.clockwise)
    encoder.setVertexBuffer(vertexBuffer, offset: 0, index: 0)
    encoder.setVertexBuffer(frameBuffers[slot], offset: 0, index: 1)
    encoder.setFragmentBuffer(frameBuffers[slot], offset: 0, index: 1)
    encoder.setVertexBuffer(objectBuffers[slot], offset: 0, index: 2)
    encoder.setFragmentTexture(albedo, index: 0)
    encoder.setFragmentSamplerState(sampler, index: 0)
    for i in 0..<draws {
        encoder.setVertexBufferOffset(i * objectStride, index: 2)
        encoder.drawIndexedPrimitives(type: .triangle, indexCount: indices.count, indexType: .uint32, indexBuffer: indexBuffer, indexBufferOffset: 0)
    }
    encoder.endEncoding()
    let post = MTLRenderPassDescriptor()
    post.colorAttachments[0].texture = output
    post.colorAttachments[0].loadAction = .dontCare
    post.colorAttachments[0].storeAction = .store
    let postEncoder = commands.makeRenderCommandEncoder(descriptor: post)!
    postEncoder.setRenderPipelineState(postPipeline)
    postEncoder.setFragmentTexture(targets.hdr, index: 0)
    postEncoder.setFragmentSamplerState(sampler, index: 0)
    postEncoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
    postEncoder.endEncoding()
}

// Statistics, filled from completion/presentation handlers.
let lock = NSLock()
var gpuTimes: [Double] = []
var presented: [Double] = []
var presentCallbacks = 0
var frameStarts: [Double] = []
var failures = 0
func percentile(_ values: [Double], _ p: Double) -> Double {
    guard !values.isEmpty else { return 0 }
    let sorted = values.sorted()
    return sorted[min(sorted.count - 1, Int(Double(sorted.count - 1) * p))]
}
func round2(_ x: Double) -> NSDecimalNumber { NSDecimalNumber(string: String(format: "%.2f", x)) }

func run(next: () -> (MTLTexture, MTLDrawable?)?) -> (frames: Int, elapsed: Double) {
    let semaphore = DispatchSemaphore(value: inFlight)
    let start = CACurrentMediaTime()
    var frames = 0
    while CACurrentMediaTime() - start < seconds {
        semaphore.wait()
        let ok: Bool = autoreleasepool {
            guard let (texture, drawable) = next() else { return false }
            frameStarts.append(CACurrentMediaTime())
            let slot = frames % inFlight
            update(slot, time: Float(CACurrentMediaTime() - start))
            let commands = queue.makeCommandBuffer()!
            encode(commands, slot: slot, output: texture)
            if let drawable {
                drawable.addPresentedHandler { d in
                    lock.lock()
                    presentCallbacks += 1
                    if d.presentedTime > 0 { presented.append(d.presentedTime) }
                    lock.unlock()
                }
                commands.present(drawable)
            }
            commands.addCompletedHandler { cb in
                lock.lock()
                if cb.status == .completed { gpuTimes.append((cb.gpuEndTime - cb.gpuStartTime) * 1000) } else { failures += 1 }
                lock.unlock()
                semaphore.signal()
            }
            commands.commit()
            return true
        }
        if !ok { semaphore.signal(); break }
        frames += 1
    }
    for _ in 0..<inFlight { semaphore.wait() }
    let elapsed = CACurrentMediaTime() - start
    // libdispatch traps when a semaphore is released below its initial value.
    for _ in 0..<inFlight { semaphore.signal() }
    return (frames, elapsed)
}

func report(frames: Int, elapsed: Double, extra: [String: Any]) -> Never {
    lock.lock()
    let gpu = gpuTimes, shown = presented, failed = failures, callbacks = presentCallbacks
    lock.unlock()
    let intervals = zip(shown.dropFirst(), shown).map { ($0 - $1) * 1000 }
    // Pacing as the app sees it (drawable acquisition), for displays that report no presentedTime.
    let pacing = zip(frameStarts.dropFirst(), frameStarts).map { ($0 - $1) * 1000 }
    var result: [String: Any] = [
        "ok": failed == 0 && frames > 0, "mode": mode, "device": device.name, "width": width, "height": height,
        "draws": draws, "triangles_per_frame": trianglesPerFrame, "lights": lightCount, "msaa": 4,
        "frames": frames, "seconds": round2(elapsed), "fps": round2(Double(frames) / elapsed),
        "gpu_ms_avg": round2(gpu.isEmpty ? 0 : gpu.reduce(0, +) / Double(gpu.count)),
        "gpu_ms_p99": round2(percentile(gpu, 0.99)), "failed_command_buffers": failed,
        "host": ProcessInfo.processInfo.operatingSystemVersionString,
    ]
    if mode == "window" {
        result["present_callbacks"] = callbacks
        result["pacing_ms_p50"] = round2(percentile(pacing, 0.5))
        result["pacing_ms_p99"] = round2(percentile(pacing, 0.99))
    }
    if !shown.isEmpty {
        result["presented_frames"] = shown.count
        result["presented_fps"] = round2(Double(shown.count - 1) / max(shown.last! - shown.first!, 0.001))
        result["frame_interval_ms_p50"] = round2(percentile(intervals, 0.5))
        result["frame_interval_ms_p99"] = round2(percentile(intervals, 0.99))
    }
    result.merge(extra) { $1 }
    let data = try! JSONSerialization.data(withJSONObject: result, options: [.sortedKeys])
    let line = String(data: data, encoding: .utf8)!
    print(line)
    if let path = options["output"] { try? line.write(toFile: path, atomically: true, encoding: .utf8) }
    exit(result["ok"] as! Bool ? 0 : 1)
}

if mode == "offscreen" {
    let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: width, height: height, mipmapped: false)
    d.storageMode = .private
    d.usage = [.renderTarget, .shaderRead]
    let outputs = (0..<inFlight).map { _ in device.makeTexture(descriptor: d)! }
    var index = 0
    let (frames, elapsed) = run { index += 1; return (outputs[index % inFlight], nil) }
    report(frames: frames, elapsed: elapsed, extra: [:])
}

guard mode == "window" else { fail("unknown --mode \(mode)") }
let app = NSApplication.shared
app.setActivationPolicy(.regular)
guard let screen = NSScreen.main else { fail("no screen (run inside the GUI session)") }
let window = NSWindow(contentRect: screen.frame, styleMask: [.borderless], backing: .buffered, defer: false)
window.level = .screenSaver
let layer = CAMetalLayer()
layer.device = device
layer.pixelFormat = .bgra8Unorm
layer.framebufferOnly = true
layer.displaySyncEnabled = options["vsync"] != "0"
layer.maximumDrawableCount = 3
let view = NSView(frame: screen.frame)
view.wantsLayer = true
view.layer = layer
window.contentView = view
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
let scale = screen.backingScaleFactor
width = Int(screen.frame.width * scale)
height = Int(screen.frame.height * scale)
layer.contentsScale = scale
layer.drawableSize = CGSize(width: width, height: height)
makeTargets()
Thread.detachNewThread {
    Thread.sleep(forTimeInterval: 1)
    let (frames, elapsed) = run {
        guard let drawable = layer.nextDrawable() else { return nil }
        return (drawable.texture, drawable)
    }
    report(frames: frames, elapsed: elapsed, extra: ["vsync": layer.displaySyncEnabled,
                                                     "screen_max_fps": screen.maximumFramesPerSecond])
}
app.run()
