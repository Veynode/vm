#include "hevc-profile.h"
// Integration diagnostic, derived from the validated spice-h264-probe codec/Metal path.
// stdout is framed Annex B only; no reference pixels or recordings are saved.
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#import <IOSurface/IOSurface.h>
#import <Metal/Metal.h>
#include <glib.h>
#include <spice-client.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <errno.h>
#include <math.h>
#include <sys/ioctl.h>
static GMainLoop *loop;
static IOSurfaceRef surface;
static VTCompressionSessionRef encoder;
static CVPixelBufferRef normalizedPixel;
static id<MTLDevice> metal;
static id<MTLCommandQueue> commandQueue;
static id<MTLComputePipelineState> normalizePipeline;
static NSLock *lock;
static NSString *failure,*codecName,*actualH264Profile;
static NSDictionary *actualHEVCProfile;
static BOOL useHEVC,top,hardwareVerified,forceKeyframe,hardwareProbe,probeOutput;
static NSUInteger sourceWidth,sourceHeight,width,height,normalizationCopies,received,submitted,encoded,skipped,keyframes;
static gint64 firstTime,lastSample;
static NSUInteger generation,cachedGeneration,idleFrames,cachedKeyframes,cacheInvalidations;
static BOOL cachedFrameValid,loggedIdle,loggedCachedKeyframe;
static int generationFD=-1;
static NSMutableData *controlInput;
static const NSUInteger maxAU=4*1024*1024;
static BOOL surfaceLive(void);
static void fail(NSString *message){[lock lock];if(!failure)failure=message;[lock unlock];}
static void diagnostic(NSDictionary *value){NSData *data=[NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:nil];fwrite(data.bytes,1,data.length,stderr);fputc('\n',stderr);fflush(stderr);}
static BOOL appendNAL(NSMutableData *au,const void *bytes,size_t count){static const uint8_t start[]={0,0,0,1};if(count==0||count>maxAU-4||au.length>maxAU-count-4){fail(@"Access unit exceeds 4MiB or empty NAL");return NO;}[au appendBytes:start length:4];[au appendBytes:bytes length:count];return YES;}
static BOOL writeRecord(NSData *payload,uint64_t pts,BOOL key){
 uint8_t header[13];uint32_t length=CFSwapInt32HostToBig((uint32_t)payload.length);uint64_t timestamp=CFSwapInt64HostToBig(pts);memcpy(header,&length,4);memcpy(header+4,&timestamp,8);header[12]=key?1:0;
 const uint8_t *parts[]={header,payload.bytes};size_t lengths[]={sizeof(header),payload.length};gint64 deadline=g_get_monotonic_time()+500000;
 for(int part=0;part<2;part++){size_t sent=0;while(sent<lengths[part]){
  if(g_get_monotonic_time()>=deadline){fail(@"stdout backpressure exceeded 500ms; partial record must be discarded");return NO;}
  ssize_t amount=write(STDOUT_FILENO,parts[part]+sent,lengths[part]-sent);
  if(amount>0){sent+=(size_t)amount;continue;}if(amount<0&&errno==EINTR)continue;
  if(amount<0&&(errno==EAGAIN||errno==EWOULDBLOCK)){struct pollfd p={.fd=STDOUT_FILENO,.events=POLLOUT};int wait=(int)MAX(1,(deadline-g_get_monotonic_time())/1000);int result=poll(&p,1,wait);if(result<0&&errno==EINTR)continue;if(result>0&&!(p.revents&(POLLHUP|POLLERR|POLLNVAL)))continue;}
  fail(@"stdout closed, errored or stalled; partial record must be discarded");return NO;
 }}return YES;
}
static void compressed(void *context,void *source,OSStatus status,VTEncodeInfoFlags flags,CMSampleBufferRef sample){@autoreleasepool{
 if(status!=noErr||!sample||!CMSampleBufferDataIsReady(sample)||(flags&kVTEncodeInfo_FrameDropped)){fail([NSString stringWithFormat:@"Encode callback failed status=%d flags=%u",(int)status,(unsigned)flags]);return;}
 CMFormatDescriptionRef format=CMSampleBufferGetFormatDescription(sample);
 if(CMFormatDescriptionGetMediaSubType(format)!=(useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264)){fail(@"Unexpected output codec");return;}
 if(hardwareProbe){probeOutput=YES;return;}
 if(!surfaceLive()){fail(@"Source generation ended during encode; frame discarded");return;}
 CFArrayRef attachments=CMSampleBufferGetSampleAttachmentsArray(sample,NO);BOOL key=YES;
 if(attachments&&CFArrayGetCount(attachments)){CFDictionaryRef dict=CFArrayGetValueAtIndex(attachments,0);key=!CFDictionaryContainsKey(dict,kCMSampleAttachmentKey_NotSync)||CFDictionaryGetValue(dict,kCMSampleAttachmentKey_NotSync)==kCFBooleanFalse;}
 const uint8_t *parameter=NULL;size_t parameterSize=0,parameterCount=0;int lengthSize=0;
 OSStatus result=useHEVC?CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,0,&parameter,&parameterSize,&parameterCount,&lengthSize):CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,0,&parameter,&parameterSize,&parameterCount,&lengthSize);
 if(result!=noErr||lengthSize!=4||parameterCount<(useHEVC?3:2)){fail(@"Missing parameter sets or unsupported NAL length");return;}
 if(!useHEVC&&(parameterSize<4||(parameter[0]&31)!=7||parameter[1]!=0x42||(parameter[2]&0x4f)!=0x40||parameter[3]!=0x1f)){fail(@"Actual H264 SPS does not match constrained Baseline3.1 (RFC6184 profile42 / iop x1xx0000)");return;}
 if(!useHEVC&&!actualH264Profile){actualH264Profile=[NSString stringWithFormat:@"%02x%02x%02x",parameter[1],parameter[2],parameter[3]];diagnostic(@{@"event":@"codec-parameters",@"codec":codecName,@"actual_h264_profile_level_id":actualH264Profile,@"h264_subprofile":@"Constrained Baseline",@"level":@31});}
 if(useHEVC){
  BOOL found=NO;VeynodeHEVCProfile profile={0};
  for(size_t i=0;i<parameterCount;i++){
   result=CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,i,&parameter,&parameterSize,NULL,NULL);
   if(result!=noErr||parameterSize<2){fail(@"HEVC parameter extraction failed");return;}
   if(((parameter[0]>>1)&63)==33){
    if(found||!veynode_hevc_main_level4(parameter,parameterSize,&profile)){fail(@"Actual HEVC SPS exceeds Main/tier0/level4 or has unsupported/malformed syntax");return;}
    found=YES;
   }
  }
  if(!found){fail(@"Missing actual HEVC SPS");return;}
  NSDictionary *current=@{@"profile_space":@(profile.profile_space),@"profile_id":@(profile.profile_id),@"tier_flag":@(profile.tier),@"level_id":@(profile.level)};
  if(!actualHEVCProfile){actualHEVCProfile=current;diagnostic(@{@"event":@"codec-parameters",@"codec":codecName,@"actual_hevc_profile_tier_level":current});}
  else if(![actualHEVCProfile isEqual:current]){fail(@"HEVC profile changed during session");return;}
 }
 NSMutableData *au=[NSMutableData new];
 if(key)for(size_t i=0;i<parameterCount;i++){
  result=useHEVC?CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,i,&parameter,&parameterSize,NULL,NULL):CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,i,&parameter,&parameterSize,NULL,NULL);
  if(result!=noErr){fail(@"Parameter set extraction failed");return;}if(!appendNAL(au,parameter,parameterSize))return;
 }
 CMBlockBufferRef block=CMSampleBufferGetDataBuffer(sample);size_t total=CMBlockBufferGetDataLength(block),offset=0;
 if(total>maxAU){fail(@"Compressed buffer exceeds 4MiB");return;}NSMutableData *bytes=[NSMutableData dataWithLength:total];
 if(CMBlockBufferCopyDataBytes(block,0,total,bytes.mutableBytes)!=noErr){fail(@"Cannot read encoded block");return;}
 const uint8_t *raw=bytes.bytes;while(offset+4<=total){uint32_t size=0;memcpy(&size,raw+offset,4);size=CFSwapInt32BigToHost(size);offset+=4;if(size>total-offset){fail(@"Malformed NAL length");return;}if(!appendNAL(au,raw+offset,size))return;offset+=size;}
 CMTime pts=CMTimeConvertScale(CMSampleBufferGetPresentationTimeStamp(sample),1000000,kCMTimeRoundingMethod_Default);
 if(offset!=total||!CMTIME_IS_NUMERIC(pts)||pts.value<0||au.length==0){fail(@"Invalid AU or timestamp");return;}
 if(writeRecord(au,(uint64_t)pts.value,key)){encoded++;if(key)keyframes++;}
}}
static BOOL createEncoder(CVPixelBufferRef pixel){
    NSDictionary *spec=@{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:@YES};
    NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey:@(CVPixelBufferGetPixelFormatType(pixel)),(__bridge NSString *)kCVPixelBufferWidthKey:@(width),(__bridge NSString *)kCVPixelBufferHeightKey:@(height),(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
    OSStatus result=VTCompressionSessionCreate(NULL,(int32_t)width,(int32_t)height,useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,(__bridge CFDictionaryRef)spec,(__bridge CFDictionaryRef)attributes,NULL,compressed,NULL,&encoder);
    if(result!=noErr){fail([NSString stringWithFormat:@"Required hardware %@ creation failed: %d",codecName,(int)result]);return NO;}
    NSDictionary *properties=@{(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:@30,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@30,(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:(__bridge NSString *)(useHEVC?kVTProfileLevel_HEVC_Main_AutoLevel:kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
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
        NSString *source=@"#include <metal_stdlib>\nusing namespace metal;\nkernel void normalize(texture2d<float,access::read> src [[texture(0)]], texture2d<float,access::write> dst [[texture(1)]], constant uint &flip [[buffer(0)]], uint2 p [[thread_position_in_grid]]) { if(p.x>=dst.get_width()||p.y>=dst.get_height())return; uint sx=min(src.get_width()-1,((2*p.x+1)*src.get_width())/(2*dst.get_width())); uint sy=min(src.get_height()-1,((2*p.y+1)*src.get_height())/(2*dst.get_height())); if(flip)sy=src.get_height()-1-sy; dst.write(src.read(uint2(sx,sy)),p); }";
        id<MTLLibrary> library=[metal newLibraryWithSource:source options:nil error:&error];
        if(library)normalizePipeline=[metal newComputePipelineStateWithFunction:[library newFunctionWithName:@"normalize"] error:&error];
        if(!commandQueue||!normalizePipeline){fail(error.localizedDescription?:@"Metal normalization initialization failed");return NO;}
    }
    if(!normalizedPixel){
        NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{},(__bridge NSString *)kCVPixelBufferMetalCompatibilityKey:@YES};
        if(CVPixelBufferCreate(NULL,width,height,kCVPixelFormatType_32BGRA,(__bridge CFDictionaryRef)attributes,&normalizedPixel)!=kCVReturnSuccess){fail(@"Owned normalization pixel buffer allocation failed");return NO;}
    }
    if(top){fail(@"y0top=true scanout mapping not validated in this diagnostic");return NO;}
    if(IOSurfaceGetPixelFormat(surface)!=kCVPixelFormatType_32BGRA){fail(@"Normalization only supports verified BGRA scanout");return NO;}
    MTLTextureDescriptor *descriptor=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:sourceWidth height:sourceHeight mipmapped:NO];
    descriptor.storageMode=MTLStorageModeShared;descriptor.usage=MTLTextureUsageShaderRead;
    id<MTLTexture> input=[metal newTextureWithDescriptor:descriptor iosurface:surface plane:0];
    descriptor.width=width;descriptor.height=height;descriptor.usage=MTLTextureUsageShaderWrite;
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
    return YES;
}
static BOOL surfaceLive(void){if(generationFD<0)return NO;struct pollfd p={.fd=generationFD,.events=POLLIN};return poll(&p,1,0)>=0&&!(p.revents&(POLLHUP|POLLERR|POLLNVAL));}
// All cache mutations and submissions run on the same GLib main loop. Each
// submission calls CompleteFrames before returning; VT cannot still be using a
// buffer when a scanout callback or the idle timer releases/replaces it.
static void clearOwnedFrame(void){if(cachedFrameValid)cacheInvalidations++;cachedFrameValid=NO;cachedGeneration=0;if(normalizedPixel){CVPixelBufferRelease(normalizedPixel);normalizedPixel=NULL;}}
static void encodeOwned(gint64 now,BOOL replay){
 if(!cachedFrameValid||cachedGeneration!=generation||!normalizedPixel||!surfaceLive()){clearOwnedFrame();fail(@"Owned frame generation invalidated before encode");return;}
 BOOL forced=forceKeyframe;forceKeyframe=NO;
 if(!firstTime)firstTime=now;lastSample=now;CMTime pts=CMTimeMake(now-firstTime,1000000);
 NSDictionary *options=forced?@{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame:@YES}:nil;
 // Keep an explicit retain through completion, independent of the cache owner.
 CVPixelBufferRef input=CVPixelBufferRetain(normalizedPixel);submitted++;
 OSStatus status=VTCompressionSessionEncodeFrame(encoder,input,pts,CMTimeMake(1,30),(__bridge CFDictionaryRef)options,NULL,NULL);
 if(status!=noErr)fail([NSString stringWithFormat:@"Encode submission failed:%d",(int)status]);
 else if(VTCompressionSessionCompleteFrames(encoder,pts)!=noErr)fail(@"Encode completion failed");
 CVPixelBufferRelease(input);
 if(replay&&!failure){
  if(forced){cachedKeyframes++;if(!loggedCachedKeyframe){loggedCachedKeyframe=YES;diagnostic(@{@"event":@"cached-keyframe",@"generation":@(generation),@"draw_events":@(received),@"source_copies":@(normalizationCopies),@"encoded":@(encoded)});}}
  else{idleFrames++;if(!loggedIdle){loggedIdle=YES;diagnostic(@{@"event":@"idle-refresh",@"generation":@(generation),@"draw_events":@(received),@"source_copies":@(normalizationCopies),@"encoded":@(encoded),@"diagnostic_idle_fps":@1});}}
 }
}
static gboolean refresh(gpointer data){
 if(generationFD>=0&&!surfaceLive()){clearOwnedFrame();fail(@"Source generation invalidated while idle");}
 if(failure){g_main_loop_quit(loop);return G_SOURCE_CONTINUE;}
 gint64 now=g_get_monotonic_time();
 if(encoder&&cachedFrameValid&&((forceKeyframe&&now-lastSample>=33333)||now-lastSample>=1000000))encodeOwned(now,YES);
 if(failure)g_main_loop_quit(loop);return G_SOURCE_CONTINUE;
}
static void onScanout(SpiceDisplayChannel *channel,GParamSpec *spec,gpointer context){
 clearOwnedFrame();generation++;
 const SpiceGlScanout *info=spice_display_channel_get_gl_scanout(channel);if(!info){if(generationFD>=0)close(generationFD);generationFD=-1;if(surface)CFRelease(surface);surface=NULL;return;}
 struct pollfd p={.fd=info->fd,.events=POLLIN};int available=0;
 if(ioctl(info->fd,FIONREAD,&available)!=0||available!=4||poll(&p,1,0)!=1||!(p.revents&POLLIN)||(p.revents&(POLLHUP|POLLERR))){fail(@"No fresh live IOSurface ID");g_main_loop_quit(loop);return;}
 IOSurfaceID ident=0;if(read(info->fd,&ident,4)!=4){fail(@"Incomplete IOSurface ID");g_main_loop_quit(loop);return;}
 IOSurfaceRef replacement=IOSurfaceLookup(ident);
 if(!replacement){fail(@"IOSurface lookup failed");g_main_loop_quit(loop);return;}
 if((encoder&&(sourceWidth!=info->width||sourceHeight!=info->height||top!=info->y0top))||info->width==0||info->height==0||info->width>(useHEVC?1920:3840)||info->height>(useHEVC?1080:2160)||info->width%2||info->height%2||IOSurfaceGetWidth(replacement)!=info->width||IOSurfaceGetHeight(replacement)!=info->height){CFRelease(replacement);fail(@"Unsupported dimensions or live reconfiguration");g_main_loop_quit(loop);return;}
 int fd=dup(info->fd);if(fd<0){CFRelease(replacement);fail(@"Cannot retain scanout FD");g_main_loop_quit(loop);return;}fcntl(fd,F_SETFD,FD_CLOEXEC);
 if(generationFD>=0)close(generationFD);generationFD=fd;if(surface)CFRelease(surface);surface=replacement;sourceWidth=info->width;sourceHeight=info->height;top=info->y0top;
 // ConstrainedBaseline_AutoLevel is still gated by actual SPS level31.
 // Level3.1 fits <=3600 macroblocks at30fps. Aspect-preserving even dimensions
 // bounded by1280x720 prevent the original1280x800 profile/SDP mismatch.
 double scale=useHEVC?1.0:fmin(1.0,fmin(1280.0/sourceWidth,720.0/sourceHeight));
 width=(NSUInteger)floor(sourceWidth*scale/2.0)*2;height=(NSUInteger)floor(sourceHeight*scale/2.0)*2;
 if(width<2||height<2){fail(@"Scaled dimensions too small");g_main_loop_quit(loop);return;}
}
static void onDraw(SpiceDisplayChannel *channel,guint32 x,guint32 y,guint32 w,guint32 h,gpointer context){@autoreleasepool{
 received++;gint64 now=g_get_monotonic_time();
 if(!failure&&(!surface||!surfaceLive()))fail(@"Source generation unavailable or invalidated");
 if(!failure&&lastSample&&now-lastSample<33333){skipped++;spice_display_channel_gl_draw_done(channel);return;}
 if(!failure){cachedFrameValid=NO;if(normalizeSource()){cachedFrameValid=YES;cachedGeneration=generation;}}
 if(!failure&&!encoder&&createEncoder(normalizedPixel))diagnostic(@{@"event":@"ready",@"codec":codecName,@"hardware_verified":@(hardwareVerified),@"width":@(width),@"height":@(height),@"source_width":@(sourceWidth),@"source_height":@(sourceHeight),@"requested_h264_profile_level_id":useHEVC?[NSNull null]:@"42e01f",@"resize_filter":@"center-nearest",@"max_inflight_frames":@1,@"stdout_timeout_ms":@500,@"diagnostic_idle_fps":@1});
 if(!failure)encodeOwned(now,NO);
 // Bounded integration diagnostic: ACK waits for copy, encoder and pipe write.
 spice_display_channel_gl_draw_done(channel);if(failure)g_main_loop_quit(loop);
}}
static void channelEvent(SpiceChannel *channel,SpiceChannelEvent event,gpointer data){if(event==SPICE_CHANNEL_CLOSED||event>=SPICE_CHANNEL_ERROR_CONNECT){clearOwnedFrame();fail(@"SPICE channel disconnected");g_main_loop_quit(loop);}}
static void onChannel(SpiceSession *session,SpiceChannel *channel,gpointer context){
 if(SPICE_IS_DISPLAY_CHANNEL(channel)){g_signal_connect(channel,"notify::gl-scanout",G_CALLBACK(onScanout),NULL);g_signal_connect(channel,"gl-draw",G_CALLBACK(onDraw),NULL);g_signal_connect(channel,"channel-event",G_CALLBACK(channelEvent),NULL);spice_channel_connect(channel);}else if(SPICE_IS_MAIN_CHANNEL(channel))g_signal_connect(channel,"channel-event",G_CALLBACK(channelEvent),NULL);
}
static gboolean control(GIOChannel *channel,GIOCondition condition,gpointer context){
 char chunk[128];ssize_t count=read(STDIN_FILENO,chunk,sizeof(chunk));
 if(count==0){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
 if(count<0){if(errno==EINTR||errno==EAGAIN)return G_SOURCE_CONTINUE;fail(@"stdin error");g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
 for(ssize_t i=0;i<count;i++){
  if(chunk[i]=='\n'){NSString *command=[[NSString alloc] initWithData:controlInput encoding:NSUTF8StringEncoding];[controlInput setLength:0];if([command isEqual:@"keyframe"]){forceKeyframe=YES;refresh(NULL);}else if([command isEqual:@"stop"]){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}else{fail(@"Unsupported stdin command");g_main_loop_quit(loop);return G_SOURCE_REMOVE;}}
  else{[controlInput appendBytes:chunk+i length:1];if(controlInput.length>64){fail(@"stdin command too long");g_main_loop_quit(loop);return G_SOURCE_REMOVE;}}
 }return G_SOURCE_CONTINUE;
}
static gboolean deadline(gpointer data){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
static int probeCodecs(void){
 hardwareProbe=YES;width=height=64;NSMutableDictionary *result=[NSMutableDictionary new];
 NSDictionary *attrs=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};CVPixelBufferRef pixel=NULL;
 if(CVPixelBufferCreate(NULL,64,64,kCVPixelFormatType_32BGRA,(__bridge CFDictionaryRef)attrs,&pixel)!=kCVReturnSuccess)return 1;
 CVPixelBufferLockBaseAddress(pixel,0);memset(CVPixelBufferGetBaseAddress(pixel),80,CVPixelBufferGetBytesPerRow(pixel)*64);CVPixelBufferUnlockBaseAddress(pixel,0);
 for(NSString *name in @[@"h264",@"hevc"]){codecName=name;useHEVC=[name isEqual:@"hevc"];failure=nil;hardwareVerified=NO;probeOutput=NO;encoder=NULL;
  if(createEncoder(pixel)){OSStatus status=VTCompressionSessionEncodeFrame(encoder,pixel,kCMTimeZero,CMTimeMake(1,30),NULL,NULL,NULL);if(status!=noErr||VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid)!=noErr)fail(@"Probe frame encode failed");}
  result[useHEVC?@"h265":@"h264"]=(!failure&&hardwareVerified&&probeOutput)?@"verified-hardware":@"unavailable";
  if(encoder){VTCompressionSessionInvalidate(encoder);CFRelease(encoder);encoder=NULL;}
 }
 CVPixelBufferRelease(pixel);result[@"source"]=@"VideoToolbox required-hardware session property";
 NSData *json=[NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingSortedKeys error:nil];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return 0;
}
int main(int argc,char **argv){@autoreleasepool{
 lock=[NSLock new];if(argc==2&&strcmp(argv[1],"--probe-codecs")==0)return probeCodecs();
 if(argc!=4||strcmp(argv[2],"--codec")!=0){diagnostic(@{@"event":@"error",@"error":@"Usage: spice-video-stream UNIX_SOCKET --codec h264|hevc"});return 2;}
 codecName=[NSString stringWithUTF8String:argv[3]];if(![codecName isEqual:@"h264"]&&![codecName isEqual:@"hevc"]){diagnostic(@{@"event":@"error",@"error":@"Unsupported codec"});return 2;}useHEVC=[codecName isEqual:@"hevc"];
 signal(SIGPIPE,SIG_IGN);if(fcntl(STDOUT_FILENO,F_SETFL,fcntl(STDOUT_FILENO,F_GETFL)|O_NONBLOCK)<0||fcntl(STDIN_FILENO,F_SETFL,fcntl(STDIN_FILENO,F_GETFL)|O_NONBLOCK)<0){diagnostic(@{@"event":@"error",@"error":@"Cannot configure bounded I/O"});return 1;}
 loop=g_main_loop_new(NULL,FALSE);controlInput=[NSMutableData new];GIOChannel *input=g_io_channel_unix_new(STDIN_FILENO);g_io_add_watch(input,G_IO_IN|G_IO_HUP|G_IO_ERR,control,NULL);
 SpiceSession *session=spice_session_new();g_object_set(session,"unix-path",argv[1],NULL);g_signal_connect(session,"channel-new",G_CALLBACK(onChannel),NULL);
 if(!spice_session_connect(session))fail(@"SPICE connection failed");else{g_timeout_add_seconds(180,deadline,NULL);g_timeout_add(100,refresh,NULL);g_main_loop_run(loop);}
 if(encoder){VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid);VTCompressionSessionInvalidate(encoder);CFRelease(encoder);}
 GList *channels=spice_session_get_channels(session);for(GList *item=channels;item;item=item->next)g_signal_handlers_disconnect_by_func(item->data,G_CALLBACK(channelEvent),NULL);g_list_free(channels);
 spice_session_disconnect(session);g_object_unref(session);g_io_channel_unref(input);g_main_loop_unref(loop);if(generationFD>=0)close(generationFD);if(surface)CFRelease(surface);clearOwnedFrame();
 diagnostic(@{@"event":failure?@"error":@"stopped",@"error":failure?:[NSNull null],@"codec":codecName,@"hardware_verified":@(hardwareVerified),@"width":@(width),@"height":@(height),@"source_width":@(sourceWidth),@"source_height":@(sourceHeight),@"actual_h264_profile_level_id":actualH264Profile?:[NSNull null],@"actual_hevc_profile_tier_level":actualHEVCProfile?:[NSNull null],@"submitted":@(submitted),@"encoded":@(encoded),@"keyframes":@(keyframes),@"skipped":@(skipped),@"normalization_gpu_copies":@(normalizationCopies),@"draw_events":@(received),@"idle_refresh_frames":@(idleFrames),@"cached_keyframe_frames":@(cachedKeyframes),@"cache_invalidations":@(cacheInvalidations),@"cached_frame_released":@(!cachedFrameValid&&normalizedPixel==NULL)});return failure?1:0;
}}
