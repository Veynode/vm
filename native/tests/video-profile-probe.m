// Host-only actual VideoToolbox profile output comparison. Not Guest validation.
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
static NSString *actual;
static OSStatus callbackStatus;
static void output(void *a,void *b,OSStatus status,VTEncodeInfoFlags flags,CMSampleBufferRef sample){
 callbackStatus=status;if(status!=noErr||!sample||(flags&kVTEncodeInfo_FrameDropped))return;
 const uint8_t *p=NULL;size_t size=0;
 callbackStatus=CMVideoFormatDescriptionGetH264ParameterSetAtIndex(CMSampleBufferGetFormatDescription(sample),0,&p,&size,NULL,NULL);
 if(callbackStatus==noErr&&size>=4)actual=[NSString stringWithFormat:@"%02x%02x%02x",p[1],p[2],p[3]];
}
int main(void){@autoreleasepool{
 NSMutableArray *records=[NSMutableArray new];
 for(NSString *profile in @[(__bridge NSString *)kVTProfileLevel_H264_Baseline_3_1,(__bridge NSString *)kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel]){
  actual=nil;callbackStatus=0;VTCompressionSessionRef encoder=NULL;CVPixelBufferRef pixel=NULL;
  NSDictionary *spec=@{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:@YES};
  OSStatus status=VTCompressionSessionCreate(NULL,1152,720,kCMVideoCodecType_H264,(__bridge CFDictionaryRef)spec,NULL,NULL,output,NULL,&encoder);
  BOOL hardware=NO;
  if(status==noErr){NSDictionary *props=@{(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:profile,(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:@30,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@30};
   for(NSString *key in props){status=VTSessionSetProperty(encoder,(__bridge CFStringRef)key,(__bridge CFTypeRef)props[key]);if(status!=noErr)break;}
   if(status==noErr)status=VTCompressionSessionPrepareToEncodeFrames(encoder);
   CFTypeRef value=NULL;if(status==noErr){status=VTSessionCopyProperty(encoder,kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,NULL,&value);hardware=value&&CFGetTypeID(value)==CFBooleanGetTypeID()&&CFBooleanGetValue(value);if(value)CFRelease(value);}
   NSDictionary *attrs=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
   if(status==noErr)status=CVPixelBufferCreate(NULL,1152,720,kCVPixelFormatType_32BGRA,(__bridge CFDictionaryRef)attrs,&pixel);
   if(status==noErr){CVPixelBufferLockBaseAddress(pixel,0);memset(CVPixelBufferGetBaseAddress(pixel),80,CVPixelBufferGetBytesPerRow(pixel)*720);CVPixelBufferUnlockBaseAddress(pixel,0);status=VTCompressionSessionEncodeFrame(encoder,pixel,kCMTimeZero,CMTimeMake(1,30),NULL,NULL,NULL);}
   if(status==noErr)status=VTCompressionSessionCompleteFrames(encoder,kCMTimeInvalid);
  }
  [records addObject:@{@"requested_profile":profile,@"status":@(status),@"callback_status":@(callbackStatus),@"hardware_verified":@(hardware),@"actual_sps_profile_level_id":actual?:[NSNull null],@"scope":@"Host-only single1152x720 frame; not VM acceptance"}];
  if(pixel)CVPixelBufferRelease(pixel);if(encoder){VTCompressionSessionInvalidate(encoder);CFRelease(encoder);}
 }
 NSData *json=[NSJSONSerialization dataWithJSONObject:records options:NSJSONWritingPrettyPrinted error:nil];fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return 0;
}}
