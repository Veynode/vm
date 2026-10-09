import Foundation
import Metal

func graphicsSelfTest() {
    // Synthetic values intentionally include unsupported features and unequal
    // axes. No device enumeration, GPU work, capture or permission query occurs.
    let value=PublicMetalCapabilities(name:"Synthetic Device / Not GPU Evidence",architecture:"synthetic",
        registryID:UInt64.max,osVersion:"fixture-only",families:["apple5":true,"apple6":false,"mac2":true],
        unifiedMemory:true,threadWidth:1024,threadHeight:512,threadDepth:64,
        threadgroupMemory:32768,bufferLength:2147483648,samplerCount:96,
        argumentTier:MTLArgumentBuffersTier.tier2.rawValue,textureTier:MTLReadWriteTextureTier.tier2.rawValue,
        functionPointersCompute:false,functionPointersRender:false,dynamicLibrariesCompute:false,dynamicLibrariesRender:false,
        raytracingCompute:false,raytracingRender:false)
    let fields=guestGraphicsFields(value,queriedAt:Date(timeIntervalSince1970:123))
    precondition(fields["metal_device"] as? String==value.name)
    let graphics=fields["graphics"] as! [String:Any],device=graphics["device"] as! [String:Any]
    precondition(graphics["status"] as? String=="reported" && graphics["reason"]==nil)
    precondition(graphics["queried_at_unix_ms"] as? Int64==123000)
    precondition(device["registry_id"] as? String==String(UInt64.max),"registry ID must not lose JSON integer precision")
    let axes=device["max_threads_per_threadgroup_axes"] as! [String:Int]
    precondition(axes==["width":1024,"height":512,"depth":64] && device["max_threads_total"]==nil)
    precondition(device["per_kernel_limits"] is NSNull)
    let families=device["family_support"] as! [String:Bool]
    precondition(families["apple5"]==true && families["apple6"]==false && families["apple10"]==nil,"unqueried family is unknown, not false")
    for key in ["supports_function_pointers_compute","supports_function_pointers_render","supports_dynamic_libraries_compute","supports_dynamic_libraries_render","supports_raytracing_compute","supports_raytracing_render"] {
        precondition(device[key] as? Bool==false,"unsupported feature inflated")
    }
    precondition((device["argument_buffers_support"] as! [String:Any])["tier"] as? String=="tier2")
    let assessment=graphics["assessment"] as! [String:String]
    precondition(assessment["shader_rendering"]=="not-tested-by-this-query" && assessment["performance"]=="not-measured")
    let wire=try! JSONSerialization.data(withJSONObject:fields,options:[.sortedKeys,.withoutEscapingSlashes])
    let decoded=try! JSONSerialization.jsonObject(with:wire) as! [String:Any]
    precondition(NSDictionary(dictionary:decoded).isEqual(to:fields) && wire.count<32768)

    let absent=guestGraphicsFields(nil,queriedAt:Date(timeIntervalSince1970:123))
    let unavailable=absent["graphics"] as! [String:Any]
    precondition(absent["metal_device"] as? String=="none" && unavailable["status"] as? String=="unavailable")
    precondition(unavailable["reason"] as? String=="no_metal_device" && unavailable["device"] is NSNull)
    precondition(unavailable["family_support"]==nil,"no device must not synthesize feature support")
    _ = try! JSONSerialization.data(withJSONObject:absent)

    for expected in [fields,absent] {
        var queries=0
        let response=handle(["id":"graphics-info","method":"info"],allowExec:false,
            consoleRead:{ConsoleIdentityObservation(identity:nil,rejection:.identityUIDMismatch)},
            graphicsRead:{queries+=1;return expected})
        let info=response["result"] as! [String:Any],caps=info["capabilities"] as! [String:Bool]
        precondition(queries==1 && info["metal_device"] as? String==expected["metal_device"] as? String)
        precondition(NSDictionary(dictionary:info["graphics"] as! [String:Any]).isEqual(to:expected["graphics"] as! [String:Any]))
        precondition(caps["input"]==false && caps["screenshot"]==false && caps["exec"]==false && caps["async_exec"]==false,"graphics must not grant unrelated permissions")
    }
    print("PASS: graphics typed wire/JSON precision, per-axis limits, absent and unsupported features, no-device result, actual info handler injection and unchanged default permissions; synthetic values only, no Metal device queries")
}
