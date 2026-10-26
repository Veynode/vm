#include "../shared/encoder-rate.h"
// Per-VM persistent SPICE capture owner. Local single-owner framed pipe only.
#include "hevc-profile.h"
#include "../console/EncodedGeometry.h"
#include "../console/ScreenshotDiagnostic.h"
#ifdef VEYNODE_NATIVE_DISPLAY
#import "../console/Display.h"
#import "../console/InputFrameDiagnostic.h"
#import "../console/FrameAcquisition.h"
#include "../console/FramePacing.h"
#import "../console/CopyTiming.h"
#import "../console/AsyncScreenshot.h"
#import "../console/RecordWriter.h"
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
static const gint64 encodeInterval=16667;
static int generationFD=-1;
static NSMutableData *input;
static const NSUInteger maxAU=4*1024*1024;
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
// One required-hardware session attempt; NO leaves no session behind.
static BOOL createEncoderWith(CVPixelBufferRef pixel,NSDictionary *spec,BOOL reportFailure){
    NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey:@(CVPixelBufferGetPixelFormatType(pixel)),(__bridge NSString *)kCVPixelBufferWidthKey:@(width),(__bridge NSString *)kCVPixelBufferHeightKey:@(height),(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
    OSStatus result=VTCompressionSessionCreate(NULL,(int32_t)width,(int32_t)height,useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,(__bridge CFDictionaryRef)spec,(__bridge CFDictionaryRef)attributes,NULL,compressed,NULL,&encoder);
    NSString *problem=nil;
    if(result!=noErr){encoder=NULL;problem=[NSString stringWithFormat:@"Required hardware %@ creation failed: %d",codecName,(int)result];}
    if(!problem){
    // The H.264 hint stays at 30: at 60 the encoder picks a level above the
    // Constrained Baseline 3.1 this helper guarantees. Frames still go at 60.
    NSDictionary *properties=@{(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:useHEVC?@60:@30,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@120,(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:(__bridge NSString *)(useHEVC?kVTProfileLevel_HEVC_Main_AutoLevel:kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
    for(NSString *key in properties){result=VTSessionSetProperty(encoder,(__bridge CFStringRef)key,(__bridge CFTypeRef)properties[key]);if(result!=noErr){problem=[NSString stringWithFormat:@"Encoder property rejected %@: %d",key,(int)result];break;}}
    }
    if(!problem){
    // Best effort: not every encoder generation exposes these latency knobs.
    VTSessionSetProperty(encoder,kVTCompressionPropertyKey_MaxFrameDelayCount,(__bridge CFTypeRef)@0);
    VTSessionSetProperty(encoder,kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,kCFBooleanTrue);
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
    rateReset();rateSubmitted=0;return YES;
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
static BOOL copyPixels(IOSurfaceRef src,NSUInteger sw,NSUInteger sh,CVPixelBufferRef destination
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
  NSString *shader=@"#include <metal_stdlib>\nusing namespace metal;\nkernel void copy_frame(texture2d<float,access::read> src [[texture(0)]],texture2d<float,access::write> dst [[texture(1)]],uint2 p [[thread_position_in_grid]]){if(p.x>=dst.get_width()||p.y>=dst.get_height())return;uint sx=min(src.get_width()-1,((2*p.x+1)*src.get_width())/(2*dst.get_width()));uint sy=min(src.get_height()-1,((2*p.y+1)*src.get_height())/(2*dst.get_height()));dst.write(src.read(uint2(sx,sy)),p);}";
  id<MTLLibrary> library=[metal newLibraryWithSource:shader options:nil error:&error];if(library)pipeline=[metal newComputePipelineStateWithFunction:[library newFunctionWithName:@"copy_frame"] error:&error];if(!queue||!pipeline)COPY_RETURN(NO);
 }
 NSUInteger dw=CVPixelBufferGetWidth(destination),dh=CVPixelBufferGetHeight(destination);
 MTLTextureDescriptor *d=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:sw height:sh mipmapped:NO];d.storageMode=MTLStorageModeShared;d.usage=MTLTextureUsageShaderRead;
 id<MTLTexture> from=[metal newTextureWithDescriptor:d iosurface:src plane:0];d.width=dw;d.height=dh;d.usage=MTLTextureUsageShaderWrite;
 id<MTLTexture> to=[metal newTextureWithDescriptor:d iosurface:CVPixelBufferGetIOSurface(destination) plane:0];command=[queue commandBuffer];id<MTLComputeCommandEncoder> compute=[command computeCommandEncoder];
 if(!from||!to||!compute)COPY_RETURN(NO);[compute setComputePipelineState:pipeline];[compute setTexture:from atIndex:0];[compute setTexture:to atIndex:1];[compute dispatchThreads:MTLSizeMake(dw,dh,1) threadsPerThreadgroup:MTLSizeMake(16,16,1)];[compute endEncoding];
#ifdef VEYNODE_NATIVE_DISPLAY
 commitBegin=g_get_monotonic_time()/1e6;
#endif
 [command commit];[command waitUntilCompleted];COPY_RETURN(command.status==MTLCommandBufferStatusCompleted);
#undef COPY_RETURN
}
static BOOL cacheValid(void){return owned&&cachedGeneration==generation&&surfaceLive();}
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
static void closeEncoder(void){
 if(encoder){if(VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid)!=noErr)fail(@"Encoder drain failed");VTCompressionSessionInvalidate(encoder);CFRelease(encoder);encoder=NULL;}
 if(scaled){CVPixelBufferRelease(scaled);scaled=NULL;}active=NO;forceKeyframe=NO;
}
static void sessionFailure(void){
 if(!failure)return;NSString *message=failure;closeEncoder();failure=nil;
 emit(epoch,@{@"event":@"session-error",@"epoch":@(epoch),@"error":@{@"code":@"encoder_failed",@"message":message}});
}
static void encodeLatest(gint64 now,BOOL idle){
 if(!active||!cacheValid())return;
 if(!encoder){
  size_t selectedWidth=0,selectedHeight=0;
  if(!VNEncodedGeometry(sourceWidth,sourceHeight,useHEVC,&selectedWidth,&selectedHeight)){fail(@"Source geometry cannot fit even codec bounds");sessionFailure();return;}
  width=selectedWidth;height=selectedHeight;
  if(width!=sourceWidth||height!=sourceHeight){scaled=allocatePixel(width,height);if(!scaled){fail(@"Scale buffer unavailable");sessionFailure();return;}}
  if(!createEncoder(scaled?:owned)){sessionFailure();return;}
  emit(epoch,@{@"event":@"encoding",@"codec":codecName,@"hardware_verified":@(hardwareVerified),@"width":@(width),@"height":@(height),@"source_width":@(sourceWidth),@"source_height":@(sourceHeight),@"source_generation":@(generation),@"source_sequence":@(sequence)});
 }
 CVPixelBufferRef pixel=owned;
 if(scaled){if(!copyPixels(CVPixelBufferGetIOSurface(owned),sourceWidth,sourceHeight,scaled
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
static void awaitSource(NSString *reason){
#ifdef VEYNODE_NATIVE_DISPLAY
 VeynodeDisplayInvalidate();capturePacing.initialized=false;
#endif
 cachedGeneration=0; // Invalidate publication before draining, retain storage until drain.
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
 IOSurfaceID ident=0;if(read(info->fd,&ident,4)!=4){captureFail(@"incomplete_scanout_id");return;}
 IOSurfaceRef observed=IOSurfaceLookup(ident);
 if(scanoutLogCount++<32){NSDictionary *event=@{@"event":@"scanout-observed",@"source_generation":@(generation),@"width":@(info->width),@"height":@(info->height),@"y0top":@(info->y0top),@"surface_available":@(observed!=NULL),@"surface_width":@(observed?IOSurfaceGetWidth(observed):0),@"surface_height":@(observed?IOSurfaceGetHeight(observed):0),@"pixel_format":@(observed?IOSurfaceGetPixelFormat(observed):0)};NSData *json=[NSJSONSerialization dataWithJSONObject:event options:NSJSONWritingSortedKeys error:nil];fwrite(json.bytes,1,json.length,stderr);fputc('\n',stderr);fflush(stderr);}
 if(!info->width||!info->height||info->width>3840||info->height>2160||info->width%2||info->height%2||info->y0top){emit(0,@{@"event":@"source-unavailable",@"reason":@"unsupported_scanout_geometry",@"source_generation":@(generation),@"width":@(info->width),@"height":@(info->height),@"y0top":@(info->y0top),@"frame_available":@NO});if(observed)CFRelease(observed);return;}
 surface=observed;
 if(!surface){struct pollfd latest={.fd=info->fd,.events=POLLIN};if(poll(&latest,1,0)>=0&&(latest.revents&POLLHUP))return;captureFail(@"invalid_scanout_surface");return;}
 if(IOSurfaceGetWidth(surface)!=info->width||IOSurfaceGetHeight(surface)!=info->height){captureFail(@"invalid_scanout_surface_geometry");return;}
 if(IOSurfaceGetPixelFormat(surface)!=kCVPixelFormatType_32BGRA){CFRelease(surface);surface=NULL;emit(0,@{@"event":@"source-unavailable",@"reason":@"unsupported_pixel_format",@"source_generation":@(generation),@"frame_available":@NO});return;}
 generationFD=dup(info->fd);if(generationFD<0){captureFail(@"scanout_fd_failed");return;}fcntl(generationFD,F_SETFD,FD_CLOEXEC);sourceWidth=info->width;sourceHeight=info->height;awaitingSource=NO;lastCapture=0;
 emit(0,@{@"event":@"source-generation",@"source_generation":@(generation),@"width":@(sourceWidth),@"height":@(sourceHeight)});
}
// Copy the live scanout into an owned buffer when pacing admits it; a refused
// update stays dirty for the flush tick. Returns whether a new copy exists.
static BOOL captureSource(gint64 now){
 if(!surfaceLive()){awaitSource(@"generation_retired_during_draw");return NO;}
#ifdef VEYNODE_NATIVE_DISPLAY
 BOOL nativeVisible=VeynodeDisplayVisible();
 // A browser stream gets the visible cadence even while the window is hidden.
 if(!VNFramePacingAdmit(&capturePacing,now,nativeVisible||active)){capturePacingDrops++;sourceDirty=YES;return NO;}
 CVPixelBufferRef next=VeynodeDisplayAcquire(sourceWidth,sourceHeight);
 // Pool exhaustion drops this copy without blocking ACK; the flush retries.
 if(!next){capturePoolDrops++;sourceDirty=YES;return NO;}
 if(!copyPixels(surface,sourceWidth,sourceHeight,next,VNCopySource)){VeynodeDisplayRelease(next);captureFail(@"source_copy_failed");return NO;}
 if(!surfaceLive()){VeynodeDisplayRelease(next);awaitSource(@"generation_retired_during_copy");return NO;}
 clearCache();owned=next;sourceCopies++;sequence++;cachedGeneration=generation;capturedMono=g_get_monotonic_time();capturedWall=g_get_real_time();lastCapture=now;if(nativeVisible)visibleSourceCopies++;else backgroundSourceCopies++;VeynodeDisplayPublish(owned,generation,sequence,capturedMono);
#else
 if(lastCapture&&now-lastCapture<encodeInterval){sourceDirty=YES;return NO;}
 cachedGeneration=0;if(!owned)owned=allocatePixel(sourceWidth,sourceHeight);
 if(!owned||!copyPixels(surface,sourceWidth,sourceHeight,owned)){clearCache();captureFail(@"source_copy_failed");return NO;}
 if(!surfaceLive()){awaitSource(@"generation_retired_during_copy");return NO;}
 sourceCopies++;sequence++;cachedGeneration=generation;capturedMono=g_get_monotonic_time();capturedWall=g_get_real_time();lastCapture=now;
#endif
 sourceDirty=NO;encodeDirty=YES;return YES;
}
static void encodeIfDue(gint64 now){
 if(!fatal&&active&&cacheValid()&&encodeDirty&&(!lastEncode||now-lastEncode>=encodeInterval))encodeLatest(now,NO);
}
static void draw(SpiceDisplayChannel *channel,guint32 x,guint32 y,guint32 w,guint32 h,gpointer context){@autoreleasepool{
 received++;gint64 now=g_get_monotonic_time();
 captureSource(now);
 // Producer ACK is after the source copy, before encoding our owned buffer.
 spice_display_channel_gl_draw_done(channel);
 encodeIfDue(now);
}}
static gboolean flush(gpointer data){@autoreleasepool{
 if(closing||fatal)return G_SOURCE_CONTINUE;
 gint64 now=g_get_monotonic_time();
 if(sourceDirty&&generationFD>=0&&surfaceLive())captureSource(now);
 encodeIfDue(now);
 return G_SOURCE_CONTINUE;
}}
static void channelEvent(SpiceChannel *channel,SpiceChannelEvent event,gpointer data){
 if(closing)return;
 if(channelLogCount++<32){int type=0,ident=0;g_object_get(channel,"channel-type",&type,"channel-id",&ident,NULL);NSData *log=[NSJSONSerialization dataWithJSONObject:@{@"event":@"channel-event",@"channel_type":@(type),@"channel_id":@(ident),@"event_code":@(event)} options:NSJSONWritingSortedKeys error:nil];fwrite(log.bytes,1,log.length,stderr);fputc('\n',stderr);fflush(stderr);}
 if(event==SPICE_CHANNEL_OPENED){if(SPICE_IS_MAIN_CHANNEL(channel))mainOpen=YES;if(SPICE_IS_DISPLAY_CHANNEL(channel))displayOpen=YES;
  if(mainOpen&&displayOpen&&!readySent){readySent=YES;emit(0,@{@"event":@"ready",@"protocol":@1,@"capture_connected":@YES,@"frame_available":@(cacheValid())});}}
 else if(event==SPICE_CHANNEL_CLOSED||event>=SPICE_CHANNEL_ERROR_CONNECT){
#ifdef VEYNODE_NATIVE_DISPLAY
 VeynodeDisplayInvalidate();
#endif
 clearCache();captureFail(@"spice_disconnected");}
}
static void newChannel(SpiceSession *session,SpiceChannel *channel,gpointer data){
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
  void (^finish)(void)=[^{displayCommandPending=NO;if(!closing&&!fatal){if(error)reject(ident,0,error);else{if([method isEqual:@"show_window"]&&cacheValid())VeynodeDisplayPublish(owned,generation,sequence,capturedMono);if([method isEqual:@"validate_displayed_frame"]){reply(ident,0,result);return;}NSMutableDictionary *status=[result mutableCopy];if(!copyTiming)copyTiming=[VNCopyTiming new];status[@"copy_timing"]=[copyTiming statistics];status[@"screenshot_timing"]=lastScreenshotTiming?:[NSNull null];status[@"source_width"]=@(cacheValid()?sourceWidth:0);status[@"source_height"]=@(cacheValid()?sourceHeight:0);status[@"source_frame_available"]=@(cacheValid());status[@"encoder_width"]=@(encoder?width:0);status[@"encoder_height"]=@(encoder?height:0);status[@"source_draw_events"]=@(received);status[@"source_copies"]=@(sourceCopies);status[@"capture_pacing_drops"]=@(capturePacingDrops);status[@"capture_pool_drops"]=@(capturePoolDrops);status[@"visible_source_copies"]=@(visibleSourceCopies);status[@"background_source_copies"]=@(backgroundSourceCopies);status[@"encoder_output_frames_current_epoch"]=@(encoded);status[@"encoder_idle_frames_current_epoch"]=@(idleFrames);status[@"encoder_active"]=@(active);status[@"sampled_monotonic_us"]=@(g_get_monotonic_time());reply(ident,0,status);}}} copy];
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
static void beginScreenshot(NSString *ident){
 if(screenshotJob||screenshotWrite){reject(ident,0,@"screenshot_busy");return;}
 if(!cacheValid()||!mainOpen||!displayOpen){emit(0,@{@"id":ident,@"error":@{@"code":@"frame_unavailable",@"message":@"frame_unavailable",@"diagnostic":VNScreenshotFailure(@"cache_invalid_before_copy",nil)}});return;}
 CVPixelBufferRef pixel=owned;
 if(!VeynodeDisplayRetain(pixel)){reject(ident,0,@"frame_unavailable");return;}
 screenshotID=[ident copy];screenshotMetadata=@{@"source_generation":@(generation),@"source_sequence":@(sequence),@"captured_at_us":@(capturedWall),@"captured_monotonic_us":@(capturedMono),@"width":@(sourceWidth),@"height":@(sourceHeight),@"coordinate_space":@"guest-physical-pixels",@"mime_type":@"image/png"};
 screenshotTiming=[@{@"version":@1,@"timing_clock":@"CLOCK_MONOTONIC",@"source_generation":@(generation),@"source_sequence":@(sequence),@"admitted_monotonic_us":@(nativeClock()*1e6),@"success":@NO} mutableCopy];
 screenshotDeadline=nativeClock()+2.0;screenshotDelivered=NO;
 screenshotJob=[[VNAsyncScreenshot alloc] initWithPixel:pixel metadata:screenshotMetadata requestID:screenshotID release:^{VeynodeDisplayRelease(pixel);}];
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
  if(!screenshotJob&&!shutdownReplied){reply(shutdownID,0,@{@"state":@"stopped"});shutdownReplied=YES;}
  if(shutdownReplied&&outputWriter.pending==0){closing=YES;g_main_loop_quit(loop);}
 }
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
 if(!([ident isKindOfClass:NSString.class]&&[ident length]>0&&[ident length]<=128)){reject(nil,0,@"invalid_id");return;}
#ifdef VEYNODE_NATIVE_DISPLAY
 if(shutdownID){reject(ident,0,@"shutdown_pending");return;}
#endif
 NSString *method=request[@"method"];if(![method isKindOfClass:NSString.class]){reject(ident,0,@"invalid_method");return;}
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
 shutdownID=[ident copy];shutdownDeadline=nativeClock()+2.5;[screenshotJob invalidate];
#else
 reply(ident,0,@{@"state":@"stopped"});closing=YES;g_main_loop_quit(loop);
#endif
 return;}
 if([method isEqual:@"screenshot"]){
#ifdef VEYNODE_NATIVE_DISPLAY
 beginScreenshot(ident);
#else
 NSDictionary *diagnostic=nil;NSDictionary *png=screenshot(&diagnostic);if(png){NSData *body=VNScreenshotResponseData(ident,png);if(body)record(1,0,body);else captureFail(@"json_serialization_failed");}else{NSString *code=cacheValid()?@"screenshot_unavailable_or_too_large":@"frame_unavailable";emit(0,@{@"id":ident,@"error":@{@"code":code,@"message":code,@"diagnostic":diagnostic}});}
#endif
 return;}
 if([method isEqual:@"frame_status"]){BOOL valid=cacheValid();reply(ident,0,@{@"capture_connected":(mainOpen&&displayOpen&&!fatal)?@YES:@NO,@"frame_available":valid?@YES:@NO,@"source_generation":@(generation),@"source_sequence":@(sequence),@"width":@(valid?sourceWidth:0),@"height":@(valid?sourceHeight:0)});return;}
 id requested=request[@"epoch"];double number=[requested isKindOfClass:NSNumber.class]?[requested doubleValue]:0;
 if(![requested isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)requested)==CFBooleanGetTypeID()||!isfinite(number)||number<1||number>9007199254740991.0||floor(number)!=number){reject(ident,0,@"invalid_epoch");return;}uint64_t e=(uint64_t)number;
 if([method isEqual:@"start"]){
  if(active){reject(ident,e,@"encoding_busy");return;}if(e<=lastEpoch){reject(ident,e,@"stale_epoch");return;}
  NSString *codec=request[@"codec"];if(![codec isKindOfClass:NSString.class]||(![codec isEqual:@"h264"]&&![codec isEqual:@"hevc"])){reject(ident,e,@"unsupported_codec");return;}
  epoch=lastEpoch=e;codecName=codec;useHEVC=[codec isEqual:@"hevc"];active=YES;forceKeyframe=YES;firstPTS=lastEncode=0;encoded=keyframes=idleFrames=0;actualH264Profile=nil;actualHEVCProfile=nil;hardwareVerified=NO;failure=nil;
  reply(ident,e,@{@"state":cacheValid()?@"encoding":@"waiting-frame",@"epoch":@(epoch),@"frame_available":@(cacheValid())});if(cacheValid())encodeLatest(g_get_monotonic_time(),NO);return;
 }
 if(!active||e!=epoch){reject(ident,e,@"stale_or_inactive_epoch");return;}
 if([@[@"rate_status",@"set_bitrate"] containsObject:method]){
  NSDictionary *strict=rateParse(line);if(!strict){reject(ident,e,@"invalid_request");return;}
  NSMutableDictionary *q=[strict mutableCopy];[q removeObjectForKey:@"id"];q[@"request_id"]=ident;
  if([method isEqual:@"rate_status"])[q removeObjectForKey:@"epoch"];
  NSDictionary *answer=rateApply(q,encoder,hardwareVerified,epoch,rateSubmitted);
  if(answer[@"error"])reject(ident,e,answer[@"error"][@"code"]);else reply(ident,e,answer[@"result"]);return;
 }
 if([method isEqual:@"stop"]){closeEncoder();if(failure){reject(ident,e,@"encoder_drain_failed");failure=nil;return;}reply(ident,e,@{@"state":@"stopped",@"encoded":@(encoded),@"keyframes":@(keyframes),@"idle_frames":@(idleFrames),@"frame_available":@(cacheValid()),@"source_generation":@(generation),@"source_sequence":@(sequence)});return;}
 if([method isEqual:@"keyframe"]){forceKeyframe=YES;reply(ident,e,@{@"requested":@YES});if(cacheValid()&&g_get_monotonic_time()-lastEncode>=33333)encodeLatest(g_get_monotonic_time(),YES);return;}
 reject(ident,e,@"unsupported_method");
}
static gboolean control(GIOChannel *channel,GIOCondition condition,gpointer data){
 char bytes[1024];ssize_t count=read(STDIN_FILENO,bytes,sizeof(bytes));if(count==0){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}if(count<0){if(errno==EINTR||errno==EAGAIN)return G_SOURCE_CONTINUE;captureFail(@"stdin_failed");return G_SOURCE_REMOVE;}
 for(ssize_t i=0;i<count;i++){if(bytes[i]=='\n'){command(input);[input setLength:0];}else{[input appendBytes:bytes+i length:1];if(input.length>4096){captureFail(@"command_line_limit");return G_SOURCE_REMOVE;}}if(fatal||closing)return G_SOURCE_REMOVE;}return G_SOURCE_CONTINUE;
}
static gboolean tick(gpointer data){@autoreleasepool{
 if(generationFD>=0&&!surfaceLive()){awaitSource(@"generation_retired");return G_SOURCE_CONTINUE;}
 gint64 now=g_get_monotonic_time();if(active&&cacheValid()&&((forceKeyframe&&now-lastEncode>=33333)||now-lastEncode>=1000000))encodeLatest(now,YES);return G_SOURCE_CONTINUE;
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
#endif
 lock=[NSLock new];if(argc==2&&!strcmp(argv[1],"--probe-capabilities")){NSData *json=[NSJSONSerialization dataWithJSONObject:displayCapabilities() options:NSJSONWritingSortedKeys error:nil];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return 0;}if(argc==2&&!strcmp(argv[1],"--probe-codecs"))return probeCodecs();
 if(argc!=3||strcmp(argv[2],"--persistent")){fprintf(stderr,"Usage: spice-console SOCKET --persistent\n");return 2;}
 signal(SIGPIPE,SIG_IGN);lock=[NSLock new];input=[NSMutableData new];fcntl(STDOUT_FILENO,F_SETFL,fcntl(STDOUT_FILENO,F_GETFL)|O_NONBLOCK);fcntl(STDIN_FILENO,F_SETFL,fcntl(STDIN_FILENO,F_GETFL)|O_NONBLOCK);
#ifdef VEYNODE_NATIVE_DISPLAY
 outputWriter=[[VNRecordWriter alloc] initWithFD:STDOUT_FILENO];if(!outputWriter)return 1;
#endif
 loop=g_main_loop_new(NULL,FALSE);SpiceSession *session=spice_session_new();g_object_set(session,"unix-path",argv[1],NULL);g_signal_connect(session,"channel-new",G_CALLBACK(newChannel),NULL);
 GIOChannel *channel=g_io_channel_unix_new(STDIN_FILENO);g_io_add_watch(channel,G_IO_IN|G_IO_HUP|G_IO_ERR,control,NULL);g_timeout_add(100,tick,NULL);g_timeout_add(4,flush,NULL);
#ifdef VEYNODE_NATIVE_DISPLAY
 g_timeout_add(10,screenshotTick,NULL);
#endif
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
 NSData *summary=[NSJSONSerialization dataWithJSONObject:@{@"event":@"capture-stopped",@"error":fatal?:[NSNull null],@"source_copies":@(sourceCopies),@"resize_copies":@(resizeCopies),@"draw_events":@(received),@"cached_frame_released":@YES} options:NSJSONWritingSortedKeys error:nil];fwrite(summary.bytes,1,summary.length,stderr);fputc('\n',stderr);return fatal?1:0;
}}
