// Diagnostic only: actual SPICE GL scanout IOSurface -> required hardware H.264 or HEVC.
// Public SPICE/IOSurface/CoreVideo/VideoToolbox/AVFoundation APIs; no private capture.
// Producer ACK follows the owned GPU copy and synchronous hardware encode completion.
#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#import <IOSurface/IOSurface.h>
#import <Metal/Metal.h>
#include <sys/stat.h>
#include <glib.h>
#include <spice-client.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>

static GMainLoop *loop;
static IOSurfaceRef surface;
static VTCompressionSessionRef encoder;
static CVPixelBufferRef normalizedPixel;
static id<MTLDevice> metal;
static id<MTLCommandQueue> commandQueue;
static id<MTLComputePipelineState> normalizePipeline;
static NSUInteger normalizationCopies;
static BOOL referenceSaved;
static AVAssetWriter *writer;
static AVAssetWriterInput *writerInput;
static NSLock *lock;
static NSString *prefix,*failure,*codecName;
static BOOL useHEVC;
static FILE *annexB;
static NSUInteger width,height,received,submitted,encoded,skipped,keyframes,bytesWritten;
static BOOL top,hardwareVerified,sharedSurfaceVerified=YES;
static gint64 firstTime,lastSample;
static NSMutableArray<NSNumber *> *sampleEvents,*encodeWallMS;
static const NSUInteger targetFrames=60;
static void fail(NSString *message){[lock lock];if(!failure)failure=message;[lock unlock];}
static BOOL writeBytes(const void *bytes,size_t count){if(fwrite(bytes,1,count,annexB)!=count){fail(@"Annex B output write failed");return NO;}bytesWritten+=count;return YES;}
static BOOL writeNAL(const void *data,size_t count){static const uint8_t start[]={0,0,0,1};return writeBytes(start,4)&&writeBytes(data,count);}
static void compressed(void *context,void *source,OSStatus status,VTEncodeInfoFlags flags,CMSampleBufferRef sample){
    @autoreleasepool {
        if(status!=noErr || !sample || !CMSampleBufferDataIsReady(sample) || (flags&kVTEncodeInfo_FrameDropped)){fail([NSString stringWithFormat:@"Encode callback failed/status=%d flags=%u",(int)status,(unsigned)flags]);return;}
        CMFormatDescriptionRef format=CMSampleBufferGetFormatDescription(sample);
        if(!writer){
            NSError *error=nil;
            writer=[[AVAssetWriter alloc] initWithURL:[NSURL fileURLWithPath:[prefix stringByAppendingString:@".mp4"]] fileType:AVFileTypeMPEG4 error:&error];
            if(!writer){fail([@"MP4 creation failed: " stringByAppendingString:error.localizedDescription]);return;}
            writerInput=[[AVAssetWriterInput alloc] initWithMediaType:AVMediaTypeVideo outputSettings:nil sourceFormatHint:format];
            writerInput.expectsMediaDataInRealTime=YES;
            // Pixel orientation is normalized before compression, including raw Annex B.
            if(![writer canAddInput:writerInput]){fail(@"MP4 cannot accept selected compressed codec");return;}
            [writer addInput:writerInput];writer.shouldOptimizeForNetworkUse=YES;
            if(![writer startWriting]){fail(writer.error.localizedDescription?:@"MP4 start failed");return;}
            [writer startSessionAtSourceTime:kCMTimeZero];
        }
        if(!writerInput.readyForMoreMediaData){fail(@"MP4 writer backpressure; diagnostic refuses to queue frames");return;}
        if(![writerInput appendSampleBuffer:sample]){fail(writer.error.localizedDescription?:@"MP4 append failed");return;}
        CFArrayRef attachments=CMSampleBufferGetSampleAttachmentsArray(sample,NO);
        BOOL key=YES;
        if(attachments&&CFArrayGetCount(attachments)>0){CFDictionaryRef dict=CFArrayGetValueAtIndex(attachments,0);key=!CFDictionaryContainsKey(dict,kCMSampleAttachmentKey_NotSync)||CFDictionaryGetValue(dict,kCMSampleAttachmentKey_NotSync)==kCFBooleanFalse;}
        int lengthSize=0;size_t parameterCount=0;
        const uint8_t *parameter=NULL;size_t parameterSize=0;
        OSStatus descriptionStatus=useHEVC
            ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,0,&parameter,&parameterSize,&parameterCount,&lengthSize)
            : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,0,&parameter,&parameterSize,&parameterCount,&lengthSize);
        if(descriptionStatus!=noErr || lengthSize!=4 || parameterCount<(useHEVC?3:2)){fail(@"Unsupported NAL length encoding or missing codec parameter sets");return;}
        if(key){keyframes++;for(size_t i=0;i<parameterCount;i++){
            OSStatus status=useHEVC ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,i,&parameter,&parameterSize,NULL,NULL) : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,i,&parameter,&parameterSize,NULL,NULL);
            if(status!=noErr){fail(@"Codec parameter set extraction failed");return;}
            if(!writeNAL(parameter,parameterSize))return;
        }}
        CMBlockBufferRef block=CMSampleBufferGetDataBuffer(sample);
        size_t total=CMBlockBufferGetDataLength(block),offset=0;
        // Compressed-byte copy only. Source pixels are never staged on the CPU.
        NSMutableData *data=[NSMutableData dataWithLength:total];
        if(CMBlockBufferCopyDataBytes(block,0,total,data.mutableBytes)!=kCMBlockBufferNoErr){fail(@"Could not read encoded block");return;}
        const uint8_t *raw=data.bytes;
        while(offset+4<=total){uint32_t size=0;memcpy(&size,raw+offset,4);size=CFSwapInt32BigToHost(size);offset+=4;if(size>total-offset){fail(@"Malformed encoded NAL size");return;}if(!writeNAL(raw+offset,size))return;offset+=size;}
        if(offset!=total){fail(@"Trailing bytes after encoded NAL");return;}
        encoded++;
    }
}
static BOOL createEncoder(CVPixelBufferRef pixel){
    NSDictionary *spec=@{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:@YES};
    NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey:@(CVPixelBufferGetPixelFormatType(pixel)),(__bridge NSString *)kCVPixelBufferWidthKey:@(width),(__bridge NSString *)kCVPixelBufferHeightKey:@(height),(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
    OSStatus result=VTCompressionSessionCreate(NULL,(int32_t)width,(int32_t)height,useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,(__bridge CFDictionaryRef)spec,(__bridge CFDictionaryRef)attributes,NULL,compressed,NULL,&encoder);
    if(result!=noErr){fail([NSString stringWithFormat:@"Required hardware %@ creation failed: %d",codecName,(int)result]);return NO;}
    NSDictionary *properties=@{(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:@30,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@30,(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:(__bridge NSString *)(useHEVC?kVTProfileLevel_HEVC_Main_AutoLevel:kVTProfileLevel_H264_Main_AutoLevel)};
    for(NSString *key in properties){result=VTSessionSetProperty(encoder,(__bridge CFStringRef)key,(__bridge CFTypeRef)properties[key]);if(result!=noErr){fail([NSString stringWithFormat:@"Encoder property rejected %@: %d",key,(int)result]);return NO;}}
    if(VTCompressionSessionPrepareToEncodeFrames(encoder)!=noErr){fail(@"Encoder preparation failed");return NO;}
    CFTypeRef usingHardware=NULL;
    result=VTSessionCopyProperty(encoder,kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,NULL,&usingHardware);
    hardwareVerified=result==noErr&&usingHardware&&CFGetTypeID(usingHardware)==CFBooleanGetTypeID()&&CFBooleanGetValue(usingHardware);
    if(usingHardware)CFRelease(usingHardware);
    if(!hardwareVerified){fail(@"Encoder did not confirm hardware acceleration; software fallback rejected");return NO;}
    return YES;
}
static BOOL normalizeSource(void){
    if(!metal){
        metal=MTLCreateSystemDefaultDevice();commandQueue=[metal newCommandQueue];
        NSError *error=nil;
        NSString *source=@"#include <metal_stdlib>\nusing namespace metal;\nkernel void normalize(texture2d<float,access::read> src [[texture(0)]], texture2d<float,access::write> dst [[texture(1)]], constant uint &flip [[buffer(0)]], uint2 p [[thread_position_in_grid]]) { if(p.x>=dst.get_width()||p.y>=dst.get_height())return; uint sy=flip ? src.get_height()-1-p.y : p.y; dst.write(src.read(uint2(p.x,sy)),p); }";
        id<MTLLibrary> library=[metal newLibraryWithSource:source options:nil error:&error];
        if(library)normalizePipeline=[metal newComputePipelineStateWithFunction:[library newFunctionWithName:@"normalize"] error:&error];
        if(!commandQueue||!normalizePipeline){fail(error.localizedDescription?:@"Metal normalization initialization failed");return NO;}
        NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{},(__bridge NSString *)kCVPixelBufferMetalCompatibilityKey:@YES};
        if(CVPixelBufferCreate(NULL,width,height,kCVPixelFormatType_32BGRA,(__bridge CFDictionaryRef)attributes,&normalizedPixel)!=kCVReturnSuccess){fail(@"Owned normalization pixel buffer allocation failed");return NO;}
    }
    if(top){fail(@"y0top=true scanout mapping not validated in this diagnostic");return NO;}
    if(IOSurfaceGetPixelFormat(surface)!=kCVPixelFormatType_32BGRA){fail(@"Normalization only supports verified BGRA scanout");return NO;}
    MTLTextureDescriptor *descriptor=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:width height:height mipmapped:NO];
    descriptor.storageMode=MTLStorageModeShared;descriptor.usage=MTLTextureUsageShaderRead;
    id<MTLTexture> input=[metal newTextureWithDescriptor:descriptor iosurface:surface plane:0];
    descriptor.usage=MTLTextureUsageShaderWrite;
    id<MTLTexture> output=[metal newTextureWithDescriptor:descriptor iosurface:CVPixelBufferGetIOSurface(normalizedPixel) plane:0];
    if(!input||!output){fail(@"Metal IOSurface texture binding failed");return NO;}
    id<MTLCommandBuffer> command=[commandQueue commandBuffer];id<MTLComputeCommandEncoder> compute=[command computeCommandEncoder];
    [compute setComputePipelineState:normalizePipeline];[compute setTexture:input atIndex:0];[compute setTexture:output atIndex:1];
    // This public UTM IOSurface path is already top-row-first in memory for
    // y0top=false. GL/CIImage coordinates differ from CVPixelBuffer row order.
    // Guest XWD and untransformed decoded pixels are the orientation oracle.
    uint32_t flip=0;[compute setBytes:&flip length:sizeof(flip) atIndex:0];
    [compute dispatchThreads:MTLSizeMake(width,height,1) threadsPerThreadgroup:MTLSizeMake(16,16,1)];[compute endEncoding];[command commit];[command waitUntilCompleted];
    if(command.status!=MTLCommandBufferStatusCompleted){fail(command.error.localizedDescription?:@"Metal normalization failed");return NO;}
    normalizationCopies++;
    if(!referenceSaved){
        // Diagnostic-only readback of frame one, separate from the encode input path.
        NSString *path=[prefix stringByAppendingString:@".reference.bgra"];
        int fd=open(path.fileSystemRepresentation,O_CREAT|O_EXCL|O_WRONLY,0600);
        if(fd<0){fail(@"Cannot create first normalized reference");return NO;}
        CVReturn status=CVPixelBufferLockBaseAddress(normalizedPixel,kCVPixelBufferLock_ReadOnly);
        if(status!=kCVReturnSuccess){close(fd);fail(@"Reference readback lock failed");return NO;}
        const uint8_t *base=CVPixelBufferGetBaseAddress(normalizedPixel);size_t stride=CVPixelBufferGetBytesPerRow(normalizedPixel);
        BOOL written=YES;for(NSUInteger row=0;row<height;row++)if(write(fd,base+row*stride,width*4)!=(ssize_t)(width*4)){written=NO;break;}
        CVPixelBufferUnlockBaseAddress(normalizedPixel,kCVPixelBufferLock_ReadOnly);close(fd);
        if(!written){fail(@"Reference readback write failed");return NO;}referenceSaved=YES;
    }
    return YES;
}
static void onScanout(SpiceDisplayChannel *channel,GParamSpec *spec,gpointer context){
    const SpiceGlScanout *info=spice_display_channel_get_gl_scanout(channel);if(!info)return;
    struct pollfd descriptor={.fd=info->fd,.events=POLLIN};
    if(poll(&descriptor,1,0)<=0 || !(descriptor.revents&POLLIN)){fail(@"IOSurface identifier unavailable");return;}
    IOSurfaceID id=0;
    if(read(info->fd,&id,sizeof(id))!=sizeof(id)){fail(@"Incomplete IOSurface identifier");return;}
    IOSurfaceRef replacement=IOSurfaceLookup(id);
    if(!replacement){fail(@"IOSurfaceLookup failed");return;}
    if(encoder&&(width!=info->width||height!=info->height||top!=info->y0top)){CFRelease(replacement);fail(@"Scanout dimensions/orientation changed; dynamic reconfiguration unimplemented in diagnostic");return;}
    if(surface)CFRelease(surface);surface=replacement;width=info->width;height=info->height;top=info->y0top;
    if(width==0||height==0||width>3840||height>2160||width%2||height%2){fail(@"Scanout dimensions exceed diagnostic budget");return;}
    if(width!=IOSurfaceGetWidth(surface)||height!=IOSurfaceGetHeight(surface)){fail(@"Scanout/IOSurface dimensions differ");return;}
    printf("SCANOUT width=%lu height=%lu pixel_format=%08x top=%d\n",(unsigned long)width,(unsigned long)height,(unsigned)IOSurfaceGetPixelFormat(surface),top);fflush(stdout);
}
static void onDraw(SpiceDisplayChannel *channel,guint32 x,guint32 y,guint32 w,guint32 h,gpointer context){
    received++;
    @autoreleasepool {
        gint64 now=g_get_monotonic_time();
        if(!surface||failure){spice_display_channel_gl_draw_done(channel);g_main_loop_quit(loop);return;}
        if(lastSample&&now-lastSample<33333){skipped++;spice_display_channel_gl_draw_done(channel);return;}
        CVPixelBufferRef pixel=NULL;
        CVReturn result=CVPixelBufferCreateWithIOSurface(NULL,surface,NULL,&pixel);
        if(result!=kCVReturnSuccess){fail([NSString stringWithFormat:@"CVPixelBufferCreateWithIOSurface failed: %d",(int)result]);}
        else {
            IOSurfaceRef wrapped=CVPixelBufferGetIOSurface(pixel);
            sharedSurfaceVerified&=wrapped&&IOSurfaceGetID(wrapped)==IOSurfaceGetID(surface);
            if(!sharedSurfaceVerified)fail(@"Pixel buffer does not reference the original IOSurface");
            if(!failure)normalizeSource();
            if(!failure&&!encoder)createEncoder(normalizedPixel);
            if(!failure){
                if(!firstTime)firstTime=now;
                lastSample=now;
                CMTime pts=CMTimeMake(now-firstTime,1000000);
                gint64 started=g_get_monotonic_time();submitted++;
                [sampleEvents addObject:@(received)];
                OSStatus status=VTCompressionSessionEncodeFrame(encoder,normalizedPixel,pts,CMTimeMake(1,30),NULL,NULL,NULL);
                if(status!=noErr)fail([NSString stringWithFormat:@"Encode submission failed: %d",(int)status]);
                else if(VTCompressionSessionCompleteFrames(encoder,pts)!=noErr)fail(@"Could not complete borrowed surface before producer ACK");
                [encodeWallMS addObject:@((g_get_monotonic_time()-started)/1000.0)];
            }
            CVPixelBufferRelease(pixel);
        }
    }
    // Exactly one borrowed source in flight. Blocking producer acknowledgement is
    // intentional for this bounded correctness probe, not a production latency policy.
    spice_display_channel_gl_draw_done(channel);
    if(failure||submitted>=targetFrames)g_main_loop_quit(loop);
}
static void onChannel(SpiceSession *session,SpiceChannel *channel,gpointer context){
    if(SPICE_IS_DISPLAY_CHANNEL(channel)){
        g_signal_connect(channel,"notify::gl-scanout",G_CALLBACK(onScanout),NULL);
        g_signal_connect(channel,"gl-draw",G_CALLBACK(onDraw),NULL);spice_channel_connect(channel);
    }
}
static gboolean timeout(gpointer context){fail(@"No complete 60-frame desktop capture within 45 seconds");g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
int main(int argc,char **argv){@autoreleasepool{
    if(argc!=3&&argc!=5){fprintf(stderr,"Usage: spice-h264-probe UNIX_SOCKET OUTPUT_PREFIX [--codec h264|hevc]\n");return 2;}
    const char *selection=getenv("VEYNODE_VIDEO_CODEC");
    if(argc==5){if(strcmp(argv[3],"--codec")!=0)return 2;selection=argv[4];}
    codecName=selection?[NSString stringWithUTF8String:selection]:@"h264";
    if(![codecName isEqual:@"h264"]&&![codecName isEqual:@"hevc"]){fprintf(stderr,"Unsupported codec; choose h264 or hevc\n");return 2;}
    useHEVC=[codecName isEqual:@"hevc"];
    umask(0077);
    prefix=[NSString stringWithUTF8String:argv[2]];lock=[NSLock new];sampleEvents=[NSMutableArray new];encodeWallMS=[NSMutableArray new];
    NSString *rawPath=[prefix stringByAppendingString:useHEVC?@".hevc":@".h264"];
    if([[NSFileManager defaultManager] fileExistsAtPath:[prefix stringByAppendingString:@".mp4"]]){fprintf(stderr,"Refusing to overwrite MP4\n");return 2;}
    int output=open(rawPath.fileSystemRepresentation,O_CREAT|O_EXCL|O_WRONLY,0600);
    if(output<0){perror("creating Annex B output");return 2;}
    annexB=fdopen(output,"wb");if(!annexB){close(output);return 2;}
    loop=g_main_loop_new(NULL,FALSE);SpiceSession *session=spice_session_new();
    g_object_set(session,"unix-path",argv[1],NULL);g_signal_connect(session,"channel-new",G_CALLBACK(onChannel),NULL);
    if(!spice_session_connect(session))fail(@"SPICE connection failed");
    else{g_timeout_add_seconds(45,timeout,NULL);g_main_loop_run(loop);}
    if(encoder){VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid);VTCompressionSessionInvalidate(encoder);CFRelease(encoder);}
    if(writer){[writerInput markAsFinished];dispatch_semaphore_t done=dispatch_semaphore_create(0);[writer finishWritingWithCompletionHandler:^{dispatch_semaphore_signal(done);}];if(dispatch_semaphore_wait(done,dispatch_time(DISPATCH_TIME_NOW,10*NSEC_PER_SEC))!=0)fail(@"MP4 finalization timed out");else if(writer.status!=AVAssetWriterStatusCompleted)fail(writer.error.localizedDescription?:@"MP4 finalization failed");}
    if(fclose(annexB)!=0)fail(@"Annex B output close failed");
    spice_session_disconnect(session);g_object_unref(session);g_main_loop_unref(loop);if(surface)CFRelease(surface);if(normalizedPixel)CVPixelBufferRelease(normalizedPixel);
    BOOL success=!failure&&hardwareVerified&&sharedSurfaceVerified&&submitted==targetFrames&&encoded==submitted&&bytesWritten>0;
    NSDictionary *report=@{@"success":@(success),@"codec":codecName,@"encoder_profile":useHEVC?@"HEVC_Main_AutoLevel":@"H264_Main_AutoLevel",@"error":failure?:[NSNull null],@"scope":@"Actual SPICE desktop surface hardware encode diagnostic; desktop visual correctness, WebRTC, browser decode and performance acceptance are separate.",@"os":NSProcessInfo.processInfo.operatingSystemVersionString,@"source":@"SPICE GL scanout IOSurface",@"width":@(width),@"height":@(height),@"y0top":@(top),@"hardware_required":@YES,@"hardware_verified":@(hardwareVerified),@"source_wrapper_reuses_original_iosurface":@(sharedSurfaceVerified),@"encoder_input_reuses_original_iosurface":@NO,@"encoder_input":@"owned IOSurface-backed pixel buffer after Metal normalization",@"normalization_gpu_copies":@(normalizationCopies),@"vertical_flip_applied":@NO,@"orientation_mapping":@"y0top=false GL scanout has top-row-first IOSurface memory; verified against actual Guest XWD",@"bitstream_orientation":@"top-left",@"normalization_device":metal.name?:@"unavailable",@"cpu_pixel_copy_in_encode_path":@NO,@"diagnostic_first_frame_cpu_readback":@(referenceSaved),@"encoder_internal_copies_measured":@NO,@"max_inflight_input_frames":@1,@"producer_ack_policy":@"after synchronous hardware encode completion; deliberately blocks producer",@"sample_fps_limit":@30,@"input_draw_events":@(received),@"input_frames_submitted":@(submitted),@"input_frames_skipped_by_rate_limit":@(skipped),@"encoder_output_frames":@(encoded),@"keyframes":@(keyframes),@"annex_b_bytes":@(bytesWritten),@"sampled_draw_event_numbers":sampleEvents,@"encode_and_drain_wall_ms":encodeWallMS,@"mp4":[prefix stringByAppendingString:@".mp4"],(useHEVC?@"hevc":@"h264"):rawPath};
    NSData *json=[NSJSONSerialization dataWithJSONObject:report options:NSJSONWritingPrettyPrinted|NSJSONWritingSortedKeys error:nil];
    [json writeToFile:[prefix stringByAppendingString:@".encode.json"] atomically:YES];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return success?0:1;
}}
