// Loaded into an app inside a macOS Guest (DYLD_INSERT_LIBRARIES), this routes every direct
// indexed draw through an indirect draw. The Guest's paravirtual Metal device reads baseVertex
// of direct indexed draws as an unsigned 16-bit value (-1 arrives as 65535, 65536 as 0), and
// SuperTuxKart's Vulkan renderer still drew corrupted frames when only those draws were
// rewritten. Indirect draws carry their arguments correctly, so all of them go that way.
#import <Metal/Metal.h>
#import <objc/runtime.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

typedef void (*DrawIndirect)(id, SEL, MTLPrimitiveType, MTLIndexType, id<MTLBuffer>, NSUInteger, id<MTLBuffer>, NSUInteger);

static DrawIndirect indirect;
static id<MTLBuffer> ring;
static _Atomic NSUInteger next;
static _Atomic unsigned long rewritten;
// 20-byte argument records. At 40k draws a second the ring wraps every ~26 s, far longer than
// any command buffer stays in flight.
static const NSUInteger slots = 1 << 20;

static void drawIndexed(id self, SEL cmd, MTLPrimitiveType type, NSUInteger count, MTLIndexType indexType, id<MTLBuffer> indexBuffer,
                        NSUInteger indexOffset, NSUInteger instances, NSInteger baseVertex, NSUInteger baseInstance) {
    NSUInteger slot = atomic_fetch_add(&next, 1) % slots;
    MTLDrawIndexedPrimitivesIndirectArguments *args = (MTLDrawIndexedPrimitivesIndirectArguments *)ring.contents + slot;
    args->indexCount = (uint32_t)count;
    args->instanceCount = (uint32_t)instances;
    args->indexStart = 0;
    args->baseVertex = (int32_t)baseVertex;
    args->baseInstance = (uint32_t)baseInstance;
    indirect(self, @selector(drawIndexedPrimitives:indexType:indexBuffer:indexBufferOffset:indirectBuffer:indirectBufferOffset:), type, indexType,
             indexBuffer, indexOffset, ring, slot * sizeof *args);
    if (atomic_fetch_add(&rewritten, 1) == 0) fprintf(stderr, "[metalfix] drawing direct indexed draws as indirect draws\n");
}

__attribute__((constructor)) static void install(void) {
    id<MTLDevice> device = MTLCreateSystemDefaultDevice();
    // METALFIX_FORCE applies it on other devices, to compare against a host.
    if (!device || (![device.name containsString:@"Paravirtual"] && !getenv("METALFIX_FORCE"))) return;
    ring = [device newBufferWithLength:slots * sizeof(MTLDrawIndexedPrimitivesIndirectArguments) options:MTLResourceStorageModeShared];
    MTLTextureDescriptor *td = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:4 height:4 mipmapped:NO];
    td.usage = MTLTextureUsageRenderTarget;
    MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
    pass.colorAttachments[0].texture = [device newTextureWithDescriptor:td];
    id<MTLRenderCommandEncoder> encoder = [[[device newCommandQueue] commandBuffer] renderCommandEncoderWithDescriptor:pass];
    Class encoderClass = object_getClass(encoder);
    [encoder endEncoding];
    Method direct = class_getInstanceMethod(encoderClass, @selector(drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:baseVertex:baseInstance:));
    Method viaBuffer = class_getInstanceMethod(encoderClass, @selector(drawIndexedPrimitives:indexType:indexBuffer:indexBufferOffset:indirectBuffer:indirectBufferOffset:));
    if (!direct || !viaBuffer) return;
    indirect = (DrawIndirect)method_getImplementation(viaBuffer);
    method_setImplementation(direct, (IMP)drawIndexed);
}
