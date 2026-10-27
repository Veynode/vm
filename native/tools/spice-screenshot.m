// One bounded screenshot of the verified Darwin SPICE IOSurface console.
// No screen-recording permission, host desktop capture or native window needed.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <IOSurface/IOSurface.h>
#import <ImageIO/ImageIO.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreVideo/CoreVideo.h>
#include <spice-client.h>
#include <glib.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <signal.h>
#include <errno.h>

static GMainLoop *loop;
static IOSurfaceRef surface;
static int generationFD = -1;
static IOSurfaceID scanoutID;
static NSUInteger width, height;
static id<MTLDevice> device;
static id<MTLCommandQueue> queue;
static NSString *failure;
static NSDictionary *result;
static const NSUInteger maxPixels = 8388608, maxPNG = 8 * 1024 * 1024;

static void fail(NSString *message) {
    if (!failure) failure = message;
    if (loop) g_main_loop_quit(loop);
}
static BOOL generationLive(void) {
    if (generationFD < 0) return NO;
    struct pollfd p = {.fd = generationFD, .events = POLLIN};
    return poll(&p, 1, 0) >= 0 && !(p.revents & (POLLHUP | POLLERR | POLLNVAL));
}
static void clearSurface(void) {
    if (generationFD >= 0) close(generationFD);
    generationFD = -1;
    if (surface) CFRelease(surface);
    surface = NULL;
}
static void scanout(SpiceDisplayChannel *channel, GParamSpec *spec, gpointer data) {
    clearSurface();
    const SpiceGlScanout *info = spice_display_channel_get_gl_scanout(channel);
    if (!info) { fail(@"scanout_unavailable"); return; }
    // This adapter deliberately accepts only the measured public UTM ABI.
    // It must not interpret Linux DMA-BUF descriptors as IOSurface identifiers.
    int available = 0;
    struct pollfd p = {.fd = info->fd, .events = POLLIN};
    if (ioctl(info->fd, FIONREAD, &available) || available != 4 ||
        poll(&p, 1, 0) != 1 || !(p.revents & POLLIN) ||
        (p.revents & (POLLHUP | POLLERR | POLLNVAL))) {
        fail(@"fresh_scanout_identifier_unavailable"); return;
    }
    if (read(info->fd, &scanoutID, 4) != 4) { fail(@"incomplete_scanout_identifier"); return; }
    if (!info->width || !info->height || info->width > 4096 || info->height > 4096 ||
        (uint64_t)info->width * info->height > maxPixels || info->y0top) {
        fail(@"unsupported_scanout_geometry_or_orientation"); return;
    }
    surface = IOSurfaceLookup(scanoutID);
    if (!surface || IOSurfaceGetWidth(surface) != info->width ||
        IOSurfaceGetHeight(surface) != info->height ||
        IOSurfaceGetPixelFormat(surface) != kCVPixelFormatType_32BGRA) {
        clearSurface(); fail(@"unsupported_scanout_surface"); return;
    }
    generationFD = dup(info->fd);
    if (generationFD < 0) { clearSurface(); fail(@"scanout_descriptor_unavailable"); return; }
    fcntl(generationFD, F_SETFD, FD_CLOEXEC);
    width = info->width; height = info->height;
}
static void draw(SpiceDisplayChannel *channel, guint32 x, guint32 y,
                 guint32 w, guint32 h, gpointer data) { @autoreleasepool {
    if (!result && !failure) {
        if (!surface || !generationLive()) fail(@"scanout_invalidated");
        else {
            MTLTextureDescriptor *desc = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:width height:height mipmapped:NO];
            desc.storageMode = MTLStorageModeShared;
            id<MTLTexture> source = [device newTextureWithDescriptor:desc iosurface:surface plane:0];
            id<MTLTexture> owned = [device newTextureWithDescriptor:desc];
            id<MTLCommandBuffer> command = [queue commandBuffer];
            if (!source || !owned || !command) fail(@"metal_copy_unavailable");
            else {
                id<MTLBlitCommandEncoder> blit = [command blitCommandEncoder];
                if (!blit) fail(@"metal_blit_unavailable");
                else {
                    [blit copyFromTexture:source sourceSlice:0 sourceLevel:0 sourceOrigin:MTLOriginMake(0,0,0)
                        sourceSize:MTLSizeMake(width,height,1) toTexture:owned destinationSlice:0 destinationLevel:0 destinationOrigin:MTLOriginMake(0,0,0)];
                    [blit endEncoding]; [command commit]; [command waitUntilCompleted];
                    if (command.status != MTLCommandBufferStatusCompleted || !generationLive()) fail(@"copy_failed_or_source_invalidated");
                    else {
                        gint64 captured = g_get_monotonic_time();
                        NSDate *capturedDate = [NSDate date];
                        NSMutableData *pixels = [NSMutableData dataWithLength:width * height * 4];
                        [owned getBytes:pixels.mutableBytes bytesPerRow:width * 4 fromRegion:MTLRegionMake2D(0,0,width,height) mipmapLevel:0];
                        CGDataProviderRef provider = CGDataProviderCreateWithCFData((__bridge CFDataRef)pixels);
                        CGColorSpaceRef color = CGColorSpaceCreateDeviceRGB();
                        // Verified source memory is top-row-first BGRA; no display transform.
                        CGImageRef image = CGImageCreate(width,height,8,32,width*4,color,kCGBitmapByteOrder32Little | kCGImageAlphaNoneSkipFirst,provider,NULL,false,kCGRenderingIntentDefault);
                        NSMutableData *png = [NSMutableData new];
                        CGImageDestinationRef destination = CGImageDestinationCreateWithData((__bridge CFMutableDataRef)png,CFSTR("public.png"),1,NULL);
                        BOOL encoded = NO;
                        if (image && destination) { CGImageDestinationAddImage(destination,image,NULL); encoded = CGImageDestinationFinalize(destination); }
                        if (destination) CFRelease(destination);
                        if (image) CGImageRelease(image);
                        CGColorSpaceRelease(color); CGDataProviderRelease(provider);
                        if (!encoded) fail(@"png_encoding_failed");
                        else if (png.length > maxPNG) fail(@"screenshot_exceeds_8MiB_limit");
                        else if (!generationLive()) fail(@"source_invalidated_before_publication");
                        else {
                            NSISO8601DateFormatter *formatter = [NSISO8601DateFormatter new];
                            formatter.formatOptions = NSISO8601DateFormatWithInternetDateTime | NSISO8601DateFormatWithFractionalSeconds;
                            result = @{@"frame_id":NSUUID.UUID.UUIDString.lowercaseString,
                                @"captured_at":[formatter stringFromDate:capturedDate], @"captured_monotonic_us":@(captured),
                                @"width":@(width), @"height":@(height), @"display_id":@0,
                                @"coordinate_space":@"guest-physical-pixels", @"guest_ui_scale":[NSNull null],
                                @"mime_type":@"image/png", @"data_base64":[png base64EncodedStringWithOptions:0],
                                @"byte_length":@(png.length), @"gpu_copies":@1,
                                @"internal_scanout_id":@(scanoutID)};
                        }
                    }
                }
            }
        }
    }
    spice_display_channel_gl_draw_done(channel);
    if (result || failure) g_main_loop_quit(loop);
}}
static void channelEvent(SpiceChannel *channel, SpiceChannelEvent event, gpointer data) {
    if (event == SPICE_CHANNEL_CLOSED || event >= SPICE_CHANNEL_ERROR_CONNECT) fail(@"spice_disconnected");
}
static void newChannel(SpiceSession *session, SpiceChannel *channel, gpointer data) {
    if (SPICE_IS_DISPLAY_CHANNEL(channel)) {
        gint channelID = -1; g_object_get(channel,"channel-id",&channelID,NULL);
        if (channelID != 0) return;
        g_signal_connect(channel,"notify::gl-scanout",G_CALLBACK(scanout),NULL);
        g_signal_connect(channel,"gl-draw",G_CALLBACK(draw),NULL);
        g_signal_connect(channel,"channel-event",G_CALLBACK(channelEvent),NULL);
        spice_channel_connect(channel);
    } else if (SPICE_IS_MAIN_CHANNEL(channel)) g_signal_connect(channel,"channel-event",G_CALLBACK(channelEvent),NULL);
}
static gboolean timeout(gpointer data) { fail(@"screenshot_timeout"); return G_SOURCE_REMOVE; }
static BOOL writeBounded(NSData *data) {
    NSUInteger offset = 0;
    gint64 end = g_get_monotonic_time() + 500000;
    while (offset < data.length) {
        ssize_t n = write(STDOUT_FILENO,(const uint8_t *)data.bytes + offset,data.length-offset);
        if (n > 0) { offset += (NSUInteger)n; continue; }
        if (n < 0 && errno == EINTR) continue;
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            gint64 left = end-g_get_monotonic_time(); if (left <= 0) return NO;
            struct pollfd p = {.fd=STDOUT_FILENO,.events=POLLOUT};
            if (poll(&p,1,(int)((left+999)/1000)) > 0 && !(p.revents&(POLLHUP|POLLERR|POLLNVAL))) continue;
        }
        return NO;
    }
    return YES;
}
int main(int argc, char **argv) { @autoreleasepool {
    if (argc != 2) { fprintf(stderr,"usage: spice-screenshot SPICE_UNIX_SOCKET\n"); return 2; }
    signal(SIGPIPE,SIG_IGN);
    if (fcntl(STDOUT_FILENO,F_SETFL,fcntl(STDOUT_FILENO,F_GETFL)|O_NONBLOCK) < 0) return 1;
    device = MTLCreateSystemDefaultDevice(); queue = [device newCommandQueue];
    loop = g_main_loop_new(NULL,FALSE);
    SpiceSession *session = spice_session_new();
    g_object_set(session,"unix-path",argv[1],NULL);
    g_signal_connect(session,"channel-new",G_CALLBACK(newChannel),NULL);
    if (!device || !queue) fail(@"metal_unavailable");
    else if (!spice_session_connect(session)) fail(@"spice_connect_failed");
    else { g_timeout_add_seconds(10,timeout,NULL); g_main_loop_run(loop); }
    // Intentional shutdown must not turn a successfully captured result into failure.
    GList *channels=spice_session_get_channels(session);
    for (GList *item=channels;item;item=item->next) g_signal_handlers_disconnect_by_func(item->data,G_CALLBACK(channelEvent),NULL);
    g_list_free(channels); spice_session_disconnect(session); g_object_unref(session);
    clearSurface(); g_main_loop_unref(loop); loop=NULL;
    NSDictionary *output = failure ? @{@"error":failure} : result;
    if (!output) output = @{@"error":@"no_screenshot"};
    NSMutableData *json = [[NSJSONSerialization dataWithJSONObject:output options:NSJSONWritingSortedKeys|NSJSONWritingWithoutEscapingSlashes error:nil] mutableCopy];
    [json appendBytes:"\n" length:1];
    if (!writeBounded(json)) return 1;
    return failure || !result ? 1 : 0;
}}
