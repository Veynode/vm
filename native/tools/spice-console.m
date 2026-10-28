#include "../shared/encoder-rate.h"
// Per-VM persistent SPICE capture owner. Local single-owner framed pipe only.
#include "hevc-profile.h"
#include "../console/EncodedGeometry.h"
#include "../console/ScreenshotDiagnostic.h"
#include "../console/FramePacing.h"
#ifdef VEYNODE_NATIVE_DISPLAY
#import "../console/Display.h"
#import "../console/InputFrameDiagnostic.h"
#import "../console/FrameAcquisition.h"
#import "../console/CopyTiming.h"
#import "../console/AsyncScreenshot.h"
#import "../console/RecordWriter.h"
#import "../console/Media.h"
#include <time.h>
#include <math.h>
#define main VeynodeConsoleMain
#endif
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <VideoToolbox/VideoToolbox.h>
#import <IOSurface/IOSurface.h>
#import <ImageIO/ImageIO.h>
#import <CoreGraphics/CoreGraphics.h>
#include <spice-client.h>
#include <spice/vd_agent.h>
#include <glib.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <fcntl.h>
#include <signal.h>
#include <unistd.h>
#include <errno.h>
static GMainLoop *loop;
static IOSurfaceRef surface;
static CVPixelBufferRef owned,scaled;
static VTCompressionSessionRef encoder;
static id<MTLDevice> metal;
static id<MTLCommandQueue> queue;
static id<MTLComputePipelineState> pipeline;
static NSLock *lock;
static NSString *failure,*fatal,*codecName,*actualH264Profile;
static NSDictionary *actualHEVCProfile;
static BOOL useHEVC,hardwareVerified,hardwareProbe,probeOutput,active,forceKeyframe,mainOpen,displayOpen,readySent,closing,awaitingSource=YES;
static NSUInteger sourceWidth,sourceHeight,width,height,encoded,keyframes,sourceCopies,resizeCopies,received,idleFrames;
static uint64_t epoch,lastEpoch,generation,sequence,cachedGeneration,rateSubmitted;
// set_crop: a zero size is the full source. set_scale: 0x0 is the default codec-fit
// geometry of the view. carriedBitrate keeps the rate across a rebuild for either.
static NSUInteger cropX,cropY,cropWidth,cropHeight,scaleWidth,scaleHeight;
static NSNumber *carriedBitrate;
// A still screen gets one full-quality keyframe from a second session, which then carries the
// idle refreshes until the screen changes (enterStill). The speed-first session is parked meanwhile.
static VTCompressionSessionRef parkedEncoder;
static BOOL stillActive,stillFailed,creatingStill;
static int h264Level=31; // 40 only when the start request asked for it (WebCodecs desktop viewers)
static gint64 lastChange,lastStill;
static NSUInteger scanoutLogCount,channelLogCount;
#ifdef VEYNODE_NATIVE_DISPLAY
static VNFramePacing capturePacing;
static VNCopyTiming *copyTiming;
static VNRecordWriter *outputWriter;
static VNAsyncScreenshot *screenshotJob;
static VNWriteReceipt *screenshotWrite;
static NSString *screenshotID,*shutdownID;
static NSDictionary *screenshotMetadata,*lastScreenshotTiming;
static NSMutableDictionary *screenshotTiming;
static BOOL screenshotDelivered,shutdownReplied;
static double screenshotDeadline,shutdownDeadline;
static double nativeClock(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
static NSUInteger capturePacingDrops,capturePoolDrops,visibleSourceCopies,backgroundSourceCopies;
#endif
static gint64 capturedWall,capturedMono,lastCapture,firstPTS,lastEncode;
// Trailing-edge capture/encode: an update that arrived inside the pacing
// interval is captured and encoded at the next slot instead of waiting for
// another draw or the 1 Hz idle refresh (typing would otherwise lag seconds).
static BOOL sourceDirty,encodeDirty;
// With nobody to see frames (no visible window, no browser stream) a draw is only
// recorded and ACKed: no copy, no GPU round trip before the ACK. A screenshot, `start`
// or a window that appears copies the latest drawn scanout then (whenSourceCurrent:
// inside the next draw while the Guest is drawing, at once on a quiet screen).
// A draw not copied at once takes its sequence number at once (lazySequence), so
// frame_status and screen waits see every draw; the copy that includes it reuses it.
static BOOL lazySequence;static uint64_t drawnGeneration;static gint64 lastDrawMono;
static NSUInteger deferredDraws,onDemandCopies;
// Timers that run only while they have work: the flush is one shot at the next
// capture or encode slot; the screenshot tick runs while a screenshot or shutdown is pending.
static guint flushSource;static gint64 flushDue;
#ifdef VEYNODE_NATIVE_DISPLAY
static guint screenshotSource;
#endif
// A browser stream's rate: the viewer's screen refresh it reports with `set_fps`,
// else 240 Hz, the fastest Mac display. Frames past what the viewer's screen
// shows would only split the same bitrate thinner.
static const unsigned maxStreamFPS=240;
static unsigned streamFPS=maxStreamFPS;
static gint64 encodeInterval(void){return 1000000/streamFPS;}
// Capture runs at the stream's rate unless a visible native window wants more; only
// then does encoding need its own grid. A second grid at the same rate would sit at
// another phase and refuse captures (at 120 the stream lost about a third of its frames).
static unsigned captureFPS=maxStreamFPS;
// Copies not encoded because they matched the previous frame, this epoch.
static NSUInteger unchangedSkips;
static VNFramePacing encodePacing;
static int generationFD=-1;
static NSMutableData *input;
static const NSUInteger maxAU=4*1024*1024;
#ifdef VEYNODE_SCREENSHOT_TESTING
static NSUInteger testDrawACKs,testCopiesAtACK;
#define ACK_DRAW(channel) (testDrawACKs++,testCopiesAtACK=sourceCopies)
#else
#define ACK_DRAW(channel) spice_display_channel_gl_draw_done(channel)
#endif
static BOOL surfaceLive(void){if(generationFD<0)return NO;struct pollfd p={.fd=generationFD,.events=POLLIN};return poll(&p,1,0)>=0&&!(p.revents&(POLLHUP|POLLERR|POLLNVAL));}
static void fail(NSString *message){[lock lock];if(!failure)failure=message;[lock unlock];}
static void captureFail(NSString *message){if(!fatal)fatal=message;if(loop)g_main_loop_quit(loop);}
static BOOL record(uint8_t kind,uint64_t recordEpoch,NSData *body){
#ifdef VEYNODE_NATIVE_DISPLAY
 if(!outputWriter||![outputWriter enqueueKind:kind epoch:recordEpoch body:body]){captureFail(@"stdout_closed_or_stalled");return NO;}
 return YES;
#else
 if(body.length==0||body.length>maxAU){captureFail(@"output_record_limit");return NO;}
 uint8_t header[13];uint32_t n=CFSwapInt32HostToBig((uint32_t)body.length);uint64_t e=CFSwapInt64HostToBig(recordEpoch);memcpy(header,&n,4);header[4]=kind;memcpy(header+5,&e,8);
 gint64 deadline=g_get_monotonic_time()+500000;
 for(NSData *part in @[[NSData dataWithBytes:header length:13],body]){const uint8_t *bytes=part.bytes;NSUInteger offset=0;
  while(offset<part.length){if(g_get_monotonic_time()>=deadline){captureFail(@"stdout_timeout");return NO;}ssize_t amount=write(STDOUT_FILENO,bytes+offset,part.length-offset);
   if(amount>0){offset+=amount;continue;}if(amount<0&&errno==EINTR)continue;
   if(amount<0&&(errno==EAGAIN||errno==EWOULDBLOCK)){struct pollfd p={.fd=STDOUT_FILENO,.events=POLLOUT};int wait=(int)MAX(1,(deadline-g_get_monotonic_time())/1000);int rc=poll(&p,1,wait);if(rc<0&&errno==EINTR)continue;if(rc>0&&!(p.revents&(POLLHUP|POLLERR|POLLNVAL)))continue;}
   captureFail(@"stdout_closed_or_stalled");return NO;
  }
 }return YES;
#endif
}
static void emit(uint64_t e,NSDictionary *value){NSData *body=[NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:nil];if(body)record(1,e,body);else captureFail(@"json_serialization_failed");}
static void diagnostic(NSDictionary *value){NSMutableDictionary *event=[value mutableCopy];event[@"epoch"]=@(epoch);emit(epoch,event);}
static BOOL appendNAL(NSMutableData *au,const void *bytes,size_t count){static const uint8_t start[]={0,0,0,1};if(count==0||count>maxAU-13||au.length>maxAU-count-13){fail(@"Access unit exceeds limit or empty NAL");return NO;}[au appendBytes:start length:4];[au appendBytes:bytes length:count];return YES;}
static BOOL writeRecord(NSData *payload,uint64_t pts,BOOL key){uint64_t be=CFSwapInt64HostToBig(pts);uint8_t k=key;NSMutableData *body=[NSMutableData dataWithBytes:&be length:8];[body appendBytes:&k length:1];[body appendData:payload];return record(2,epoch,body);}
static void compressed(void *context,void *source,OSStatus status,VTEncodeInfoFlags flags,CMSampleBufferRef sample){@autoreleasepool{
 if(status!=noErr||!sample||!CMSampleBufferDataIsReady(sample)||(flags&kVTEncodeInfo_FrameDropped)){fail([NSString stringWithFormat:@"Encode callback failed status=%d flags=%u",(int)status,(unsigned)flags]);return;}
 CMFormatDescriptionRef format=CMSampleBufferGetFormatDescription(sample);
 if(CMFormatDescriptionGetMediaSubType(format)!=(useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264)){fail(@"Unexpected output codec");return;}
 if(hardwareProbe){probeOutput=YES;return;}
 if(!surfaceLive()||cachedGeneration!=generation){fail(@"Source generation ended during encode; frame discarded");return;}
 CFArrayRef attachments=CMSampleBufferGetSampleAttachmentsArray(sample,NO);BOOL key=YES;
 if(attachments&&CFArrayGetCount(attachments)){CFDictionaryRef dict=CFArrayGetValueAtIndex(attachments,0);key=!CFDictionaryContainsKey(dict,kCMSampleAttachmentKey_NotSync)||CFDictionaryGetValue(dict,kCMSampleAttachmentKey_NotSync)==kCFBooleanFalse;}
 const uint8_t *parameter=NULL;size_t parameterSize=0,parameterCount=0;int lengthSize=0;
 OSStatus result=useHEVC?CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,0,&parameter,&parameterSize,&parameterCount,&lengthSize):CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,0,&parameter,&parameterSize,&parameterCount,&lengthSize);
 if(result!=noErr||lengthSize!=4||parameterCount<(useHEVC?3:2)){fail(@"Missing parameter sets or unsupported NAL length");return;}
 if(!useHEVC&&(parameterSize<4||(parameter[0]&31)!=7||parameter[1]!=0x42||(parameter[2]&0x4f)!=0x40||!(h264Level==40?(parameter[3]==0x1f||parameter[3]==0x20||parameter[3]==0x28):parameter[3]==0x1f))){fail(h264Level==40?@"Actual H264 SPS does not match constrained Baseline up to 4.0 (profile42 / iop x1xx0000 / level 3.1, 3.2 or 4.0)":@"Actual H264 SPS does not match constrained Baseline3.1 (RFC6184 profile42 / iop x1xx0000)");return;}
 if(!useHEVC&&!actualH264Profile){actualH264Profile=[NSString stringWithFormat:@"%02x%02x%02x",parameter[1],parameter[2],parameter[3]];diagnostic(@{@"event":@"codec-parameters",@"codec":codecName,@"actual_h264_profile_level_id":actualH264Profile,@"h264_subprofile":@"Constrained Baseline",@"level":@(h264Level)});}
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
// One required-hardware session attempt; NO leaves no session behind.
static BOOL createEncoderWith(CVPixelBufferRef pixel,NSDictionary *spec,BOOL reportFailure){
    NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey:@(CVPixelBufferGetPixelFormatType(pixel)),(__bridge NSString *)kCVPixelBufferWidthKey:@(width),(__bridge NSString *)kCVPixelBufferHeightKey:@(height),(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
    OSStatus result=VTCompressionSessionCreate(NULL,(int32_t)width,(int32_t)height,useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,(__bridge CFDictionaryRef)spec,(__bridge CFDictionaryRef)attributes,NULL,compressed,NULL,&encoder);
    NSString *problem=nil;
    if(result!=noErr){encoder=NULL;problem=[NSString stringWithFormat:@"Required hardware %@ creation failed: %d",codecName,(int)result];}
    if(!problem){
    // The colour description is written into the stream: untagged, decoders (ffmpeg,
    // some browsers) assume BT.601 and shift every colour of the BT.709 conversion.
    // The rate hints only pick the level: at 60 the H.264 encoder picks one above
    // the Constrained Baseline 3.1 this helper guarantees, and above 60 HEVC's
    // passes 4.1. Frames still go at the Guest's rate.
    NSDictionary *properties=@{(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:useHEVC?@60:@30,(__bridge NSString *)kVTCompressionPropertyKey_ColorPrimaries:(__bridge NSString *)kCVImageBufferColorPrimaries_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_TransferFunction:(__bridge NSString *)kCVImageBufferTransferFunction_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_YCbCrMatrix:(__bridge NSString *)kCVImageBufferYCbCrMatrix_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@120,(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:(__bridge NSString *)(useHEVC?kVTProfileLevel_HEVC_Main_AutoLevel:kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
    for(NSString *key in properties){result=VTSessionSetProperty(encoder,(__bridge CFStringRef)key,(__bridge CFTypeRef)properties[key]);if(result!=noErr){problem=[NSString stringWithFormat:@"Encoder property rejected %@: %d",key,(int)result];break;}}
    }
    if(!problem){
    // Best effort: not every encoder generation exposes these latency knobs.
    VTSessionSetProperty(encoder,kVTCompressionPropertyKey_MaxFrameDelayCount,(__bridge CFTypeRef)@0);
    VTSessionSetProperty(encoder,kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,kCFBooleanTrue);
    // Quality only counts when set before the session is prepared; it is what lifts a photographic
    // keyframe from SSIM 0.94 to 0.99 (1920x1080: 300 KB to 1.1 MB, hence only for a still screen).
    if(creatingStill){VTSessionSetProperty(encoder,kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,kCFBooleanFalse);VTSessionSetProperty(encoder,kVTCompressionPropertyKey_Quality,(__bridge CFTypeRef)@0.9);}
    if(VTCompressionSessionPrepareToEncodeFrames(encoder)!=noErr)problem=@"Encoder preparation failed";
    }
    if(!problem){
    CFTypeRef usingHardware=NULL;
    result=VTSessionCopyProperty(encoder,kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,NULL,&usingHardware);
    hardwareVerified=result==noErr&&usingHardware&&CFGetTypeID(usingHardware)==CFBooleanGetTypeID()&&CFBooleanGetValue(usingHardware);
    if(usingHardware)CFRelease(usingHardware);
    if(!hardwareVerified)problem=@"Encoder did not confirm hardware acceleration; software fallback rejected";
    }
    if(problem){
     if(encoder){VTCompressionSessionInvalidate(encoder);CFRelease(encoder);encoder=NULL;}
     if(reportFailure)fail(problem);return NO;
    }
    if(creatingStill)return YES; // the still session is outside rate control
    // A set_scale rebuild is the same encoder instance to rate control: keep its target and sequence.
    if(carriedBitrate){if(VTSessionSetProperty(encoder,kVTCompressionPropertyKey_AverageBitRate,(__bridge CFNumberRef)carriedBitrate)!=noErr)rateFenced=YES;carriedBitrate=nil;}
    else{rateReset();rateSubmitted=0;}return YES;
}
static BOOL createEncoder(CVPixelBufferRef pixel){
    return createEncoderWith(pixel,@{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:@YES},YES);
}
static CVPixelBufferRef allocatePixel(NSUInteger w,NSUInteger h){
 NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{},(__bridge NSString *)kCVPixelBufferMetalCompatibilityKey:@YES};CVPixelBufferRef pixel=NULL;
 if(CVPixelBufferCreate(NULL,w,h,kCVPixelFormatType_32BGRA,(__bridge CFDictionaryRef)attributes,&pixel)!=kCVReturnSuccess)return NULL;return pixel;
}
#ifdef VEYNODE_NATIVE_DISPLAY
static BOOL finishCopyTiming(VNCopyKind kind,double begin,double commit,id<MTLCommandBuffer> command,BOOL success){
 double end=g_get_monotonic_time()/1e6;
 if(!copyTiming)copyTiming=[VNCopyTiming new];
 [copyTiming recordKind:kind cpuSeconds:end-begin commitWaitSeconds:isfinite(commit)?end-commit:NAN gpuStartSeconds:success?command.GPUStartTime:NAN gpuEndSeconds:success?command.GPUEndTime:NAN succeeded:success];
 return success;
}
#endif
// Nearest-sample resize of the source rectangle (rx,ry,rw,rh) into destination.
static BOOL copyPixels(IOSurfaceRef src,NSUInteger sw,NSUInteger sh,NSUInteger rx,NSUInteger ry,NSUInteger rw,NSUInteger rh,CVPixelBufferRef destination
#ifdef VEYNODE_NATIVE_DISPLAY
 ,VNCopyKind copyKind
#endif
 ){
 id<MTLCommandBuffer> command=nil;
#ifdef VEYNODE_NATIVE_DISPLAY
 double copyBegin=g_get_monotonic_time()/1e6,commitBegin=NAN;
#define COPY_RETURN(value) return finishCopyTiming(copyKind,copyBegin,commitBegin,command,(value))
#else
#define COPY_RETURN(value) return (value)
#endif
 if(!metal){metal=MTLCreateSystemDefaultDevice();queue=[metal newCommandQueue];NSError *error=nil;
  NSString *shader=@"#include <metal_stdlib>\nusing namespace metal;\nkernel void copy_frame(texture2d<float,access::read> src [[texture(0)]],texture2d<float,access::write> dst [[texture(1)]],constant uint4 &r [[buffer(0)]],uint2 p [[thread_position_in_grid]]){if(p.x>=dst.get_width()||p.y>=dst.get_height())return;uint sx=r.x+min(r.z-1,((2*p.x+1)*r.z)/(2*dst.get_width()));uint sy=r.y+min(r.w-1,((2*p.y+1)*r.w)/(2*dst.get_height()));dst.write(src.read(uint2(sx,sy)),p);}";
  id<MTLLibrary> library=[metal newLibraryWithSource:shader options:nil error:&error];if(library)pipeline=[metal newComputePipelineStateWithFunction:[library newFunctionWithName:@"copy_frame"] error:&error];if(!queue||!pipeline)COPY_RETURN(NO);
 }
 NSUInteger dw=CVPixelBufferGetWidth(destination),dh=CVPixelBufferGetHeight(destination);
 MTLTextureDescriptor *d=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:sw height:sh mipmapped:NO];d.storageMode=MTLStorageModeShared;d.usage=MTLTextureUsageShaderRead;
 id<MTLTexture> from=[metal newTextureWithDescriptor:d iosurface:src plane:0];d.width=dw;d.height=dh;d.usage=MTLTextureUsageShaderWrite;
 id<MTLTexture> to=[metal newTextureWithDescriptor:d iosurface:CVPixelBufferGetIOSurface(destination) plane:0];command=[queue commandBuffer];id<MTLComputeCommandEncoder> compute=[command computeCommandEncoder];
 if(!from||!to||!compute)COPY_RETURN(NO);[compute setComputePipelineState:pipeline];[compute setTexture:from atIndex:0];[compute setTexture:to atIndex:1];uint32_t rect[4]={(uint32_t)rx,(uint32_t)ry,(uint32_t)rw,(uint32_t)rh};[compute setBytes:rect length:sizeof rect atIndex:0];[compute dispatchThreads:MTLSizeMake(dw,dh,1) threadsPerThreadgroup:MTLSizeMake(16,16,1)];[compute endEncoding];
#ifdef VEYNODE_NATIVE_DISPLAY
 commitBegin=g_get_monotonic_time()/1e6;
#endif
 [command commit];[command waitUntilCompleted];COPY_RETURN(command.status==MTLCommandBufferStatusCompleted);
#undef COPY_RETURN
}
static BOOL cacheValid(void){return owned&&cachedGeneration==generation&&surfaceLive();}
// The live generation has a drawn frame: a copy of it exists or can be made now.
static BOOL frameReady(void){return drawnGeneration==generation&&generationFD>=0&&surfaceLive();}
static void clearCache(void){cachedGeneration=0;if(owned){
#ifdef VEYNODE_NATIVE_DISPLAY
 VeynodeDisplayRelease(owned);
#else
 CVPixelBufferRelease(owned);
#endif
 owned=NULL;}}
static NSDictionary *displayCapabilities(void){
#ifdef VEYNODE_NATIVE_DISPLAY
 NSMutableDictionary *caps=[VeynodeDisplayCapabilities() mutableCopy];caps[@"rate_control_version"]=@1;caps[@"rate_min_bps"]=@(rateMinimum);caps[@"rate_max_bps"]=@(rateMaximum);return caps;
#else
 return @{@"rate_control_version":@1,@"rate_min_bps":@(rateMinimum),@"rate_max_bps":@(rateMaximum),@"native_window":@NO,@"native_window_protocol":@1,@"display_only":@YES,@"input_enabled":@NO};
#endif
}
static void invalidateSession(VTCompressionSessionRef session){
 if(!session)return;
 if(VTCompressionSessionCompleteFrames(session,kCMTimeInvalid)!=noErr)fail(@"Encoder drain failed");
 VTCompressionSessionInvalidate(session);CFRelease(session);
}
// Back to the speed-first session; its next frame is a keyframe (the two reference chains differ).
static void leaveStill(void){
 if(!stillActive)return;
 VTCompressionSessionRef still=encoder;encoder=parkedEncoder;parkedEncoder=still;stillActive=NO;forceKeyframe=YES;
}
static void closeEncoder(void){
 leaveStill();invalidateSession(parkedEncoder);parkedEncoder=NULL;stillFailed=NO;lastChange=lastStill=0;
 if(encoder){if(VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid)!=noErr)fail(@"Encoder drain failed");VTCompressionSessionInvalidate(encoder);CFRelease(encoder);encoder=NULL;}
 if(scaled){CVPixelBufferRelease(scaled);scaled=NULL;}active=NO;forceKeyframe=NO;
}
// Even, within the default geometry, and at least 0.4x it per side.
static BOOL scaleFits(NSUInteger w,NSUInteger h,size_t fitWidth,size_t fitHeight){
 if(!w&&!h)return YES;return w&&h&&!(w&1)&&!(h&1)&&w<=fitWidth&&h<=fitHeight&&w*5>=fitWidth*2&&h*5>=fitHeight*2;
}
typedef struct{size_t x,y,w,h,fitW,fitH,outW,outH;}View;
// The crop clamped to the source and kept even (at least 2x2), its codec-fit size,
// and the output: the scale, never larger than that fit.
static BOOL resolveView(View *v){
 if(sourceWidth<2||sourceHeight<2)return NO;
 if(!cropWidth){v->x=v->y=0;v->w=sourceWidth;v->h=sourceHeight;}
 else{v->x=MIN(cropX,sourceWidth-2);v->y=MIN(cropY,sourceHeight-2);v->w=MIN(cropWidth,sourceWidth-v->x)&~(size_t)1;v->h=MIN(cropHeight,sourceHeight-v->y)&~(size_t)1;}
 if(!VNEncodedGeometryForLevel(v->w,v->h,useHEVC,h264Level,&v->fitW,&v->fitH))return NO;
 v->outW=scaleWidth?MIN(scaleWidth,v->fitW):v->fitW;v->outH=scaleHeight?MIN(scaleHeight,v->fitH):v->fitH;return YES;
}
static NSDictionary *viewFields(const View *v){
 return @{@"event":@"view",@"track":@0,@"x":@(v->x),@"y":@(v->y),@"w":@(v->w),@"h":@(v->h),@"src_w":@(sourceWidth),@"src_h":@(sourceHeight),@"out_w":@(v->outW),@"out_h":@(v->outH),@"default_w":@(v->fitW),@"default_h":@(v->fitH)};
}
static void sessionFailure(void){
 if(!failure)return;NSString *message=failure;closeEncoder();failure=nil;
 emit(epoch,@{@"event":@"session-error",@"epoch":@(epoch),@"error":@{@"code":@"encoder_failed",@"message":message}});
}
static View view;
// A guest that redraws a still screen (Windows Basic Display flushes its ramfb at
// ~27 Hz) would otherwise be encoded 27 times a second as identical frames. The
// previous source bytes are kept to recognise them; the 1 Hz idle refresh and
// keyframe requests still encode. Only rows a draw may have changed are compared:
// each draw's SPICE damage and, since the orientation of that rectangle against the
// IOSurface rows is not verified, its vertical mirror (whole rows: they are contiguous).
// Pending: drawn, not yet in a copy; copied: in `owned`, not yet compared. A new
// generation, a resize or a zero rectangle is every row. No draw since the last
// comparison means unchanged without reading a byte (the idle refresh, keyframes).
typedef struct{size_t top,bottom;} Rows;
static Rows pendingRows[2],copiedRows[2];static BOOL pendingAll=YES,copiedAll=YES;
static void addRows(Rows *r,size_t top,size_t bottom){if(top>=bottom)return;if(r->top>=r->bottom){r->top=top;r->bottom=bottom;return;}r->top=MIN(r->top,top);r->bottom=MAX(r->bottom,bottom);}
static void noteDamage(guint32 x,guint32 y,guint32 w,guint32 h){
 size_t rows=sourceHeight;if(!w||!h||!rows||y>=rows){pendingAll=YES;return;}
 size_t bottom=MIN((size_t)y+h,rows);addRows(&pendingRows[0],y,bottom);addRows(&pendingRows[1],rows-bottom,rows-y);
}
static void damageCopied(void){copiedAll|=pendingAll;for(int i=0;i<2;i++)addRows(&copiedRows[i],pendingRows[i].top,pendingRows[i].bottom);pendingAll=NO;memset(pendingRows,0,sizeof pendingRows);}
static uint8_t *previousFrame;static size_t previousSize;
static BOOL sourceUnchanged(CVPixelBufferRef pixel){
 size_t row=CVPixelBufferGetBytesPerRow(pixel),rows=CVPixelBufferGetHeight(pixel),size=row*rows;
 BOOL all=copiedAll||!previousFrame||previousSize!=size;
 if(!all&&copiedRows[0].top>=copiedRows[0].bottom&&copiedRows[1].top>=copiedRows[1].bottom)return YES;
 if(CVPixelBufferLockBaseAddress(pixel,kCVPixelBufferLock_ReadOnly)!=kCVReturnSuccess)return NO;
 const uint8_t *base=CVPixelBufferGetBaseAddress(pixel);BOOL same=NO;
 if(base&&size){
  BOOL fresh=!previousFrame||previousSize!=size;
  if(fresh){free(previousFrame);previousFrame=malloc(size);previousSize=previousFrame?size:0;}
  if(previousFrame){
   Rows bands[2]={all?(Rows){0,rows}:copiedRows[0],all?(Rows){0,0}:copiedRows[1]};same=!fresh;
   // Overlapping bands (a full-frame damage is its own mirror) are compared once.
   if(bands[1].top<bands[1].bottom&&bands[1].top<bands[0].bottom&&bands[0].top<bands[1].bottom){addRows(&bands[0],bands[1].top,bands[1].bottom);bands[1]=(Rows){0,0};}
   // Keep the copy in step: only the span from the first to the last differing row is copied.
   for(int i=0;i<2;i++){size_t top=bands[i].top,bottom=MIN(bands[i].bottom,rows);
    while(top<bottom&&!memcmp(previousFrame+top*row,base+top*row,row))top++;
    while(bottom>top&&!memcmp(previousFrame+(bottom-1)*row,base+(bottom-1)*row,row))bottom--;
    if(top<bottom){same=NO;memcpy(previousFrame+top*row,base+top*row,(bottom-top)*row);}
   }
   copiedAll=NO;memset(copiedRows,0,sizeof copiedRows);
  }
 }
 CVPixelBufferUnlockBaseAddress(pixel,kCVPixelBufferLock_ReadOnly);return same;
}
static void encodeLatest(gint64 now,BOOL idle){
 if(!active||!cacheValid())return;
 BOOL same=sourceUnchanged(owned)&&encoder; // always refreshes the kept copy
 if(same&&!idle&&!forceKeyframe){encodeDirty=NO;unchangedSkips++;return;}
 if(!same){lastChange=now;leaveStill();}
 if(!encoder){
  if(!resolveView(&view)){fail(@"Source geometry cannot fit even codec bounds");sessionFailure();return;}
  width=view.outW;height=view.outH;
  if(width!=sourceWidth||height!=sourceHeight||view.w!=sourceWidth||view.h!=sourceHeight){scaled=allocatePixel(width,height);if(!scaled){fail(@"Scale buffer unavailable");sessionFailure();return;}}
  if(!createEncoder(scaled?:owned)){sessionFailure();return;}
  emit(epoch,@{@"event":@"encoding",@"codec":codecName,@"hardware_verified":@(hardwareVerified),@"width":@(width),@"height":@(height),@"source_width":@(sourceWidth),@"source_height":@(sourceHeight),@"source_generation":@(generation),@"source_sequence":@(sequence)});
  emit(epoch,viewFields(&view));
 }
 CVPixelBufferRef pixel=owned;
 if(scaled){if(!copyPixels(CVPixelBufferGetIOSurface(owned),sourceWidth,sourceHeight,view.x,view.y,view.w,view.h,scaled
#ifdef VEYNODE_NATIVE_DISPLAY
 ,VNCopyEncoderResize
#endif
 )){fail(@"Owned scaling failed");sessionFailure();return;}resizeCopies++;pixel=scaled;}
 encodeDirty=NO;if(!firstPTS)firstPTS=now;lastEncode=now;CMTime pts=CMTimeMake(now-firstPTS,1000000);
 NSDictionary *options=forceKeyframe?@{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame:@YES}:nil;forceKeyframe=NO;
 CVPixelBufferRetain(pixel);OSStatus status=VTCompressionSessionEncodeFrame(encoder,pixel,pts,CMTimeMake(1,30),(__bridge CFDictionaryRef)options,NULL,NULL);
 if(status!=noErr)fail(@"Frame submission failed");else {rateSubmitted++;if(VTCompressionSessionCompleteFrames(encoder,pts)!=noErr)fail(@"Frame completion failed");}CVPixelBufferRelease(pixel);
 if(idle)idleFrames++;sessionFailure();
}
// A screen that stopped changing: one keyframe at full quality, from the second session.
// Not above about 4.2 Mpx (a photographic keyframe would near the 4 MiB access unit bound),
// not more than once per 3 s, and a failed creation leaves the speed-first session alone.
static void enterStill(gint64 now){
 if(stillActive||stillFailed||!encoder||!cacheValid())return;
 if(width*height>4200000){stillFailed=YES;return;}
 // The bitrate control is the network's say: a viewer it has backed off for would stall on a 1 MB keyframe.
 CFTypeRef raw=NULL;BOOL backedOff=VTSessionCopyProperty(encoder,kVTCompressionPropertyKey_AverageBitRate,NULL,&raw)==noErr&&raw&&[(__bridge_transfer NSNumber *)raw unsignedLongLongValue]<8000000;
 if(backedOff){lastStill=now;return;}
 VTCompressionSessionRef speedFirst=encoder;
 if(parkedEncoder){encoder=parkedEncoder;parkedEncoder=NULL;}
 else{
  encoder=NULL;creatingStill=YES;
  BOOL ok=createEncoderWith(scaled?:owned,@{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:@YES},NO);
  creatingStill=NO;
  if(!ok||!encoder){encoder=speedFirst;stillFailed=YES;return;}
 }
 parkedEncoder=speedFirst;stillActive=YES;forceKeyframe=YES;lastStill=now;
 encodeLatest(now,YES);
}
static void awaitSource(NSString *reason){
#ifdef VEYNODE_NATIVE_DISPLAY
 VeynodeDisplayInvalidate();capturePacing.initialized=false;
#endif
 cachedGeneration=0; // Invalidate publication before draining, retain storage until drain.
 sourceDirty=lazySequence=NO;pendingAll=copiedAll=YES;
 if(active&&encoder){fail(@"Source generation unavailable; start a fresh encoding epoch after replacement");sessionFailure();}
 clearCache();
#ifdef VEYNODE_NATIVE_DISPLAY
 if(![outputWriter drainFor:.6]){if(!fatal)fatal=@"stdout_cleanup_unconfirmed";[outputWriter invalidate];[outputWriter drainFor:.1];if(outputWriter.pending){fputs("stdout_cleanup_unconfirmed\n",stderr);_exit(1);}}
#endif
 if(generationFD>=0)close(generationFD);generationFD=-1;if(surface)CFRelease(surface);surface=NULL;
 if(!awaitingSource){emit(0,@{@"event":@"source-unavailable",@"reason":reason,@"source_generation":@(generation),@"frame_available":@NO});}
 awaitingSource=YES;
}
static void scanout(SpiceDisplayChannel *channel,GParamSpec *spec,gpointer context){
 awaitSource(@"scanout_replaced");generation++;
 const SpiceGlScanout *info=spice_display_channel_get_gl_scanout(channel);if(!info||info->fd<0)return;
 int available=0;struct pollfd p={.fd=info->fd,.events=POLLIN};
 // A retired descriptor may arrive before its replacement during EFI/DRM
 // mode transitions. It carries no usable source; never read/revive its ID.
 int polled=poll(&p,1,0);if(polled>=0&&(p.revents&POLLHUP))return;
 if(ioctl(info->fd,FIONREAD,&available)!=0||available!=4||polled!=1||!(p.revents&POLLIN)||(p.revents&(POLLERR|POLLNVAL))){captureFail(@"fresh_scanout_unavailable");return;}
 // y0top is accepted as is: UTM's IOSurface for such a scanout already holds
 // its rows in display order (seen live on a Debian 13 virgl console).
 IOSurfaceID ident=0;if(read(info->fd,&ident,4)!=4){captureFail(@"incomplete_scanout_id");return;}
 IOSurfaceRef observed=IOSurfaceLookup(ident);
 if(scanoutLogCount++<32){NSDictionary *event=@{@"event":@"scanout-observed",@"source_generation":@(generation),@"width":@(info->width),@"height":@(info->height),@"y0top":@(info->y0top),@"surface_available":@(observed!=NULL),@"surface_width":@(observed?IOSurfaceGetWidth(observed):0),@"surface_height":@(observed?IOSurfaceGetHeight(observed):0),@"pixel_format":@(observed?IOSurfaceGetPixelFormat(observed):0)};NSData *json=[NSJSONSerialization dataWithJSONObject:event options:NSJSONWritingSortedKeys error:nil];fwrite(json.bytes,1,json.length,stderr);fputc('\n',stderr);fflush(stderr);}
 if(!info->width||!info->height||info->width>3840||info->height>2160||info->width%2||info->height%2){emit(0,@{@"event":@"source-unavailable",@"reason":@"unsupported_scanout_geometry",@"source_generation":@(generation),@"width":@(info->width),@"height":@(info->height),@"y0top":@(info->y0top),@"frame_available":@NO});if(observed)CFRelease(observed);return;}
 surface=observed;
 if(!surface){struct pollfd latest={.fd=info->fd,.events=POLLIN};if(poll(&latest,1,0)>=0&&(latest.revents&POLLHUP))return;captureFail(@"invalid_scanout_surface");return;}
 if(IOSurfaceGetWidth(surface)!=info->width||IOSurfaceGetHeight(surface)!=info->height){captureFail(@"invalid_scanout_surface_geometry");return;}
 if(IOSurfaceGetPixelFormat(surface)!=kCVPixelFormatType_32BGRA){CFRelease(surface);surface=NULL;emit(0,@{@"event":@"source-unavailable",@"reason":@"unsupported_pixel_format",@"source_generation":@(generation),@"frame_available":@NO});return;}
 generationFD=dup(info->fd);if(generationFD<0){captureFail(@"scanout_fd_failed");return;}fcntl(generationFD,F_SETFD,FD_CLOEXEC);sourceWidth=info->width;sourceHeight=info->height;awaitingSource=NO;lastCapture=0;
 emit(0,@{@"event":@"source-generation",@"source_generation":@(generation),@"width":@(sourceWidth),@"height":@(sourceHeight)});
}
// Frames are wanted continuously only while a window shows them or a browser stream encodes them.
static BOOL captureWanted(void){
#ifdef VEYNODE_NATIVE_DISPLAY
 return active||VeynodeDisplayVisibleFPS()>0;
#else
 return active;
#endif
}
// Copy the live scanout into an owned buffer when pacing admits it (or at once,
// onDemand); a refused update stays dirty for the flush. Returns whether a new copy exists.
static BOOL captureSource(gint64 now,BOOL onDemand){
 if(!surfaceLive()){awaitSource(@"generation_retired_during_draw");return NO;}
#ifdef VEYNODE_NATIVE_DISPLAY
 unsigned windowFPS=VeynodeDisplayVisibleFPS();BOOL nativeVisible=windowFPS>0;
 // A browser stream gets its own rate even while the window is hidden; both together get the faster.
 unsigned fps=MAX(windowFPS,active?streamFPS:0);captureFPS=fps?fps:VN_BACKGROUND_FPS;
 if(!onDemand&&!VNFramePacingAdmit(&capturePacing,now,captureFPS)){capturePacingDrops++;sourceDirty=YES;return NO;}
 CVPixelBufferRef next=VeynodeDisplayAcquire(sourceWidth,sourceHeight);
 // Pool exhaustion drops this copy without blocking ACK; the flush retries.
 if(!next){capturePoolDrops++;sourceDirty=YES;return NO;}
 if(!copyPixels(surface,sourceWidth,sourceHeight,0,0,sourceWidth,sourceHeight,next,VNCopySource)){VeynodeDisplayRelease(next);captureFail(@"source_copy_failed");return NO;}
 if(!surfaceLive()){VeynodeDisplayRelease(next);awaitSource(@"generation_retired_during_copy");return NO;}
 if(!lazySequence)sequence++;lazySequence=NO;
 clearCache();owned=next;sourceCopies++;cachedGeneration=generation;capturedMono=g_get_monotonic_time();capturedWall=g_get_real_time();lastCapture=now;if(nativeVisible)visibleSourceCopies++;else backgroundSourceCopies++;VeynodeDisplayPublish(owned,generation,sequence,capturedMono);
#else
 captureFPS=streamFPS;if(!onDemand&&lastCapture&&now-lastCapture<encodeInterval()){sourceDirty=YES;return NO;}
 cachedGeneration=0;if(!owned)owned=allocatePixel(sourceWidth,sourceHeight);
 if(!owned||!copyPixels(surface,sourceWidth,sourceHeight,0,0,sourceWidth,sourceHeight,owned)){clearCache();captureFail(@"source_copy_failed");return NO;}
 if(!surfaceLive()){awaitSource(@"generation_retired_during_copy");return NO;}
 if(!lazySequence)sequence++;lazySequence=NO;
 sourceCopies++;cachedGeneration=generation;capturedMono=g_get_monotonic_time();capturedWall=g_get_real_time();lastCapture=now;
#endif
 if(onDemand)onDemandCopies++;
 damageCopied();sourceDirty=NO;encodeDirty=YES;return YES;
}
// The latest drawn scanout as `owned`, copied now when no copy has it yet.
static BOOL refreshSource(void){
 if(frameReady()&&(sourceDirty||!cacheValid()))captureSource(g_get_monotonic_time(),YES);
 return cacheValid();
}
// Requests that need the latest frame (screenshot, `start`, `show_window`) while the
// newest draw was ACKed uncopied. After that ACK the Guest may already be rendering
// the next frame into the scanout, so a copy now could be torn. A Guest that drew
// within drawQuietUS is drawing: the next draw copies before its ACK, as a watched
// draw does, and completes them. A screen quiet that long has nothing rendering: copy
// now. The quiet timer (one shot, armed only while requests wait) is the fallback when
// no draw comes, so a request waits at most drawQuietUS.
static const gint64 drawQuietUS=50000;
static NSMutableArray *captureWaiters;static guint captureTimer;
static void finishCaptureWaiters(void){
 if(captureTimer){g_source_remove(captureTimer);captureTimer=0;}
 NSArray *waiting=captureWaiters;captureWaiters=nil;for(void (^done)(void) in waiting)done();
}
static gboolean captureQuiet(gpointer data){@autoreleasepool{
 captureTimer=0;if(!closing&&!fatal)refreshSource();finishCaptureWaiters();return G_SOURCE_REMOVE;
}}
static void whenSourceCurrent(void (^done)(void)){
 gint64 now=g_get_monotonic_time();
 if(!(frameReady()&&(sourceDirty||!cacheValid()))){done();return;}
 if(now-lastDrawMono>=drawQuietUS){refreshSource();done();return;}
 if(!captureWaiters)captureWaiters=[NSMutableArray new];[captureWaiters addObject:[done copy]];
 if(!captureTimer)captureTimer=g_timeout_add((guint)((lastDrawMono+drawQuietUS-now+999)/1000),captureQuiet,NULL);
}
static void encodeIfDue(gint64 now){
 if(!fatal&&active&&cacheValid()&&encodeDirty&&(captureFPS<=streamFPS||VNFramePacingAdmit(&encodePacing,now,streamFPS)))encodeLatest(now,NO);
}
static gboolean flush(gpointer data);
// When the flush has work, the earliest time it can do it, else -1: a refused copy
// while frames are wanted, or a copy waiting for its encode slot.
static gint64 flushDeadline(void){
 if(closing||fatal)return -1;gint64 due=-1;
 if(sourceDirty&&generationFD>=0&&captureWanted()){
#ifdef VEYNODE_NATIVE_DISPLAY
  due=VNFramePacingDue(&capturePacing,captureFPS);
#else
  due=lastCapture?lastCapture+encodeInterval():0;
#endif
 }
 if(active&&encodeDirty&&cacheValid()&&captureFPS>streamFPS){gint64 slot=VNFramePacingDue(&encodePacing,streamFPS);due=due<0?slot:MIN(due,slot);}
 return due;
}
static void scheduleFlush(gint64 now){
 gint64 due=flushDeadline();
 if(due<0){if(flushSource){g_source_remove(flushSource);flushSource=0;}return;}
 if(flushSource&&flushDue<=due)return;
 if(flushSource)g_source_remove(flushSource);
 guint ms=due>now?(guint)((due-now+999)/1000):0;flushDue=now+(gint64)ms*1000;flushSource=g_timeout_add(ms,flush,NULL);
}
static void draw(SpiceDisplayChannel *channel,guint32 x,guint32 y,guint32 w,guint32 h,gpointer context){@autoreleasepool{
 received++;gint64 now=g_get_monotonic_time();noteDamage(x,y,w,h);lastDrawMono=now;
 if(!surfaceLive())awaitSource(@"generation_retired_during_draw");
 else{
  // This draw is newer than any number a deferred draw took: a copy of it takes its own.
  drawnGeneration=generation;lazySequence=NO;BOOL wanted=captureWanted(),waiting=captureWaiters.count>0;
  if(!((wanted||waiting)&&captureSource(now,waiting))&&generationFD>=0){sequence++;lazySequence=YES;sourceDirty=YES;if(!wanted)deferredDraws++;}
 }
 // Producer ACK is after any source copy, before encoding our owned buffer.
 ACK_DRAW(channel);
 if(captureWaiters.count&&cacheValid()&&!sourceDirty)finishCaptureWaiters();
 encodeIfDue(now);scheduleFlush(now);
}}
static gboolean flush(gpointer data){@autoreleasepool{
 flushSource=0;
 if(closing||fatal)return G_SOURCE_REMOVE;
 gint64 now=g_get_monotonic_time();
 if(sourceDirty&&generationFD>=0&&captureWanted())captureSource(now,NO);
 encodeIfDue(now);scheduleFlush(now);
 return G_SOURCE_REMOVE;
}}
// Guest audio (Config::audio): SPICE playback PCM becomes kind-3 records
// (u32 BE rate, u8 channels, s16le PCM) while a broker client listens, and
// broker `audio_in` lines (48 kHz mono s16le, base64) feed SPICE record.
// Audio is best effort: a backed-up stdout drops audio, never fails capture.
static BOOL audioListening;static int playbackRate,playbackChannels;
static SpiceRecordChannel *recordChannel;static int recordRate,recordChannels;
static void playbackStart(SpicePlaybackChannel *channel,gint format,gint channels,gint rate,gpointer data){playbackRate=format==SPICE_AUDIO_FMT_S16?rate:0;playbackChannels=channels;}
static void playbackStop(SpicePlaybackChannel *channel,gpointer data){playbackRate=0;}
static void playbackData(SpicePlaybackChannel *channel,gpointer pcm,gint size,gpointer data){
 if(!playbackRate||playbackChannels<1||playbackChannels>2||size<=0||size>65536)return;
#ifdef VEYNODE_NATIVE_DISPLAY
 VeynodeAudioPlay(pcm,(NSUInteger)size,playbackRate,playbackChannels);
#endif
 if(!audioListening||closing||fatal)return;
#ifdef VEYNODE_NATIVE_DISPLAY
 if(!outputWriter||outputWriter.pending>4)return;
#else
 struct pollfd p={.fd=STDOUT_FILENO,.events=POLLOUT};if(poll(&p,1,0)!=1||(p.revents&(POLLHUP|POLLERR|POLLNVAL)))return;
#endif
 uint32_t rate=CFSwapInt32HostToBig((uint32_t)playbackRate);uint8_t count=(uint8_t)playbackChannels;
 NSMutableData *body=[NSMutableData dataWithBytes:&rate length:4];[body appendBytes:&count length:1];[body appendBytes:pcm length:(NSUInteger)size];record(3,0,body);
}
static void recordStart(SpiceRecordChannel *channel,gint format,gint channels,gint rate,gpointer data){recordRate=format==SPICE_AUDIO_FMT_S16?rate:0;recordChannels=channels;}
static void recordStop(SpiceRecordChannel *channel,gpointer data){recordRate=0;}
// Mono 48 kHz in; the record stream's own rate and channel count out (linear resampling).
static void audioIn(NSData *mono){
 if(!recordChannel||!recordRate||recordChannels<1||recordChannels>2||mono.length<2)return;
 const int16_t *in=mono.bytes;NSUInteger frames=mono.length/2,outFrames=(NSUInteger)((uint64_t)frames*(uint64_t)recordRate/48000);if(!outFrames)return;
 NSMutableData *out=[NSMutableData dataWithLength:outFrames*recordChannels*2];int16_t *o=out.mutableBytes;
 for(NSUInteger i=0;i<outFrames;i++){double at=(double)i*48000/recordRate;NSUInteger a=MIN((NSUInteger)at,frames-1),b=MIN(a+1,frames-1);double t=at-(double)a;int16_t v=(int16_t)lrint(in[a]*(1-t)+in[b]*t);for(int c=0;c<recordChannels;c++)o[i*recordChannels+c]=v;}
 spice_record_channel_send_data(recordChannel,o,out.length,(guint32)(g_get_monotonic_time()/1000));
}
// Guest clipboard (the Guest's spice-vdagent): a Guest copy becomes the event
// {"event":"clipboard","text"}; viewer text arrives as broker `clipboard_chunk`
// lines (base64 parts, the last with "last":true) and is offered to the Guest.
// UTF-8 text only, at most clipboardMax bytes.
static const NSUInteger clipboardMax=200*1024;
static SpiceMainChannel *mainChannel;static NSData *hostClipboard;static NSMutableData *clipboardParts;static BOOL guestClipboard;
static void clipboardEvent(NSDictionary *value){NSMutableDictionary *event=[value mutableCopy];event[@"event"]=@"clipboard";emit(0,event);
#ifdef VEYNODE_NATIVE_DISPLAY
 if(value[@"text"])VeynodeClipboardFromGuest(value[@"text"]);
#endif
}
static gboolean clipboardGrab(SpiceMainChannel *channel,guint selection,guint32 *types,guint32 ntypes,gpointer data){
 if(selection!=VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD)return FALSE;
 guestClipboard=YES;hostClipboard=nil;
 for(guint32 i=0;i<ntypes;i++)if(types[i]==VD_AGENT_CLIPBOARD_UTF8_TEXT){spice_main_channel_clipboard_selection_request(channel,selection,VD_AGENT_CLIPBOARD_UTF8_TEXT);return TRUE;}
 return TRUE;
}
static void clipboardData(SpiceMainChannel *channel,guint selection,guint type,const guchar *bytes,guint size,gpointer data){
 if(selection!=VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD||type!=VD_AGENT_CLIPBOARD_UTF8_TEXT)return;
 if(size>clipboardMax){clipboardEvent(@{@"error":@"the Guest's clipboard is over 200 KiB"});return;}
 NSString *text=[[NSString alloc] initWithBytes:bytes length:size encoding:NSUTF8StringEncoding];
 // A Windows-style agent sends CRLF; the viewer's side is Unix text.
 if(text&&spice_main_channel_agent_test_capability(channel,VD_AGENT_CAP_GUEST_LINEEND_CRLF))text=[text stringByReplacingOccurrencesOfString:@"\r\n" withString:@"\n"];
 clipboardEvent(text?@{@"text":text}:@{@"error":@"the Guest's clipboard is not UTF-8 text"});
}
static gboolean clipboardRequest(SpiceMainChannel *channel,guint selection,guint type,gpointer data){
 if(selection!=VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD||type!=VD_AGENT_CLIPBOARD_UTF8_TEXT||!hostClipboard)return FALSE;
 spice_main_channel_clipboard_selection_notify(channel,selection,type,hostClipboard.bytes,hostClipboard.length);return TRUE;
}
static void clipboardRelease(SpiceMainChannel *channel,guint selection,gpointer data){if(selection==VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD)guestClipboard=NO;}
static void clipboardSet(NSData *text){
 if(text.length>clipboardMax||![[NSString alloc] initWithData:text encoding:NSUTF8StringEncoding]){clipboardEvent(@{@"error":@"clipboard text must be UTF-8 of at most 200 KiB"});return;}
 gboolean agent=FALSE;if(mainChannel)g_object_get(mainChannel,"agent-connected",&agent,NULL);
 if(!agent){clipboardEvent(@{@"error":@"the Guest runs no clipboard agent (spice-vdagent)"});return;}
 hostClipboard=text;guestClipboard=NO;guint32 type=VD_AGENT_CLIPBOARD_UTF8_TEXT;
 spice_main_channel_clipboard_selection_grab(mainChannel,VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,&type,1);
}
// Guest pointer shape (the SPICE cursor channel): each new shape becomes the
// event {"event":"cursor","width","height","hot_x","hot_y","rgba"} (base64 of
// straight-alpha RGBA rows) and a hidden pointer {"event":"cursor","hidden":true}.
// `cursor_get` sends the current one again, for a viewer that starts watching.
static NSDictionary *lastCursor;
static void cursorEvent(NSDictionary *value){lastCursor=value;NSMutableDictionary *event=[value mutableCopy];event[@"event"]=@"cursor";emit(0,event);}
static void cursorSet(SpiceCursorChannel *channel,gint width,gint height,gint hotX,gint hotY,gpointer data,gpointer user){
 if(width<=0||height<=0||width>512||height>512||!data)return;
 // spice-gtk hands over native-endian ARGB32 (B, G, R, A in memory here).
 NSMutableData *rgba=[NSMutableData dataWithLength:(NSUInteger)width*(NSUInteger)height*4];const uint8_t *in=data;uint8_t *out=rgba.mutableBytes;
 for(NSUInteger i=0;i<(NSUInteger)width*(NSUInteger)height;i++){out[i*4]=in[i*4+2];out[i*4+1]=in[i*4+1];out[i*4+2]=in[i*4];out[i*4+3]=in[i*4+3];}
 cursorEvent(@{@"width":@(width),@"height":@(height),@"hot_x":@(MIN(MAX(hotX,0),width-1)),@"hot_y":@(MIN(MAX(hotY,0),height-1)),@"rgba":[rgba base64EncodedStringWithOptions:0]});
}
static void cursorHide(SpiceCursorChannel *channel,gpointer user){cursorEvent(@{@"hidden":@YES});}
static void cursorReset(SpiceCursorChannel *channel,gpointer user){lastCursor=nil;}
// The viewer's size as the Guest's one monitor (SPICE monitor config). The
// Guest's spice-vdagent applies it; without one there is nothing to tell.
static NSString *monitorConfig(NSNumber *width,NSNumber *height){
 if(![width isKindOfClass:NSNumber.class]||![height isKindOfClass:NSNumber.class])return @"invalid_size";
 int w=width.intValue,h=height.intValue;if(w<320||h<200||w>8192||h>8192)return @"invalid_size";
 gboolean agent=FALSE;if(mainChannel)g_object_get(mainChannel,"agent-connected",&agent,NULL);
 if(!agent)return @"no_agent";
 spice_main_channel_update_display_enabled(mainChannel,0,TRUE,FALSE);
 spice_main_channel_update_display(mainChannel,0,0,0,w&~1,h&~1,FALSE);
 return spice_main_channel_send_monitor_config(mainChannel)?nil:@"monitor_config_refused";
}
static void clipboardGet(void){
 if(guestClipboard&&mainChannel)spice_main_channel_clipboard_selection_request(mainChannel,VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,VD_AGENT_CLIPBOARD_UTF8_TEXT);
 else if(hostClipboard)clipboardEvent(@{@"text":[[NSString alloc] initWithData:hostClipboard encoding:NSUTF8StringEncoding]});
 else clipboardEvent(@{@"text":@""});
}
#ifdef VEYNODE_NATIVE_DISPLAY
// The native window's side runs on AppKit's thread; SPICE calls belong to the GLib loop's.
static gboolean clipboardSetOnLoop(gpointer data){clipboardSet(CFBridgingRelease(data));return G_SOURCE_REMOVE;}
static gboolean audioInOnLoop(gpointer data){audioIn(CFBridgingRelease(data));return G_SOURCE_REMOVE;}
static gboolean clipboardGetOnLoop(gpointer data){clipboardGet();return G_SOURCE_REMOVE;}
void VeynodeClipboardGet(void){g_main_context_invoke(NULL,clipboardGetOnLoop,NULL);}
void VeynodeClipboardSet(NSString *text){g_main_context_invoke(NULL,clipboardSetOnLoop,(gpointer)CFBridgingRetain([text dataUsingEncoding:NSUTF8StringEncoding]));}
// The native window's microphone, already mono 48 kHz s16le.
void VeynodeAudioCapture(NSData *mono){g_main_context_invoke(NULL,audioInOnLoop,(gpointer)CFBridgingRetain(mono));}
#endif
static void channelEvent(SpiceChannel *channel,SpiceChannelEvent event,gpointer data){
 if(closing)return;
 if(channelLogCount++<32){int type=0,ident=0;g_object_get(channel,"channel-type",&type,"channel-id",&ident,NULL);NSData *log=[NSJSONSerialization dataWithJSONObject:@{@"event":@"channel-event",@"channel_type":@(type),@"channel_id":@(ident),@"event_code":@(event)} options:NSJSONWritingSortedKeys error:nil];fwrite(log.bytes,1,log.length,stderr);fputc('\n',stderr);fflush(stderr);}
 if(event==SPICE_CHANNEL_OPENED){if(SPICE_IS_MAIN_CHANNEL(channel))mainOpen=YES;if(SPICE_IS_DISPLAY_CHANNEL(channel))displayOpen=YES;
  if(mainOpen&&displayOpen&&!readySent){readySent=YES;emit(0,@{@"event":@"ready",@"protocol":@1,@"capture_connected":@YES,@"frame_available":@(frameReady())});}}
 else if(event==SPICE_CHANNEL_CLOSED||event>=SPICE_CHANNEL_ERROR_CONNECT){
#ifdef VEYNODE_NATIVE_DISPLAY
 VeynodeDisplayInvalidate();
#endif
 clearCache();captureFail(@"spice_disconnected");}
}
static void newChannel(SpiceSession *session,SpiceChannel *channel,gpointer data){
 if(SPICE_IS_MAIN_CHANNEL(channel)){mainChannel=SPICE_MAIN_CHANNEL(channel);g_signal_connect(channel,"main-clipboard-selection-grab",G_CALLBACK(clipboardGrab),NULL);g_signal_connect(channel,"main-clipboard-selection-request",G_CALLBACK(clipboardRequest),NULL);g_signal_connect(channel,"main-clipboard-selection-release",G_CALLBACK(clipboardRelease),NULL);g_signal_connect(channel,"main-clipboard-selection",G_CALLBACK(clipboardData),NULL);}
#ifdef VEYNODE_NATIVE_DISPLAY
 if(SPICE_IS_PLAYBACK_CHANNEL(channel))VeynodeAudioAvailable();
#endif
 if(SPICE_IS_PLAYBACK_CHANNEL(channel)){g_signal_connect(channel,"playback-start",G_CALLBACK(playbackStart),NULL);g_signal_connect(channel,"playback-stop",G_CALLBACK(playbackStop),NULL);g_signal_connect(channel,"playback-data",G_CALLBACK(playbackData),NULL);spice_channel_connect(channel);return;}
 if(SPICE_IS_CURSOR_CHANNEL(channel)){g_signal_connect(channel,"cursor-set",G_CALLBACK(cursorSet),NULL);g_signal_connect(channel,"cursor-hide",G_CALLBACK(cursorHide),NULL);g_signal_connect(channel,"cursor-reset",G_CALLBACK(cursorReset),NULL);spice_channel_connect(channel);return;}
 if(SPICE_IS_RECORD_CHANNEL(channel)){recordChannel=SPICE_RECORD_CHANNEL(channel);g_signal_connect(channel,"record-start",G_CALLBACK(recordStart),NULL);g_signal_connect(channel,"record-stop",G_CALLBACK(recordStop),NULL);spice_channel_connect(channel);return;}
 if(SPICE_IS_DISPLAY_CHANNEL(channel)){g_signal_connect(channel,"notify::gl-scanout",G_CALLBACK(scanout),NULL);g_signal_connect(channel,"gl-draw",G_CALLBACK(draw),NULL);g_signal_connect(channel,"channel-event",G_CALLBACK(channelEvent),NULL);spice_channel_connect(channel);}
 else if(SPICE_IS_MAIN_CHANNEL(channel))g_signal_connect(channel,"channel-event",G_CALLBACK(channelEvent),NULL);
}
static void reply(id ident,uint64_t e,NSDictionary *result){emit(e,@{@"id":ident,@"result":result});}
static void reject(id ident,uint64_t e,NSString *code){emit(e,@{@"id":ident?:[NSNull null],@"error":@{@"code":code,@"message":code}});}
#ifdef VEYNODE_NATIVE_DISPLAY
static BOOL displayCommandPending;
static void windowCommand(NSString *method,NSString *ident,NSDictionary *frame,NSNumber *diagnosticID,NSDictionary *receiptFields){
 if(displayCommandPending){reject(ident,0,@"native_window_busy");return;}displayCommandPending=YES;
 GMainContext *context=g_main_context_ref(g_main_loop_get_context(loop));
 VeynodeDisplayCommand(method,frame,diagnosticID,receiptFields,^(NSDictionary *result,NSString *error){
  void (^finish)(void)=[^{displayCommandPending=NO;if(!closing&&!fatal){if(error)reject(ident,0,error);else{if([method isEqual:@"show_window"])whenSourceCurrent(^{if(!closing&&!fatal&&cacheValid())VeynodeDisplayPublish(owned,generation,sequence,capturedMono);});if([method isEqual:@"validate_displayed_frame"]){reply(ident,0,result);return;}NSMutableDictionary *status=[result mutableCopy];if(!copyTiming)copyTiming=[VNCopyTiming new];status[@"copy_timing"]=[copyTiming statistics];status[@"screenshot_timing"]=lastScreenshotTiming?:[NSNull null];BOOL ready=frameReady();status[@"source_width"]=@(ready?sourceWidth:0);status[@"source_height"]=@(ready?sourceHeight:0);status[@"source_frame_available"]=@(ready);status[@"encoder_width"]=@(encoder?width:0);status[@"encoder_height"]=@(encoder?height:0);status[@"source_draw_events"]=@(received);status[@"source_copies"]=@(sourceCopies);status[@"capture_pacing_drops"]=@(capturePacingDrops);status[@"capture_pool_drops"]=@(capturePoolDrops);status[@"visible_source_copies"]=@(visibleSourceCopies);status[@"background_source_copies"]=@(backgroundSourceCopies);status[@"on_demand_source_copies"]=@(onDemandCopies);status[@"source_deferred_draws"]=@(deferredDraws);status[@"encoder_output_frames_current_epoch"]=@(encoded);status[@"encoder_idle_frames_current_epoch"]=@(idleFrames);status[@"encoder_unchanged_skips_current_epoch"]=@(unchangedSkips);status[@"encoder_active"]=@(active);status[@"sampled_monotonic_us"]=@(g_get_monotonic_time());reply(ident,0,status);}}} copy];
  // A C trampoline owns the retained block; no stdout emission from AppKit.
  extern gboolean VeynodeConsoleInvoke(gpointer);
  extern void VeynodeConsoleInvokeFree(gpointer);
  // invoke_full can execute inline if the caller can acquire an idle context.
  // Attaching a source always defers work to the sole GLib-loop owner instead.
  GSource *source=g_idle_source_new();
  g_source_set_callback(source,VeynodeConsoleInvoke,(__bridge_retained void *)finish,VeynodeConsoleInvokeFree);
  g_source_attach(source,context);g_source_unref(source);g_main_context_unref(context);
 });
}
gboolean VeynodeConsoleInvoke(gpointer data){void (^block)(void)=(__bridge id)data;block();return G_SOURCE_REMOVE;}
void VeynodeConsoleInvokeFree(gpointer data){CFBridgingRelease(data);}
#endif
#ifdef VEYNODE_NATIVE_DISPLAY
static gboolean screenshotTick(gpointer unused);
static void armScreenshotTick(void){if(!screenshotSource)screenshotSource=g_timeout_add(10,screenshotTick,NULL);}
static BOOL screenshotWaiting;
static void admitScreenshot(NSString *ident){
 if(!mainOpen||!displayOpen||!cacheValid()){emit(0,@{@"id":ident,@"error":@{@"code":@"frame_unavailable",@"message":@"frame_unavailable",@"diagnostic":VNScreenshotFailure(@"cache_invalid_before_copy",nil)}});return;}
 CVPixelBufferRef pixel=owned;
 if(!VeynodeDisplayRetain(pixel)){reject(ident,0,@"frame_unavailable");return;}
 screenshotID=[ident copy];screenshotMetadata=@{@"source_generation":@(generation),@"source_sequence":@(sequence),@"captured_at_us":@(capturedWall),@"captured_monotonic_us":@(capturedMono),@"width":@(sourceWidth),@"height":@(sourceHeight),@"coordinate_space":@"guest-physical-pixels",@"mime_type":@"image/png"};
 screenshotTiming=[@{@"version":@1,@"timing_clock":@"CLOCK_MONOTONIC",@"source_generation":@(generation),@"source_sequence":@(sequence),@"admitted_monotonic_us":@(nativeClock()*1e6),@"success":@NO} mutableCopy];
 screenshotDeadline=nativeClock()+2.0;screenshotDelivered=NO;
 screenshotJob=[[VNAsyncScreenshot alloc] initWithPixel:pixel metadata:screenshotMetadata requestID:screenshotID release:^{VeynodeDisplayRelease(pixel);}];
 armScreenshotTick();
}
static void beginScreenshot(NSString *ident){
 if(screenshotJob||screenshotWrite||screenshotWaiting){reject(ident,0,@"screenshot_busy");return;}
 screenshotWaiting=YES;NSString *request=[ident copy];
 // A shutdown or teardown while it waited answers for it (see the screenshot tick).
 whenSourceCurrent(^{screenshotWaiting=NO;if(!closing&&!fatal&&!shutdownID)admitScreenshot(request);});
}
static gboolean screenshotTick(gpointer unused){
 if(outputWriter.failed){captureFail(@"stdout_closed_or_stalled");return G_SOURCE_CONTINUE;}
 if(screenshotJob&&!screenshotDelivered){
  NSDictionary *result=[screenshotJob resultIfFinished];
  if(result){
   [screenshotTiming addEntriesFromDictionary:result[@"timing"]];screenshotTiming[@"glib_completion_monotonic_us"]=@(nativeClock()*1e6);
   // A delayed owner tick cannot accept a worker that finished after admission's
   // absolute deadline. Finishing before it remains valid even if this tick is late.
   if([result[@"timing"][@"worker_completed_monotonic_us"] doubleValue]>screenshotDeadline*1e6){[screenshotJob invalidate];captureFail(@"screenshot_worker_deadline");}
   BOOL valid=!closing&&!fatal&&!shutdownID&&mainOpen&&displayOpen&&cacheValid()&&generation==[screenshotMetadata[@"source_generation"] unsignedLongLongValue]&&sourceWidth==[screenshotMetadata[@"width"] unsignedIntegerValue]&&sourceHeight==[screenshotMetadata[@"height"] unsignedIntegerValue];
   if(!closing&&!fatal&&!shutdownID){
    NSData *body=nil;
    if(valid&&result[@"error"]==[NSNull null]){body=result[@"body"];screenshotTiming[@"success"]=@YES;}
    else{NSString *code=valid?@"screenshot_unavailable_or_too_large":@"frame_unavailable";body=[NSJSONSerialization dataWithJSONObject:@{@"id":screenshotID,@"error":@{@"code":code,@"message":code,@"diagnostic":valid?result[@"diagnostic"]:VNScreenshotFailure(@"cache_invalid_after_encode",nil)}} options:0 error:nil];}
    screenshotTiming[@"output_admitted_monotonic_us"]=@(nativeClock()*1e6);
    screenshotWrite=[outputWriter enqueueKind:1 epoch:0 body:body];
    if(!screenshotWrite)captureFail(@"stdout_closed_or_stalled");
   }
   screenshotDelivered=YES;
  }else if(nativeClock()>=screenshotDeadline){[screenshotJob invalidate];captureFail(@"screenshot_worker_deadline");}
 }
 if(screenshotJob&&screenshotDelivered&&(!screenshotWrite||[[screenshotWrite status][@"finished"] boolValue])){
  if(screenshotWrite){NSDictionary *s=[screenshotWrite status];screenshotTiming[@"output_queue_us"]=s[@"queue_us"];screenshotTiming[@"output_write_us"]=s[@"write_us"];screenshotTiming[@"output_completed_monotonic_us"]=s[@"completed_monotonic_us"];if(![s[@"success"] boolValue])screenshotTiming[@"success"]=@NO;}
  lastScreenshotTiming=[screenshotTiming copy];screenshotJob=nil;screenshotWrite=nil;screenshotID=nil;screenshotMetadata=nil;screenshotTiming=nil;
 }
 if(shutdownID){
  if(nativeClock()>=shutdownDeadline){captureFail(@"screenshot_shutdown_deadline");return G_SOURCE_CONTINUE;}
  if(!screenshotJob&&!screenshotWaiting&&!shutdownReplied){reply(shutdownID,0,@{@"state":@"stopped"});shutdownReplied=YES;}
  if(shutdownReplied&&outputWriter.pending==0){closing=YES;g_main_loop_quit(loop);}
 }
 if(!screenshotJob&&!shutdownID){screenshotSource=0;return G_SOURCE_REMOVE;}
 return G_SOURCE_CONTINUE;
}
#endif
#ifndef VEYNODE_NATIVE_DISPLAY
static NSDictionary *screenshot(NSDictionary **diagnostic){
 *diagnostic=nil;
 if(!cacheValid()){*diagnostic=VNScreenshotFailure(@"cache_invalid_before_copy",nil);return nil;}
 CVPixelBufferLockBaseAddress(owned,kCVPixelBufferLock_ReadOnly);
 size_t row=CVPixelBufferGetBytesPerRow(owned);NSData *pixels=[NSData dataWithBytes:CVPixelBufferGetBaseAddress(owned) length:row*sourceHeight];
 CVPixelBufferUnlockBaseAddress(owned,kCVPixelBufferLock_ReadOnly);
 CGDataProviderRef provider=CGDataProviderCreateWithCFData((__bridge CFDataRef)pixels);CGColorSpaceRef color=CGColorSpaceCreateDeviceRGB();
 CGImageRef image=CGImageCreate(sourceWidth,sourceHeight,8,32,row,color,kCGBitmapByteOrder32Little|kCGImageAlphaNoneSkipFirst,provider,NULL,false,kCGRenderingIntentDefault);
 NSMutableData *png=[NSMutableData new];CGImageDestinationRef destination=CGImageDestinationCreateWithData((__bridge CFMutableDataRef)png,CFSTR("public.png"),1,NULL);BOOL okay=NO;
 if(image&&destination){CGImageDestinationAddImage(destination,image,NULL);okay=CGImageDestinationFinalize(destination);}if(destination)CFRelease(destination);if(image)CGImageRelease(image);CGColorSpaceRelease(color);CGDataProviderRelease(provider);
 *diagnostic=VNScreenshotEncodingFailure(okay,png.length,cacheValid());
 if(*diagnostic)return nil;
 return @{@"source_generation":@(generation),@"source_sequence":@(sequence),@"captured_at_us":@(capturedWall),@"captured_monotonic_us":@(capturedMono),@"width":@(sourceWidth),@"height":@(sourceHeight),@"coordinate_space":@"guest-physical-pixels",@"mime_type":@"image/png",@"data_base64":[png base64EncodedStringWithOptions:0],@"byte_length":@(png.length)};
}
#endif
static void command(NSData *line){
 id obj=[NSJSONSerialization JSONObjectWithData:line options:0 error:nil];
 if(![obj isKindOfClass:NSDictionary.class]){reject(nil,0,@"invalid_request");return;}NSDictionary *request=obj;id ident=request[@"id"];
 // Microphone frames are fire-and-forget: no id, no reply.
 if([request[@"method"] isEqual:@"audio_in"]&&request.count==2&&[request[@"pcm"] isKindOfClass:NSString.class]){NSData *pcm=[[NSData alloc] initWithBase64EncodedString:request[@"pcm"] options:0];if(pcm.length&&pcm.length<=1920&&!(pcm.length%2))audioIn(pcm);return;}
 // Clipboard parts are fire-and-forget too; a malformed or oversized text is dropped whole.
 if([request[@"method"] isEqual:@"clipboard_chunk"]&&request.count==3&&[request[@"data"] isKindOfClass:NSString.class]&&[request[@"last"] isKindOfClass:NSNumber.class]){
  NSData *part=[[NSData alloc] initWithBase64EncodedString:request[@"data"] options:0];if(!clipboardParts)clipboardParts=[NSMutableData new];
  if(!part||clipboardParts.length+part.length>clipboardMax){clipboardParts=nil;clipboardEvent(@{@"error":@"clipboard text must be UTF-8 of at most 200 KiB"});return;}
  [clipboardParts appendData:part];if([request[@"last"] boolValue]){clipboardSet(clipboardParts);clipboardParts=nil;}return;}
 if([request[@"method"] isEqual:@"clipboard_get"]&&request.count==1){clipboardGet();return;}
 if([request[@"method"] isEqual:@"cursor_get"]&&request.count==1){if(lastCursor)cursorEvent(lastCursor);return;}
 if(!([ident isKindOfClass:NSString.class]&&[ident length]>0&&[ident length]<=128)){reject(nil,0,@"invalid_id");return;}
#ifdef VEYNODE_NATIVE_DISPLAY
 if(shutdownID){reject(ident,0,@"shutdown_pending");return;}
#endif
 NSString *method=request[@"method"];if(![method isKindOfClass:NSString.class]){reject(ident,0,@"invalid_method");return;}
 if([method isEqual:@"audio_start"]||[method isEqual:@"audio_stop"]){if(request.count!=2){reject(ident,0,@"invalid_request");return;}audioListening=[method isEqual:@"audio_start"];reply(ident,0,@{@"listening":@(audioListening),@"playback":@(playbackRate>0),@"record":@(recordChannel!=NULL)});return;}
 if([method isEqual:@"monitor_config"]){if(request.count!=4){reject(ident,0,@"invalid_request");return;}NSString *error=monitorConfig(request[@"width"],request[@"height"]);if(error)reject(ident,0,error);else reply(ident,0,@{@"width":@([request[@"width"] intValue]&~1),@"height":@([request[@"height"] intValue]&~1)});return;}
 if([method isEqual:@"capabilities"]){if(request.count!=2){reject(ident,0,@"invalid_window_request");return;}reply(ident,0,displayCapabilities());return;}
 if([@[@"show_window",@"close_window",@"window_status",@"validate_displayed_frame"] containsObject:method]){
  BOOL validating=[method isEqual:@"validate_displayed_frame"];id diagnosticID=request[@"diagnostic_id"];
  BOOL hasReceipt=request[@"acquisition_receipt"]||request[@"epoch"]||request[@"sequence"];
  if(request.count!=(validating?(3+(diagnosticID?1:0)+(hasReceipt?3:0)):2)||(!validating&&(diagnosticID||hasReceipt))){reject(ident,0,@"invalid_window_request");return;}
  #ifdef VEYNODE_NATIVE_DISPLAY
  NSDictionary *receiptFields=nil;
  if(hasReceipt){if(!VNFrameReceiptIdentifier(request[@"acquisition_receipt"])||!VNFrameUnsignedInteger(request[@"epoch"],NO)||!VNFrameUnsignedInteger(request[@"sequence"],NO)){reject(ident,0,@"invalid_window_request");return;}receiptFields=@{@"acquisition_receipt":request[@"acquisition_receipt"],@"epoch":request[@"epoch"],@"sequence":request[@"sequence"]};}
  if(diagnosticID&&!VNInputDiagnosticIDValid(diagnosticID)){reject(ident,0,@"invalid_window_request");return;}
#endif
#ifdef VEYNODE_NATIVE_DISPLAY
  windowCommand(method,ident,request[@"frame"],diagnosticID,receiptFields);
#else
  reject(ident,0,@"native_window_unavailable");
#endif
  return;
 }
 if([method isEqual:@"shutdown"]){closeEncoder();
#ifdef VEYNODE_NATIVE_DISPLAY
 shutdownID=[ident copy];shutdownDeadline=nativeClock()+2.5;[screenshotJob invalidate];armScreenshotTick();
#else
 reply(ident,0,@{@"state":@"stopped"});closing=YES;g_main_loop_quit(loop);
#endif
 return;}
 if([method isEqual:@"screenshot"]){
#ifdef VEYNODE_NATIVE_DISPLAY
 beginScreenshot(ident);
#else
 NSString *request=[ident copy];
 whenSourceCurrent(^{if(closing||fatal)return;NSDictionary *diagnostic=nil;NSDictionary *png=screenshot(&diagnostic);if(png){NSData *body=VNScreenshotResponseData(request,png);if(body)record(1,0,body);else captureFail(@"json_serialization_failed");}else{NSString *code=cacheValid()?@"screenshot_unavailable_or_too_large":@"frame_unavailable";emit(0,@{@"id":request,@"error":@{@"code":code,@"message":code,@"diagnostic":diagnostic}});}});
#endif
 return;}
 if([method isEqual:@"frame_status"]){BOOL valid=frameReady();reply(ident,0,@{@"capture_connected":(mainOpen&&displayOpen&&!fatal)?@YES:@NO,@"frame_available":valid?@YES:@NO,@"source_generation":@(generation),@"source_sequence":@(sequence),@"width":@(valid?sourceWidth:0),@"height":@(valid?sourceHeight:0)});return;}
 id requested=request[@"epoch"];double number=[requested isKindOfClass:NSNumber.class]?[requested doubleValue]:0;
 if(![requested isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)requested)==CFBooleanGetTypeID()||!isfinite(number)||number<1||number>9007199254740991.0||floor(number)!=number){reject(ident,0,@"invalid_epoch");return;}uint64_t e=(uint64_t)number;
 if([method isEqual:@"start"]){
  if(active){reject(ident,e,@"encoding_busy");return;}if(e<=lastEpoch){reject(ident,e,@"stale_epoch");return;}
  NSString *codec=request[@"codec"];if(![codec isKindOfClass:NSString.class]||(![codec isEqual:@"h264"]&&![codec isEqual:@"hevc"])){reject(ident,e,@"unsupported_codec");return;}
  id level=request[@"level"];if(level&&(![level isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)level)==CFBooleanGetTypeID()||![codec isEqual:@"h264"]||([level intValue]!=31&&[level intValue]!=40))){reject(ident,e,@"unsupported_level");return;}
  h264Level=level?[level intValue]:31;
  epoch=lastEpoch=e;codecName=codec;useHEVC=[codec isEqual:@"hevc"];active=YES;streamFPS=maxStreamFPS;encodePacing.initialized=false;forceKeyframe=YES;cropX=cropY=cropWidth=cropHeight=scaleWidth=scaleHeight=0;carriedBitrate=nil;firstPTS=lastEncode=0;encoded=keyframes=idleFrames=unchangedSkips=0;actualH264Profile=nil;actualHEVCProfile=nil;hardwareVerified=NO;failure=nil;
  BOOL ready=frameReady();
  reply(ident,e,@{@"state":ready?@"encoding":@"waiting-frame",@"epoch":@(epoch),@"frame_available":@(ready)});
  // A draw that arrives first encodes through the normal path (firstPTS is then set).
  whenSourceCurrent(^{if(!fatal&&active&&epoch==e&&!firstPTS&&cacheValid())encodeLatest(g_get_monotonic_time(),NO);});return;
 }
 if(!active||e!=epoch){reject(ident,e,@"stale_or_inactive_epoch");return;}
 if([@[@"rate_status",@"set_bitrate"] containsObject:method]){
  NSDictionary *strict=rateParse(line);if(!strict){reject(ident,e,@"invalid_request");return;}
  NSMutableDictionary *q=[strict mutableCopy];[q removeObjectForKey:@"id"];q[@"request_id"]=ident;
  if([method isEqual:@"rate_status"])[q removeObjectForKey:@"epoch"];
  leaveStill();
  NSDictionary *answer=rateApply(q,encoder,hardwareVerified,epoch,rateSubmitted);
  if(answer[@"error"])reject(ident,e,answer[@"error"][@"code"]);else reply(ident,e,answer[@"result"]);return;
 }
 if([method isEqual:@"set_scale"]||[method isEqual:@"set_crop"]){
  BOOL crop=[method isEqual:@"set_crop"];NSArray *keys=crop?@[@"x",@"y",@"width",@"height"]:@[@"width",@"height"];uint64_t v[4]={0};
  for(NSUInteger i=0;i<keys.count;i++){if(!rateUInt(request[keys[i]],0,4096)){reject(ident,e,@"invalid_request");return;}v[i]=[request[keys[i]] unsignedLongLongValue];}
  View before;if(!resolveView(&before)){reject(ident,e,@"frame_unavailable");return;}
  if(crop){BOOL full=!v[0]&&!v[1]&&!v[2]&&!v[3];if(!full&&(v[2]<2||v[3]<2||v[0]>=sourceWidth||v[1]>=sourceHeight)){reject(ident,e,@"crop_outside_bound");return;}
   NSUInteger saved[4]={cropX,cropY,cropWidth,cropHeight};cropX=v[0];cropY=v[1];cropWidth=full?0:v[2];cropHeight=full?0:v[3];
   View probe;if(!resolveView(&probe)){cropX=saved[0];cropY=saved[1];cropWidth=saved[2];cropHeight=saved[3];reject(ident,e,@"crop_outside_bound");return;}}
  else{if(!scaleFits(v[0],v[1],before.fitW,before.fitH)){reject(ident,e,@"scale_outside_bound");return;}scaleWidth=v[0];scaleHeight=v[1];}
  // Any change is a new compression session (rebuilt by the next encode) whose first frame is a keyframe.
  View after;resolveView(&after);
  leaveStill();invalidateSession(parkedEncoder);parkedEncoder=NULL;stillFailed=NO;lastChange=lastStill=0;
  if(encoder){
   CFTypeRef raw=NULL;if(VTSessionCopyProperty(encoder,kVTCompressionPropertyKey_AverageBitRate,NULL,&raw)==noErr)carriedBitrate=CFBridgingRelease(raw);else if(raw)CFRelease(raw);
   if(VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid)!=noErr)fail(@"Encoder drain failed");VTCompressionSessionInvalidate(encoder);CFRelease(encoder);encoder=NULL;
   if(scaled){CVPixelBufferRelease(scaled);scaled=NULL;}forceKeyframe=YES;
   if(failure){carriedBitrate=nil;sessionFailure();reject(ident,e,@"encoder_drain_failed");return;}
   if(cacheValid())encodeLatest(g_get_monotonic_time(),NO);
  }
  NSMutableDictionary *result=[viewFields(&after) mutableCopy];[result removeObjectForKey:@"event"];result[@"width"]=@(after.outW);result[@"height"]=@(after.outH);result[@"source_width"]=@(sourceWidth);result[@"source_height"]=@(sourceHeight);
  reply(ident,e,result);return;
 }
 if([method isEqual:@"set_fps"]){
  if(!rateUInt(request[@"fps"],1,maxStreamFPS)){reject(ident,e,@"invalid_request");return;}
  streamFPS=[request[@"fps"] unsignedIntValue];reply(ident,e,@{@"fps":@(streamFPS)});return;
 }
 if([method isEqual:@"stop"]){closeEncoder();if(failure){reject(ident,e,@"encoder_drain_failed");failure=nil;return;}reply(ident,e,@{@"state":@"stopped",@"encoded":@(encoded),@"keyframes":@(keyframes),@"idle_frames":@(idleFrames),@"frame_available":@(frameReady()),@"source_generation":@(generation),@"source_sequence":@(sequence)});return;}
 if([method isEqual:@"keyframe"]){forceKeyframe=YES;reply(ident,e,@{@"requested":@YES});if(cacheValid()&&g_get_monotonic_time()-lastEncode>=33333)encodeLatest(g_get_monotonic_time(),YES);return;}
 reject(ident,e,@"unsupported_method");
}
static gboolean control(GIOChannel *channel,GIOCondition condition,gpointer data){
 char bytes[1024];ssize_t count=read(STDIN_FILENO,bytes,sizeof(bytes));if(count==0){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}if(count<0){if(errno==EINTR||errno==EAGAIN)return G_SOURCE_CONTINUE;captureFail(@"stdin_failed");return G_SOURCE_REMOVE;}
 for(ssize_t i=0;i<count;i++){if(bytes[i]=='\n'){command(input);[input setLength:0];}else{[input appendBytes:bytes+i length:1];if(input.length>4096){captureFail(@"command_line_limit");return G_SOURCE_REMOVE;}}if(fatal||closing)return G_SOURCE_REMOVE;}scheduleFlush(g_get_monotonic_time());return G_SOURCE_CONTINUE;
}
static gboolean tick(gpointer data){@autoreleasepool{
#ifdef VEYNODE_NATIVE_DISPLAY
 if(outputWriter.failed){captureFail(@"stdout_closed_or_stalled");return G_SOURCE_CONTINUE;}
#endif
 if(generationFD>=0&&!surfaceLive()){awaitSource(@"generation_retired");return G_SOURCE_CONTINUE;}
 gint64 now=g_get_monotonic_time();
 if(active&&encoder&&!stillActive&&!stillFailed&&!forceKeyframe&&lastChange&&now-lastChange>=700000&&now-lastStill>=3000000)enterStill(now);
 else if(active&&cacheValid()&&((forceKeyframe&&now-lastEncode>=33333)||now-lastEncode>=1000000))encodeLatest(now,YES);
 // A window that became visible (un-minimised, uncovered) gets the draw it missed.
 scheduleFlush(now);return G_SOURCE_CONTINUE;
}}
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
 CVPixelBufferRelease(pixel);result[@"source"]=@"VideoToolbox required-hardware session property";[result addEntriesFromDictionary:displayCapabilities()];
 NSData *json=[NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingSortedKeys error:nil];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return 0;
}
#ifdef VEYNODE_SCREENSHOT_TESTING
#include "../console/ScreenshotPipelineTests.inc"
#endif
int main(int argc,char **argv){@autoreleasepool{
#ifdef VEYNODE_SCREENSHOT_TESTING
 if(argc==2&&!strcmp(argv[1],"--screenshot-pipeline-self-test"))return screenshotPipelineSelfTest();
 if(argc==2&&!strcmp(argv[1],"--lazy-capture-self-test"))return lazyCaptureSelfTest();
#endif
 lock=[NSLock new];if(argc==2&&!strcmp(argv[1],"--probe-capabilities")){NSData *json=[NSJSONSerialization dataWithJSONObject:displayCapabilities() options:NSJSONWritingSortedKeys error:nil];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return 0;}if(argc==2&&!strcmp(argv[1],"--probe-codecs"))return probeCodecs();
 if(argc!=3||strcmp(argv[2],"--persistent")){fprintf(stderr,"Usage: spice-console SOCKET --persistent\n");return 2;}
 signal(SIGPIPE,SIG_IGN);lock=[NSLock new];input=[NSMutableData new];fcntl(STDOUT_FILENO,F_SETFL,fcntl(STDOUT_FILENO,F_GETFL)|O_NONBLOCK);fcntl(STDIN_FILENO,F_SETFL,fcntl(STDIN_FILENO,F_GETFL)|O_NONBLOCK);
#ifdef VEYNODE_NATIVE_DISPLAY
 outputWriter=[[VNRecordWriter alloc] initWithFD:STDOUT_FILENO];if(!outputWriter)return 1;
#endif
 loop=g_main_loop_new(NULL,FALSE);SpiceSession *session=spice_session_new();g_object_set(session,"unix-path",argv[1],"enable-audio",TRUE,NULL);g_signal_connect(session,"channel-new",G_CALLBACK(newChannel),NULL);
 GIOChannel *channel=g_io_channel_unix_new(STDIN_FILENO);g_io_add_watch(channel,G_IO_IN|G_IO_HUP|G_IO_ERR,control,NULL);g_timeout_add(100,tick,NULL);
 if(!spice_session_connect(session))captureFail(@"spice_connect_failed");if(!fatal)g_main_loop_run(loop);
 #ifdef VEYNODE_NATIVE_DISPLAY
 // No success while a CPU task still owns a frame. ImageIO cannot be forcibly
 // cancelled; a stuck worker forces process failure, never a clean receipt.
 [screenshotJob invalidate];
 if(screenshotJob&&![screenshotJob waitFor:.25]){[outputWriter invalidate];fputs("screenshot_cleanup_unconfirmed\n",stderr);_exit(1);}
 screenshotJob=nil;screenshotWrite=nil;
 VeynodeDisplayInvalidate();
 #endif
 closeEncoder();clearCache();if(fatal)emit(0,@{@"event":@"capture-error",@"error":@{@"code":fatal,@"message":fatal}});
 // Disconnect only after final replies; callbacks must not add teardown errors.
 closing=YES;g_signal_handlers_disconnect_by_func(session,G_CALLBACK(newChannel),NULL);spice_session_disconnect(session);g_object_unref(session);g_io_channel_unref(channel);g_main_loop_unref(loop);loop=NULL;
#ifdef VEYNODE_NATIVE_DISPLAY
 if(![outputWriter drainFor:.6]){if(!fatal)fatal=@"stdout_cleanup_unconfirmed";[outputWriter invalidate];[outputWriter drainFor:.1];if(outputWriter.pending){fputs("stdout_cleanup_unconfirmed\n",stderr);_exit(1);}}
#endif
 if(generationFD>=0)close(generationFD);if(surface)CFRelease(surface);
 NSData *summary=[NSJSONSerialization dataWithJSONObject:@{@"event":@"capture-stopped",@"error":fatal?:[NSNull null],@"source_copies":@(sourceCopies),@"on_demand_source_copies":@(onDemandCopies),@"deferred_draws":@(deferredDraws),@"resize_copies":@(resizeCopies),@"draw_events":@(received),@"cached_frame_released":@YES} options:NSJSONWritingSortedKeys error:nil];fwrite(summary.bytes,1,summary.length,stderr);fputc('\n',stderr);return fatal?1:0;
}}
