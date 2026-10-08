// Guest raw NV12 -> required-hardware host VideoToolbox -> framed Annex B.
// No host capture API, software fallback, recording or public network listener.
#include "hevc-profile.h"
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <errno.h>
#include <time.h>
static VTCompressionSessionRef encoder;
static NSLock *lock;
static NSString *failure,*codecName,*actualH264Profile;
static NSDictionary *actualHEVCProfile;
static BOOL useHEVC,hardwareVerified,hardwareProbe,probeOutput;
static NSUInteger width,height,encoded,keyframes;
static const NSUInteger maxAU=4*1024*1024;
static int64_t nowUs(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return (int64_t)t.tv_sec*1000000+t.tv_nsec/1000;}
static void fail(NSString *message){[lock lock];if(!failure)failure=message;[lock unlock];}
static void diagnostic(NSDictionary *value){NSData *data=[NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:NULL];fwrite(data.bytes,1,data.length,stderr);fputc('\n',stderr);fflush(stderr);}
static BOOL appendNAL(NSMutableData *au,const void *bytes,size_t count){static const uint8_t start[]={0,0,0,1};if(count==0||count>maxAU-4||au.length>maxAU-count-4){fail(@"Access unit exceeds 4MiB or empty NAL");return NO;}[au appendBytes:start length:4];[au appendBytes:bytes length:count];return YES;}
static BOOL writeRecord(NSData *payload,uint64_t pts,BOOL key){
 uint8_t header[13];uint32_t length=CFSwapInt32HostToBig((uint32_t)payload.length);uint64_t timestamp=CFSwapInt64HostToBig(pts);memcpy(header,&length,4);memcpy(header+4,&timestamp,8);header[12]=key?1:0;
 const uint8_t *parts[]={header,payload.bytes};size_t lengths[]={sizeof(header),payload.length};int64_t deadline=nowUs()+500000;
 for(int part=0;part<2;part++){size_t sent=0;while(sent<lengths[part]){
  if(nowUs()>=deadline){fail(@"stdout backpressure exceeded 500ms; partial record must be discarded");return NO;}
  ssize_t amount=write(STDOUT_FILENO,parts[part]+sent,lengths[part]-sent);
  if(amount>0){sent+=(size_t)amount;continue;}if(amount<0&&errno==EINTR)continue;
  if(amount<0&&(errno==EAGAIN||errno==EWOULDBLOCK)){struct pollfd p={.fd=STDOUT_FILENO,.events=POLLOUT};int wait=(int)MAX(1,(deadline-nowUs())/1000);int result=poll(&p,1,wait);if(result<0&&errno==EINTR)continue;if(result>0&&!(p.revents&(POLLHUP|POLLERR|POLLNVAL)))continue;}
  fail(@"stdout closed, errored or stalled; partial record must be discarded");return NO;
 }}return YES;
}
static void compressed(void *context,void *source,OSStatus status,VTEncodeInfoFlags flags,CMSampleBufferRef sample){@autoreleasepool{
 if(status!=noErr||!sample||!CMSampleBufferDataIsReady(sample)||(flags&kVTEncodeInfo_FrameDropped)){fail([NSString stringWithFormat:@"Encode callback failed status=%d flags=%u",(int)status,(unsigned)flags]);return;}
 CMFormatDescriptionRef format=CMSampleBufferGetFormatDescription(sample);
 if(CMFormatDescriptionGetMediaSubType(format)!=(useHEVC?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264)){fail(@"Unexpected output codec");return;}
 if(hardwareProbe){probeOutput=YES;return;}

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
 CMBlockBufferRef block=CMSampleBufferGetDataBuffer(sample);if(!block){fail(@"Missing compressed block");return;}size_t total=CMBlockBufferGetDataLength(block),offset=0;
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
    NSDictionary *properties=@{(__bridge NSString *)kVTCompressionPropertyKey_ColorPrimaries:(__bridge NSString *)kCVImageBufferColorPrimaries_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_TransferFunction:(__bridge NSString *)kCVImageBufferTransferFunction_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_YCbCrMatrix:(__bridge NSString *)kCVImageBufferYCbCrMatrix_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:@30,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@30,(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:(__bridge NSString *)(useHEVC?kVTProfileLevel_HEVC_Main_AutoLevel:kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
    for(NSString *key in properties){result=VTSessionSetProperty(encoder,(__bridge CFStringRef)key,(__bridge CFTypeRef)properties[key]);if(result!=noErr){fail([NSString stringWithFormat:@"Encoder property rejected %@: %d",key,(int)result]);return NO;}}
    if(VTCompressionSessionPrepareToEncodeFrames(encoder)!=noErr){fail(@"Encoder preparation failed");return NO;}
    CFTypeRef usingHardware=NULL;
    result=VTSessionCopyProperty(encoder,kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,NULL,&usingHardware);
    hardwareVerified=result==noErr&&usingHardware&&CFGetTypeID(usingHardware)==CFBooleanGetTypeID()&&CFBooleanGetValue(usingHardware);
    if(usingHardware)CFRelease(usingHardware);
    if(!hardwareVerified){fail(@"Encoder did not confirm hardware acceleration; software fallback rejected");return NO;}
    return YES;
}
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
 result[@"protocol"]=@"veynode-guest-video-stream-v1";result[@"cleanup_contract_version"]=@1;result[@"cleanup_confirmed_failure_exit"]=@78;
 NSData *json=[NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingSortedKeys error:NULL];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return 0;
}
static BOOL stopping,forceKeyframe,started,stopped;
static BOOL terminalReceived,cleanCaptureFailure;
// Exit78 is opt-in via the exact capabilities contract. It remains a failed
// media outcome: only a matched Guest error+terminal+EOF attests cleanup.
// EOF alone, a process exit or an ordinary capture error never suffices.
static NSDictionary *guestError;
static NSString *cleanupBasis;
static NSDictionary *secondaryStopError;
static NSMutableData *commands,*incoming;
static CVPixelBufferRef cachedPixel;
static uint64_t sourceSequence,sourcePTS,submitted,cpuCopies,idleFrames;
static int64_t firstSubmit,lastSubmit;
static uint64_t lastPTS;
static int sourceFD=-1;
static uint32_t read32(const void *p){uint32_t v;memcpy(&v,p,4);return CFSwapInt32BigToHost(v);}
static uint64_t read64(const void *p){uint64_t v;memcpy(&v,p,8);return CFSwapInt64BigToHost(v);}
static BOOL sendCommand(NSDictionary *value){
 NSData *json=[NSJSONSerialization dataWithJSONObject:value options:0 error:NULL];NSMutableData *line=[json mutableCopy];[line appendBytes:"\n" length:1];
 const uint8_t *p=line.bytes;size_t sent=0;int64_t end=nowUs()+500000;
 while(sent<line.length){if(nowUs()>=end){fail(@"Guest control write exceeded 500ms deadline");return NO;}ssize_t n=write(sourceFD,p+sent,line.length-sent);if(n>0){sent+=n;continue;}if(n<0&&errno==EINTR)continue;
  if(n<0&&(errno==EAGAIN||errno==EWOULDBLOCK)&&nowUs()<end){struct pollfd f={sourceFD,POLLOUT,0};if(poll(&f,1,20)>=0)continue;}
  fail(@"Guest control write failed or exceeded deadline");return NO;
 }return YES;
}
static void readCommands(void){
 uint8_t bytes[128];ssize_t n=read(STDIN_FILENO,bytes,sizeof(bytes));if(n==0){stopping=YES;return;}if(n<0){if(errno!=EINTR&&errno!=EAGAIN){fail(@"stdin read failed");stopping=YES;}return;}
 for(ssize_t i=0;i<n;i++){if(bytes[i]=='\n'){
  NSString *line=[[NSString alloc]initWithData:commands encoding:NSUTF8StringEncoding];[commands setLength:0];
  if([line isEqual:@"stop"])stopping=YES;else if([line isEqual:@"keyframe"])forceKeyframe=YES;else{fail(@"Invalid stdin command");stopping=YES;}
 }else{[commands appendBytes:bytes+i length:1];if(commands.length>64){fail(@"stdin command exceeds limit");stopping=YES;return;}}}
}
static void encodeCached(BOOL idle){
 if(!cachedPixel||failure||stopping)return;
 int64_t now=nowUs();uint64_t pts=0;if(!submitted)firstSubmit=now;else pts=MAX(lastPTS+1,(uint64_t)(now-firstSubmit));
 NSDictionary *options=(forceKeyframe||submitted==0)?@{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame:@YES}:nil;forceKeyframe=NO;
 OSStatus result=VTCompressionSessionEncodeFrame(encoder,cachedPixel,CMTimeMake((int64_t)pts,1000000),CMTimeMake(1,30),(__bridge CFDictionaryRef)options,NULL,NULL);
 if(result!=noErr||VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid)!=noErr){fail(@"Hardware frame encode failed");return;}
 lastSubmit=now;lastPTS=pts;submitted++;if(idle)idleFrames++;
}
static BOOL unsignedNumber(id value,uint64_t expected){return [value isKindOfClass:[NSNumber class]]&&CFGetTypeID((__bridge CFTypeRef)value)!=CFBooleanGetTypeID()&&[value doubleValue]==(double)expected;}
static void record(NSData *body,BOOL draining){
 const uint8_t *p=body.bytes;if(!body.length){fail(@"Empty Guest record");return;}
 if(p[0]==1){
  if(body.length>65536){fail(@"Guest JSON event exceeds limit");return;}
  id value=[NSJSONSerialization JSONObjectWithData:[body subdataWithRange:NSMakeRange(1,body.length-1)] options:0 error:NULL];
  if(![value isKindOfClass:[NSDictionary class]]){fail(@"Invalid Guest event");return;}NSString *event=value[@"event"];
  if([event isEqual:@"started"]&&!started&&!draining&&[value[@"id"] isEqual:@"host-start"]&&unsignedNumber(value[@"width"],width)&&unsignedNumber(value[@"height"],height)&&[value[@"pixel_format"] isEqual:@"nv12-video-range"]&&[value[@"color_matrix"] isEqual:@"ITU-R BT.709"]&&[value[@"color_space"] isEqual:@"ITU-R BT.709"]&&[value[@"capture_dynamic_range"] isEqual:@"SDR"]){started=YES;diagnostic(@{@"event":@"guest-started",@"geometry":value});}
  else if([event isEqual:@"stopped"]&&draining&&[value[@"id"] isEqual:@"host-stop"]){stopped=YES;}
  else if([event isEqual:@"error"]&&[value[@"id"] isEqual:@"host-start"]&&[value[@"code"] isKindOfClass:[NSString class]]&&[value[@"code"] length]>0&&[value[@"code"] length]<=64&&!guestError&&!terminalReceived){
   NSMutableDictionary *safe=[@{@"code":value[@"code"]} mutableCopy];
   if([value[@"error_domain"] isKindOfClass:[NSString class]]&&[value[@"error_domain"] length]<=128)safe[@"error_domain"]=value[@"error_domain"];
   if([value[@"error_code"] isKindOfClass:[NSNumber class]])safe[@"error_code"]=value[@"error_code"];
   for(NSString *key in @[@"main_display_active",@"main_display_asleep"])if(value[key]&&CFGetTypeID((__bridge CFTypeRef)value[key])==CFBooleanGetTypeID())safe[key]=value[key];
   guestError=safe;if(!draining)fail(@"Guest capture failed; waiting for independent cleanup attestation");
  }
  else if([event isEqual:@"terminated"]&&draining&&guestError&&!terminalReceived&&[value[@"id"] isEqual:@"host-start"]&&[value[@"reason"] isEqual:@"capture_failed"]&&value[@"cleanup_confirmed"]&&CFGetTypeID((__bridge CFTypeRef)value[@"cleanup_confirmed"])==CFBooleanGetTypeID()&&[value[@"cleanup_confirmed"] boolValue]&&([value[@"cleanup_basis"] isEqual:@"no_stream_created"]||[value[@"cleanup_basis"] isEqual:@"stop_capture_completed"]||[value[@"cleanup_basis"] isEqual:@"delegate_reported_stopped"])){
   if(started&&[value[@"cleanup_basis"] isEqual:@"no_stream_created"]){fail(@"Guest cleanup basis contradicts started stream");return;}
   if([value[@"cleanup_basis"] isEqual:@"delegate_reported_stopped"]){
    id code=guestError[@"error_code"];
    if(![guestError[@"code"] isEqual:@"capture_stopped_with_error"]||![guestError[@"error_domain"] isEqual:@"com.apple.ScreenCaptureKit.SCStreamErrorDomain"]||![code isKindOfClass:[NSNumber class]]||CFGetTypeID((__bridge CFTypeRef)code)==CFBooleanGetTypeID()||[code doubleValue]!=[code longLongValue]){fail(@"Guest delegate cleanup lacks ScreenCaptureKit stop error evidence");return;}
   }
   id secondary=value[@"secondary_stop_error"];
   if([secondary isKindOfClass:[NSDictionary class]]&&[secondary[@"error_domain"] isKindOfClass:[NSString class]]&&[@[@"com.apple.ScreenCaptureKit.SCStreamErrorDomain",@"NSCocoaErrorDomain",@"NSOSStatusErrorDomain",@"NSPOSIXErrorDomain",@"unrecognized-domain"] containsObject:secondary[@"error_domain"]]&&[secondary[@"error_code"] isKindOfClass:[NSNumber class]])secondaryStopError=@{@"error_domain":secondary[@"error_domain"],@"error_code":secondary[@"error_code"]};
   terminalReceived=YES;cleanupBasis=value[@"cleanup_basis"]; // EOF still required.
  }
  else{fail(@"Guest error, unexpected event or start/stop contract mismatch");}return;
 }
 if(p[0]!=2||!started||body.length<29){fail(@"Unexpected Guest frame");return;}
 uint64_t seq=read64(p+1),pts=read64(p+9);uint32_t w=read32(p+17),h=read32(p+21),y=read32(p+25);
 // Header is kind + two u64 + four u32 = 33 bytes (including UV size).
 if(body.length<33){fail(@"Truncated Guest frame header");return;}uint32_t uv=read32(p+29);
 if(w!=width||h!=height||y!=w*h||uv!=w*h/2||body.length!=(size_t)33+y+uv||seq<=sourceSequence||(sourceSequence&&pts<=sourcePTS)){fail(@"Guest frame geometry, sequence or timestamp mismatch");return;}
 sourceSequence=seq;sourcePTS=pts;if(draining)return;
 if(!cachedPixel){NSDictionary *attrs=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
  if(CVPixelBufferCreate(NULL,width,height,kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,(__bridge CFDictionaryRef)attrs,&cachedPixel)!=kCVReturnSuccess){fail(@"Host NV12 pixel buffer allocation failed");return;}
  CVBufferSetAttachment(cachedPixel,kCVImageBufferColorPrimariesKey,kCVImageBufferColorPrimaries_ITU_R_709_2,kCVAttachmentMode_ShouldPropagate);
  CVBufferSetAttachment(cachedPixel,kCVImageBufferTransferFunctionKey,kCVImageBufferTransferFunction_ITU_R_709_2,kCVAttachmentMode_ShouldPropagate);
  CVBufferSetAttachment(cachedPixel,kCVImageBufferYCbCrMatrixKey,kCVImageBufferYCbCrMatrix_ITU_R_709_2,kCVAttachmentMode_ShouldPropagate);
  if(!createEncoder(cachedPixel))return;
  diagnostic(@{@"event":@"ready",@"hardware_verified":@YES,@"codec":codecName,@"width":@(width),@"height":@(height),@"pixel_format":@"nv12-video-range",@"timestamp_basis":@"host monotonic submission, first PTS zero",@"max_host_inflight_frames":@1,@"idle_fps":@1,@"stdout_timeout_ms":@500});
 }
 if(CVPixelBufferLockBaseAddress(cachedPixel,0)!=kCVReturnSuccess){fail(@"Host NV12 lock failed");return;}
 if(CVPixelBufferGetPlaneCount(cachedPixel)!=2||CVPixelBufferGetWidthOfPlane(cachedPixel,0)!=width||CVPixelBufferGetHeightOfPlane(cachedPixel,0)!=height||CVPixelBufferGetHeightOfPlane(cachedPixel,1)!=height/2||CVPixelBufferGetBytesPerRowOfPlane(cachedPixel,0)<width||CVPixelBufferGetBytesPerRowOfPlane(cachedPixel,1)<width||!CVPixelBufferGetBaseAddressOfPlane(cachedPixel,0)||!CVPixelBufferGetBaseAddressOfPlane(cachedPixel,1)){CVPixelBufferUnlockBaseAddress(cachedPixel,0);fail(@"Host NV12 plane layout mismatch");return;}
 for(size_t plane=0;plane<2;plane++){const uint8_t *src=p+33+(plane?y:0);uint8_t *dst=CVPixelBufferGetBaseAddressOfPlane(cachedPixel,plane);size_t stride=CVPixelBufferGetBytesPerRowOfPlane(cachedPixel,plane),rows=plane?height/2:height;
  for(size_t row=0;row<rows;row++)memcpy(dst+row*stride,src+row*width,width);
 }
 CVPixelBufferUnlockBaseAddress(cachedPixel,0);cpuCopies++;encodeCached(NO);
}
// A bounded parser retains at most one frame plus one 64KiB socket read.
static void consume(BOOL draining){
 if(terminalReceived&&incoming.length){fail(@"Bytes after Guest terminal cleanup event");return;}
 while(incoming.length>=4){const uint8_t *p=incoming.bytes;uint32_t len=read32(p);if(len<1||len>33+1920*1080*3/2){fail(@"Guest record length outside bound");return;}if(incoming.length<(size_t)len+4)return;
  NSData *body=[incoming subdataWithRange:NSMakeRange(4,len)];[incoming replaceBytesInRange:NSMakeRange(0,len+4) withBytes:NULL length:0];record(body,draining);if(terminalReceived&&incoming.length)fail(@"Bytes after Guest terminal cleanup event");if(failure||stopped||terminalReceived)return;
 }
}
static BOOL readSource(BOOL draining){
 uint8_t bytes[65536];ssize_t n=read(sourceFD,bytes,sizeof(bytes));if(n>0){[incoming appendBytes:bytes length:(NSUInteger)n];consume(draining);return YES;}
 if(n<0&&(errno==EINTR||errno==EAGAIN||errno==EWOULDBLOCK))return YES;
 if(n==0&&terminalReceived&&incoming.length==0&&!failure){stopped=YES;cleanCaptureFailure=YES;return NO;}
 if(!stopped)fail(@"Guest stream closed without completed stop acknowledgement");return NO;
}
static int connectPrivate(const char *path){
 NSString *name=[NSString stringWithUTF8String:path];struct stat parent,endpoint;
 if(![name isAbsolutePath]||lstat(name.stringByDeletingLastPathComponent.fileSystemRepresentation,&parent)||!S_ISDIR(parent.st_mode)||parent.st_uid!=getuid()||(parent.st_mode&0777)!=0700||lstat(path,&endpoint)||!S_ISSOCK(endpoint.st_mode)||endpoint.st_uid!=getuid()||(endpoint.st_mode&0777)!=0600){fail(@"Private current-user 0700 directory and 0600 socket required");return -1;}
 struct sockaddr_un addr={0};addr.sun_family=AF_UNIX;if(strlen(path)>=sizeof(addr.sun_path)){fail(@"Unix socket path too long");return -1;}strcpy(addr.sun_path,path);
 int fd=socket(AF_UNIX,SOCK_STREAM,0);if(fd<0){fail(@"socket failed");return -1;}if(fcntl(fd,F_SETFD,FD_CLOEXEC)<0||fcntl(fd,F_SETFL,O_NONBLOCK)<0){close(fd);fail(@"Cannot configure bounded nonblocking Guest socket");return -1;}
 if(connect(fd,(struct sockaddr *)&addr,sizeof(addr))<0){if(errno!=EINPROGRESS){close(fd);fail(@"Guest bridge connect failed");return -1;}struct pollfd f={fd,POLLOUT,0};int error=0;socklen_t length=sizeof(error);if(poll(&f,1,3000)<=0||getsockopt(fd,SOL_SOCKET,SO_ERROR,&error,&length)||error){close(fd);fail(@"Guest bridge connect deadline/error");return -1;}}
 uid_t uid;gid_t gid;if(getpeereid(fd,&uid,&gid)||uid!=getuid()){close(fd);fail(@"Guest bridge peer UID mismatch");return -1;}return fd;
}
int main(int argc,char **argv){@autoreleasepool{
 lock=[NSLock new];if(argc==2&&!strcmp(argv[1],"--probe-codecs"))return probeCodecs();
 if(argc==2&&!strcmp(argv[1],"--capabilities")){puts("{\"protocol\":\"veynode-guest-video-stream-v1\",\"cleanup_contract_version\":1,\"cleanup_confirmed_failure_exit\":78}");return 0;}
 if(argc!=4||strcmp(argv[2],"--codec")||((strcmp(argv[3],"h264")!=0)&&(strcmp(argv[3],"hevc")!=0))){fprintf(stderr,"Usage: guest-video-stream PRIVATE_UNIX_SOCKET --codec h264|hevc\n");return 2;}
 codecName=[NSString stringWithUTF8String:argv[3]];useHEVC=[codecName isEqual:@"hevc"];width=useHEVC?1920:1280;height=useHEVC?1080:720;
 signal(SIGPIPE,SIG_IGN);int outputFlags=fcntl(STDOUT_FILENO,F_GETFL),inputFlags=fcntl(STDIN_FILENO,F_GETFL);if(outputFlags<0||inputFlags<0||fcntl(STDOUT_FILENO,F_SETFL,outputFlags|O_NONBLOCK)<0||fcntl(STDIN_FILENO,F_SETFL,inputFlags|O_NONBLOCK)<0){diagnostic(@{@"event":@"error",@"error":@"Cannot configure bounded nonblocking standard pipes",@"guest_cleanup_confirmed":@NO});return 1;}
 commands=[NSMutableData new];incoming=[NSMutableData new];sourceFD=connectPrivate(argv[1]);
 if(sourceFD>=0&&!failure&&sendCommand(@{@"id":@"host-start",@"method":@"start",@"width":@(width),@"height":@(height),@"fps":@30})){
  int64_t begin=nowUs();while(!failure&&!stopping){
   struct pollfd f[2]={{sourceFD,POLLIN,0},{STDIN_FILENO,POLLIN,0}};int ready=poll(f,2,50);if(ready<0&&errno!=EINTR){fail(@"poll failed");break;}
   if(f[1].revents&(POLLIN|POLLHUP|POLLERR))readCommands();if(stopping)break;
   if(f[0].revents&(POLLIN|POLLHUP|POLLERR)){if(!readSource(NO))break;}
   if(!submitted&&nowUs()-begin>10000000){fail(@"Guest first frame timeout");break;}
   if(cachedPixel&&(forceKeyframe||nowUs()-lastSubmit>=1000000))encodeCached(YES);
  }
  // Stop is an explicit protocol acknowledgement; EOF alone never attests cleanup.
  stopping=YES;NSString *original=failure;failure=nil;
  if(guestError||sendCommand(@{@"id":@"host-stop",@"method":@"stop"})){
   int64_t end=nowUs()+1500000;while(!failure&&!stopped&&nowUs()<end){consume(YES);if(stopped||failure)break;struct pollfd f={sourceFD,POLLIN,0};int r=poll(&f,1,50);if(r>0&&!readSource(YES))break;}
  }
  if(!stopped&&!failure)fail(@"Guest stop acknowledgement exceeded 1500ms");if(original)failure=original;else if(guestError)fail(@"Guest capture failed");
 }
 if(encoder){VTCompressionSessionInvalidate(encoder);CFRelease(encoder);}if(cachedPixel)CVPixelBufferRelease(cachedPixel);if(sourceFD>=0)close(sourceFD);
 diagnostic(@{@"event":failure?@"error":@"stopped",@"error":failure?:[NSNull null],@"guest_error":guestError?:[NSNull null],@"secondary_stop_error":secondaryStopError?:[NSNull null],@"cleanup_basis":cleanupBasis?:[NSNull null],@"guest_cleanup_confirmed":stopped?@YES:@NO,@"hardware_verified":hardwareVerified?@YES:@NO,@"codec":codecName,@"width":@(width),@"height":@(height),@"source_sequence":@(sourceSequence),@"source_pts_ns":@(sourcePTS),@"submitted":@(submitted),@"encoded":@(encoded),@"keyframes":@(keyframes),@"host_cpu_plane_copies":@(cpuCopies),@"idle_refresh_frames":@(idleFrames)});
 return cleanCaptureFailure&&failure?78:(failure||!stopped?1:0);
}}
