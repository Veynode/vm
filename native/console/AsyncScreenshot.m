#import "AsyncScreenshot.h"
#import "ScreenshotDiagnostic.h"
#import <ImageIO/ImageIO.h>
#import <CoreGraphics/CoreGraphics.h>
#include <time.h>
static double clockSeconds(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
@interface VNBoundedPNG : NSObject
@property NSMutableData *bytes;
@property BOOL overflow;
@end
@implementation VNBoundedPNG
@end
static size_t putPNG(void *info,const void *bytes,size_t count){
 VNBoundedPNG *sink=(__bridge VNBoundedPNG *)info;
 if(sink.overflow||count>VEYNODE_SCREENSHOT_MAX_BYTES-sink.bytes.length){sink.overflow=YES;return 0;}
 [sink.bytes appendBytes:bytes length:count];return count;
}
#ifdef VEYNODE_SCREENSHOT_TESTING
static void (^testBarrier)(void);
#endif
@implementation VNAsyncScreenshot {
 NSLock *_lock; dispatch_group_t _group; BOOL _invalid; NSDictionary *_result;
}
#ifdef VEYNODE_SCREENSHOT_TESTING
+ (void)setTestBarrier:(void (^)(void))barrier {testBarrier=[barrier copy];}
#endif
- (instancetype)initWithPixel:(CVPixelBufferRef)pixel metadata:(NSDictionary *)metadata requestID:(NSString *)requestID release:(void (^)(void))release {
 if((self=[super init])){
  _lock=[NSLock new];_group=dispatch_group_create();NSDictionary *frozen=[metadata copy];NSString *ident=[requestID copy];
#ifdef VEYNODE_SCREENSHOT_TESTING
  void (^barrier)(void)=[testBarrier copy];
#endif
  dispatch_group_async(_group,dispatch_get_global_queue(QOS_CLASS_USER_INITIATED,0),^{@autoreleasepool{
   double began=clockSeconds(),copied=began,encoded=began;NSData *pixels=nil,*body=nil;NSString *error=nil;NSNumber *encodedBytes=nil;size_t row=0,w=0,h=0;
   @try {
#ifdef VEYNODE_SCREENSHOT_TESTING
    if(barrier)barrier();
#endif
    w=CVPixelBufferGetWidth(pixel);h=CVPixelBufferGetHeight(pixel);row=CVPixelBufferGetBytesPerRow(pixel);
    if(!w||!h||w>3840||h>2160||w%2||h%2||row<w*4||row>16384||h>SIZE_MAX/row||row*h>CVPixelBufferGetDataSize(pixel)||CVPixelBufferGetPixelFormatType(pixel)!=kCVPixelFormatType_32BGRA){error=@"invalid_pixel_geometry";}
    else if(CVPixelBufferLockBaseAddress(pixel,kCVPixelBufferLock_ReadOnly)!=kCVReturnSuccess){error=@"pixel_lock_failed";}
    else{
     @try {pixels=[NSData dataWithBytes:CVPixelBufferGetBaseAddress(pixel) length:row*h];}
     @finally {CVPixelBufferUnlockBaseAddress(pixel,kCVPixelBufferLock_ReadOnly);}
    }
   } @catch(NSException *exception){(void)exception;error=@"pixel_copy_failed";}
   @finally {release();copied=clockSeconds();}
   [self->_lock lock];BOOL cancelled=self->_invalid;[self->_lock unlock];
   if(cancelled)error=@"cancelled";
   @try {if(!error){
    VNBoundedPNG *sink=[VNBoundedPNG new];sink.bytes=[NSMutableData new];
    CGDataProviderRef provider=NULL;CGColorSpaceRef color=NULL;CGImageRef image=NULL;CGDataConsumerRef consumer=NULL;CGImageDestinationRef destination=NULL;BOOL okay=NO;
    @try {
     provider=CGDataProviderCreateWithCFData((__bridge CFDataRef)pixels);color=CGColorSpaceCreateDeviceRGB();
     image=provider&&color?CGImageCreate(w,h,8,32,row,color,kCGBitmapByteOrder32Little|kCGImageAlphaNoneSkipFirst,provider,NULL,false,kCGRenderingIntentDefault):NULL;
     CGDataConsumerCallbacks callbacks={putPNG,NULL};consumer=CGDataConsumerCreate((__bridge void *)sink,&callbacks);
     destination=consumer?CGImageDestinationCreateWithDataConsumer(consumer,CFSTR("public.png"),1,NULL):NULL;
     if(image&&destination){CGImageDestinationAddImage(destination,image,NULL);okay=CGImageDestinationFinalize(destination);}
    } @finally {
     if(destination)CFRelease(destination);if(consumer)CFRelease(consumer);if(image)CFRelease(image);if(color)CGColorSpaceRelease(color);if(provider)CGDataProviderRelease(provider);
    }
    if(!sink.overflow)encodedBytes=@(sink.bytes.length);
    encoded=clockSeconds();
    if(sink.overflow)error=@"encoded_size_limit";else if(!okay)error=@"png_finalize_failed";
    else{
     NSMutableDictionary *result=[frozen mutableCopy];result[@"data_base64"]=[sink.bytes base64EncodedStringWithOptions:0];result[@"byte_length"]=@(sink.bytes.length);
     body=VNScreenshotResponseData(ident,result);if(!body)error=@"json_serialization_failed";
    }
   }
   } @catch(NSException *exception){(void)exception;error=@"png_worker_failed";body=nil;}
   double finished=clockSeconds();
   [self->_lock lock];if(self->_invalid)error=@"cancelled";
   self->_result=@{@"error":error?:[NSNull null],@"body":(!error&&body)?body:[NSData data],@"metadata":frozen,@"diagnostic":error?VNScreenshotFailure(error,encodedBytes):[NSNull null],@"timing":@{@"copy_us":@((copied-began)*1e6),@"png_us":@(MAX(0,encoded-copied)*1e6),@"serialize_us":@(MAX(0,finished-MAX(copied,encoded))*1e6),@"worker_total_us":@((finished-began)*1e6),@"worker_started_monotonic_us":@(began*1e6),@"copy_completed_monotonic_us":@(copied*1e6),@"png_completed_monotonic_us":@(MAX(copied,encoded)*1e6),@"worker_completed_monotonic_us":@(finished*1e6)}};
   [self->_lock unlock];
  }});
 }return self;
}
- (void)invalidate {[_lock lock];_invalid=YES;[_lock unlock];}
- (NSDictionary *)resultIfFinished {if(dispatch_group_wait(_group,DISPATCH_TIME_NOW)!=0)return nil;[_lock lock];NSDictionary *result=_result;[_lock unlock];return result;}
- (BOOL)waitFor:(double)seconds {return dispatch_group_wait(_group,dispatch_time(DISPATCH_TIME_NOW,(int64_t)(seconds*NSEC_PER_SEC)))==0;}
@end
