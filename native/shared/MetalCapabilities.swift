import Foundation
import Metal

// A typed value separates public device reads from wire formatting. It does not
// encode successful shader compilation, rendering, performance or authorization.
struct PublicMetalCapabilities {
    let name:String,architecture:String,registryID:UInt64,osVersion:String
    let families:[String:Bool],unifiedMemory:Bool
    let threadWidth:Int,threadHeight:Int,threadDepth:Int
    let threadgroupMemory:Int,bufferLength:Int,samplerCount:Int
    let argumentTier:UInt,textureTier:UInt
    let functionPointersCompute:Bool,functionPointersRender:Bool
    let dynamicLibrariesCompute:Bool,dynamicLibrariesRender:Bool
    let raytracingCompute:Bool,raytracingRender:Bool

    var wire:[String:Any] {
        ["schema":1,"name":name,"architecture_name":architecture,"registry_id":String(registryID),
         "os_version":osVersion,"family_support":families,"unified_memory":unifiedMemory,
         "max_threads_per_threadgroup_axes":["width":threadWidth,"height":threadHeight,"depth":threadDepth],
         "max_threadgroup_memory_bytes":threadgroupMemory,"max_buffer_bytes":bufferLength,
         "max_argument_buffer_sampler_count":samplerCount,
         "argument_buffers_support":["raw_value":argumentTier,"tier":argumentTier==MTLArgumentBuffersTier.tier1.rawValue ? "tier1":argumentTier==MTLArgumentBuffersTier.tier2.rawValue ? "tier2":"unknown"],
         "read_write_texture_support":["raw_value":textureTier,"tier":textureTier==MTLReadWriteTextureTier.tierNone.rawValue ? "none":textureTier==MTLReadWriteTextureTier.tier1.rawValue ? "tier1":textureTier==MTLReadWriteTextureTier.tier2.rawValue ? "tier2":"unknown"],
         "supports_function_pointers_compute":functionPointersCompute,"supports_function_pointers_render":functionPointersRender,
         "supports_dynamic_libraries_compute":dynamicLibrariesCompute,"supports_dynamic_libraries_render":dynamicLibrariesRender,
         "supports_raytracing_compute":raytracingCompute,"supports_raytracing_render":raytracingRender,
         "per_kernel_limits":NSNull(),
         "limit_semantics":"Thread axes are independent device maxima, not multiplied into a total. Per-kernel thread total, execution width and static memory require a successfully created pipeline.",
         "interpretation":"Reported public capabilities are context, not proof that a particular kernel compiles, nor a diagnosis of nil pipeline creation."]
    }
}

func readPublicMetalCapabilities(_ device:MTLDevice)->PublicMetalCapabilities {
    var families:[(String,MTLGPUFamily)] = [("apple1",.apple1),("apple2",.apple2),("apple3",.apple3),
        ("apple4",.apple4),("apple5",.apple5),("apple6",.apple6),("apple7",.apple7),
        ("apple8",.apple8),("apple9",.apple9),("mac2",.mac2),("common1",.common1),
        ("common2",.common2),("common3",.common3),("metal3",.metal3)]
    if #available(macOS 26.0,*) {families += [("apple10",.apple10),("metal4",.metal4)]}
    var support:[String:Bool]=[:]
    for (name,family) in families {support[name]=device.supportsFamily(family)}
    let axes=device.maxThreadsPerThreadgroup
    return .init(name:device.name,architecture:device.architecture.name,registryID:device.registryID,
        osVersion:ProcessInfo.processInfo.operatingSystemVersionString,families:support,unifiedMemory:device.hasUnifiedMemory,
        threadWidth:axes.width,threadHeight:axes.height,threadDepth:axes.depth,
        threadgroupMemory:device.maxThreadgroupMemoryLength,bufferLength:device.maxBufferLength,
        samplerCount:device.maxArgumentBufferSamplerCount,argumentTier:device.argumentBuffersSupport.rawValue,
        textureTier:device.readWriteTextureSupport.rawValue,functionPointersCompute:device.supportsFunctionPointers,
        functionPointersRender:device.supportsFunctionPointersFromRender,
        dynamicLibrariesCompute:device.supportsDynamicLibraries,dynamicLibrariesRender:device.supportsRenderDynamicLibraries,
        raytracingCompute:device.supportsRaytracing,raytracingRender:device.supportsRaytracingFromRender)
}

func guestGraphicsFields(_ device:PublicMetalCapabilities?,queriedAt:Date=Date())->[String:Any] {
    var graphics:[String:Any] = ["schema":1,"api":"metal","status":device == nil ? "unavailable":"reported",
        "device_selection":"system-default","evidence":"public-device-properties",
        "queried_at_unix_ms":Int64(queriedAt.timeIntervalSince1970*1000),
        "device":device?.wire as Any? ?? NSNull(),
        "assessment":["shader_rendering":"not-tested-by-this-query","performance":"not-measured",
                      "per_kernel_limits":"unavailable-without-pipeline"],
        "scope":"Read-only properties of the Guest service's system-default Metal device. This query does not compile, render, benchmark, authorize input or change permissions."]
    if device==nil {graphics["reason"]="no_metal_device"}
    return ["metal_device":device?.name ?? "none","graphics":graphics]
}

func readGuestGraphicsFields()->[String:Any] {
    // Select once so the legacy name and structured record cannot refer to two
    // different default devices. No library, pipeline or command queue is created.
    let device=MTLCreateSystemDefaultDevice()
    return guestGraphicsFields(device.map{readPublicMetalCapabilities($0)})
}
