import Foundation
import Metal

func output(_ value:[String:Any]) { let d = try! JSONSerialization.data(withJSONObject:value,options:[.sortedKeys,.prettyPrinted]); print(String(data:d,encoding:.utf8)!) }
func abort(_ message:String) -> Never { output(["success":false,"error":message]); exit(1) }
guard let device = MTLCreateSystemDefaultDevice() else { abort("No Metal device") }
let source = """
#include <metal_stdlib>
using namespace metal;
vertex float4 fullscreen_triangle(uint i [[vertex_id]]) {
    const float2 positions[3] = {float2(-1.0,-1.0),float2(3.0,-1.0),float2(-1.0,3.0)};
    return float4(positions[i],0.0,1.0);
}
fragment float4 solid_red() { return float4(1.0,0.0,0.0,1.0); }
kernel void double_values(device const uint *input [[buffer(0)]], device uint *output [[buffer(1)]], uint i [[thread_position_in_grid]]) { output[i] = input[i] * 2u + 1u; }
"""
do {
    let library = try device.makeLibrary(source:source,options:nil)
    guard let function = library.makeFunction(name:"double_values") else { abort("Missing Metal function") }
    let pipeline = try device.makeComputePipelineState(function:function)
    let count = 1_048_576
    guard let input = device.makeBuffer(length:count*MemoryLayout<UInt32>.size,options:.storageModeShared), let result = device.makeBuffer(length:count*MemoryLayout<UInt32>.size,options:.storageModeShared), let queue = device.makeCommandQueue() else { abort("Buffer or queue allocation failed") }
    let inputValues = input.contents().bindMemory(to:UInt32.self,capacity:count)
    for i in 0..<count { inputValues[i] = UInt32(i) }
    var wallSamples:[Double] = [], gpuSamples:[Double] = []
    for iteration in 0..<21 {
        guard let command = queue.makeCommandBuffer(), let encoder = command.makeComputeCommandEncoder() else { abort("Command allocation failed") }
        encoder.setComputePipelineState(pipeline);encoder.setBuffer(input,offset:0,index:0);encoder.setBuffer(result,offset:0,index:1)
        encoder.dispatchThreads(MTLSize(width:count,height:1,depth:1),threadsPerThreadgroup:MTLSize(width:min(256,pipeline.maxTotalThreadsPerThreadgroup),height:1,depth:1));encoder.endEncoding()
        let start = DispatchTime.now().uptimeNanoseconds
        command.commit();command.waitUntilCompleted()
        guard command.status == .completed else { abort(command.error?.localizedDescription ?? "GPU command failed") }
        if iteration > 0 { wallSamples.append(Double(DispatchTime.now().uptimeNanoseconds-start)/1e6);gpuSamples.append((command.gpuEndTime-command.gpuStartTime)*1000) }
    }
    let outputValues = result.contents().bindMemory(to:UInt32.self,capacity:count)
    for i in 0..<count { guard outputValues[i] == UInt32(i)*2+1 else { abort("GPU compute mismatch at \(i)") } }
    let textureDescriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat:.rgba8Unorm,width:256,height:256,mipmapped:false)
    textureDescriptor.usage = [.renderTarget]
    textureDescriptor.storageMode = .shared
    guard let texture = device.makeTexture(descriptor:textureDescriptor), let renderCommand = queue.makeCommandBuffer() else { abort("Render resource allocation failed") }
    let renderPipelineDescriptor = MTLRenderPipelineDescriptor()
    renderPipelineDescriptor.vertexFunction = library.makeFunction(name:"fullscreen_triangle")
    renderPipelineDescriptor.fragmentFunction = library.makeFunction(name:"solid_red")
    renderPipelineDescriptor.colorAttachments[0].pixelFormat = .rgba8Unorm
    let renderPipeline = try device.makeRenderPipelineState(descriptor:renderPipelineDescriptor)
    let pass = MTLRenderPassDescriptor()
    pass.colorAttachments[0].texture = texture
    pass.colorAttachments[0].loadAction = .clear
    pass.colorAttachments[0].storeAction = .store
    pass.colorAttachments[0].clearColor = MTLClearColor(red:0,green:0,blue:1,alpha:1)
    guard let renderer = renderCommand.makeRenderCommandEncoder(descriptor:pass) else { abort("Render encoder failed") }
    renderer.setRenderPipelineState(renderPipeline)
    renderer.drawPrimitives(type:.triangle,vertexStart:0,vertexCount:3)
    renderer.endEncoding();renderCommand.commit();renderCommand.waitUntilCompleted()
    guard renderCommand.status == .completed else { abort(renderCommand.error?.localizedDescription ?? "Render command failed") }
    var pixel = [UInt8](repeating:0,count:4)
    texture.getBytes(&pixel,bytesPerRow:4,from:MTLRegionMake2D(128,128,1,1),mipmapLevel:0)
    guard pixel == [255,0,0,255] else { abort("Triangle pixel mismatch: \(pixel)") }
    let families:[(String,MTLGPUFamily)] = [("apple1",.apple1),("apple2",.apple2),("apple3",.apple3),("apple4",.apple4),("apple5",.apple5),("apple6",.apple6),("apple7",.apple7),("apple8",.apple8),("apple9",.apple9),("mac2",.mac2),("common1",.common1),("common2",.common2),("common3",.common3),("metal3",.metal3)]
    output(["success":true,"scope":"Run inside Guest for Guest evidence; host output proves host only. Compute and offscreen triangle correctness probe, not graphics/game/CAD performance benchmark.","os":ProcessInfo.processInfo.operatingSystemVersionString,"device":device.name,"registry_id":String(device.registryID),"unified_memory":device.hasUnifiedMemory,"low_power":device.isLowPower,"removable":device.isRemovable,"recommended_max_working_set":device.recommendedMaxWorkingSetSize,"supported_families":families.filter{device.supportsFamily($0.1)}.map{$0.0},"verified_values":count,"triangle_render_verified":true,"triangle_center_rgba":pixel,"warmup_iterations":1,"sample_count":20,"wall_ms":wallSamples,"gpu_ms":gpuSamples])
} catch { abort(error.localizedDescription) }
