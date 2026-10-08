// Diagnostic SPICE GL-scanout consumer. Protocol reference: CocoaSpice CSDisplay.m
// commit 127033fa3e59cd49678f49ed54f8adfc060afb56 (Apache-2.0).
// Copies the shared surface before acknowledging the producer; never opens a view.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <CoreImage/CoreImage.h>
#import <ImageIO/ImageIO.h>
#import <IOSurface/IOSurface.h>
#include <glib.h>
#include <spice-client.h>
#include <poll.h>
#include <unistd.h>
static GMainLoop *loop;
static id<MTLDevice> device;
static id<MTLCommandQueue> queue;
static id<MTLTexture> scanout;
static NSString *output;
static int frames=0, saved=0;
static BOOL y0top;
static void on_scanout(SpiceDisplayChannel *channel,GParamSpec *spec,gpointer data) {
    scanout=nil;
    const SpiceGlScanout *info=spice_display_channel_get_gl_scanout(channel);
    if(!info)return;
    y0top=info->y0top;
    struct pollfd p={.fd=info->fd,.events=POLLIN};
    if(poll(&p,1,0)<0||(p.revents&(POLLHUP|POLLIN))!=POLLIN){fprintf(stderr,"scanout fd unavailable revents=%x\n",p.revents);return;}
    IOSurfaceID ident=0;
    if(read(info->fd,&ident,sizeof(ident))!=sizeof(ident))return;
    IOSurfaceRef surface=IOSurfaceLookup(ident);
    if(!surface){fprintf(stderr,"surface lookup failed\n");return;}
    printf("SURFACE width=%zu height=%zu bytes_per_row=%zu allocation=%zu format=%08x\n",IOSurfaceGetWidth(surface),IOSurfaceGetHeight(surface),IOSurfaceGetBytesPerRow(surface),IOSurfaceGetAllocSize(surface),(unsigned)IOSurfaceGetPixelFormat(surface));
    MTLPixelFormat format=IOSurfaceGetPixelFormat(surface)=='RGBA'?MTLPixelFormatRGBA8Unorm:MTLPixelFormatBGRA8Unorm;
    MTLTextureDescriptor *desc=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:format width:info->width height:info->height mipmapped:NO];
    desc.usage=MTLTextureUsageShaderRead;
    scanout=[device newTextureWithDescriptor:desc iosurface:surface plane:0];
    printf("SCANOUT %ux%u format=%x top=%d\n",info->width,info->height,info->format,info->y0top);fflush(stdout);
    CFRelease(surface);
}
static void on_draw(SpiceDisplayChannel *channel,guint32 x,guint32 y,guint32 w,guint32 h,gpointer data) {
    frames++;
    if(scanout && (frames==10||frames==80)){
        @autoreleasepool {
            MTLTextureDescriptor *desc=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:scanout.pixelFormat width:scanout.width height:scanout.height mipmapped:NO];
            desc.usage=MTLTextureUsageShaderRead;desc.storageMode=MTLStorageModeShared;
            id<MTLTexture> owned=[device newTextureWithDescriptor:desc];
            id<MTLCommandBuffer> command=[queue commandBuffer];id<MTLBlitCommandEncoder> blit=[command blitCommandEncoder];
            [blit copyFromTexture:scanout sourceSlice:0 sourceLevel:0 sourceOrigin:MTLOriginMake(0,0,0) sourceSize:MTLSizeMake(scanout.width,scanout.height,1) toTexture:owned destinationSlice:0 destinationLevel:0 destinationOrigin:MTLOriginMake(0,0,0)];
            [blit endEncoding];[command commit];[command waitUntilCompleted];
            if(command.status==MTLCommandBufferStatusCompleted){
                NSUInteger bytes=owned.width*owned.height*4;
                uint8_t *pixels=malloc(bytes); if(!pixels){spice_display_channel_gl_draw_done(channel);g_main_loop_quit(loop);return;}
                [owned getBytes:pixels bytesPerRow:owned.width*4 fromRegion:MTLRegionMake2D(0,0,owned.width,owned.height) mipmapLevel:0];
                NSUInteger nonblack=0; uint64_t hash=14695981039346656037ULL;
                for(NSUInteger i=0;i<bytes;i+=4){if(pixels[i]||pixels[i+1]||pixels[i+2])nonblack++;for(int j=0;j<3;j++){hash^=pixels[i+j];hash*=1099511628211ULL;}}
                free(pixels);
                printf("FRAME frame=%d width=%lu height=%lu nonblack=%lu rgb_hash=%016llx time_us=%lld\n",frames,(unsigned long)owned.width,(unsigned long)owned.height,(unsigned long)nonblack,(unsigned long long)hash,(long long)g_get_monotonic_time());
                CIImage *ci=[[CIImage alloc] initWithMTLTexture:owned options:nil];
                ci=[ci imageBySettingAlphaOneInExtent:ci.extent];
                if(!y0top)ci=[ci imageByApplyingOrientation:kCGImagePropertyOrientationDownMirrored];
                CIContext *context=[CIContext contextWithMTLDevice:device];CGImageRef image=[context createCGImage:ci fromRect:ci.extent];
                NSURL *url=[NSURL fileURLWithPath:[output stringByAppendingFormat:@"-%d.png",frames]];
                CGImageDestinationRef dest=CGImageDestinationCreateWithURL((__bridge CFURLRef)url,CFSTR("public.png"),1,NULL);
                if(image&&dest){CGImageDestinationAddImage(dest,image,NULL);if(CGImageDestinationFinalize(dest)){saved++;printf("CAPTURE %s frame=%d\n",url.path.UTF8String,frames);fflush(stdout);}}
                if(dest)CFRelease(dest);if(image)CGImageRelease(image);
            }
        }
    }
    spice_display_channel_gl_draw_done(channel);
    if(saved==2)g_main_loop_quit(loop);
}
static void on_channel(SpiceSession *session,SpiceChannel *channel,gpointer data){
    if(SPICE_IS_DISPLAY_CHANNEL(channel)){
        g_signal_connect(channel,"notify::gl-scanout",G_CALLBACK(on_scanout),NULL);
        g_signal_connect(channel,"gl-draw",G_CALLBACK(on_draw),NULL);
        spice_channel_connect(channel);
    }
}
static gboolean on_timeout(gpointer data){fprintf(stderr,"capture timeout frames=%d saved=%d\n",frames,saved);g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
int main(int argc,char **argv){@autoreleasepool{
    if(argc!=3){fprintf(stderr,"usage: spice-capture UNIX_SOCKET OUTPUT_PREFIX\n");return 2;}
    device=MTLCreateSystemDefaultDevice();queue=[device newCommandQueue];output=[NSString stringWithUTF8String:argv[2]];
    if(!device||!queue)return 3;
    loop=g_main_loop_new(NULL,FALSE);SpiceSession *session=spice_session_new();
    g_object_set(session,"unix-path",argv[1],NULL);g_signal_connect(session,"channel-new",G_CALLBACK(on_channel),NULL);
    if(!spice_session_connect(session))return 4;g_timeout_add_seconds(45,on_timeout,NULL);g_main_loop_run(loop);
    spice_session_disconnect(session);g_object_unref(session);g_main_loop_unref(loop);return saved==2?0:1;
}}
