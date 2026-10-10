// CPU ImageIO and actual three-slot frame pool only: no Metal device or VM.
#import "AsyncScreenshot.h"
#import "FramePool.h"
#import "ScreenshotDiagnostic.h"
#import <ImageIO/ImageIO.h>
#define CHECK(x) do{if(!(x)){fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#x);exit(1);}}while(0)
static void paint(CVPixelBufferRef p,uint8_t value){CHECK(CVPixelBufferLockBaseAddress(p,0)==0);memset(CVPixelBufferGetBaseAddress(p),value,CVPixelBufferGetBytesPerRow(p)*CVPixelBufferGetHeight(p));CVPixelBufferUnlockBaseAddress(p,0);}
static NSDictionary *meta(void){return @{@"source_generation":@7,@"source_sequence":@8,@"captured_at_us":@900,@"captured_monotonic_us":@800,@"width":@64,@"height":@64,@"mime_type":@"image/png"};}
static void actualPixelsAndLeases(void){
 VNFramePool *pool=[VNFramePool new];CVPixelBufferRef a=[pool acquireWidth:64 height:64];paint(a,31);CHECK([pool retainPixel:a]);
 dispatch_semaphore_t entered=dispatch_semaphore_create(0),resume=dispatch_semaphore_create(0);
 [VNAsyncScreenshot setTestBarrier:^{dispatch_semaphore_signal(entered);dispatch_semaphore_wait(resume,DISPATCH_TIME_FOREVER);}];
 VNAsyncScreenshot *job=[[VNAsyncScreenshot alloc]initWithPixel:a metadata:meta() requestID:@"frozen" release:^{[pool releasePixel:a];}];
 CHECK(dispatch_semaphore_wait(entered,dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC))==0);
 [pool releasePixel:a]; // retire latest publication, leaving actual screenshot lease
 CVPixelBufferRef b=[pool acquireWidth:64 height:64],c=[pool acquireWidth:64 height:64];CHECK(b&&c&&b!=a&&c!=a);paint(b,240);paint(c,180);CHECK(![pool acquireWidth:64 height:64]);CHECK(![job waitFor:.01]);
 dispatch_semaphore_signal(resume);CHECK([job waitFor:2]);[VNAsyncScreenshot setTestBarrier:nil];NSDictionary *r=job.resultIfFinished;CHECK(r[@"error"]==NSNull.null);
 NSDictionary *wire=[NSJSONSerialization JSONObjectWithData:r[@"body"] options:0 error:nil];CHECK([wire[@"id"] isEqual:@"frozen"]);NSDictionary *result=wire[@"result"];CHECK([result[@"source_sequence"] isEqual:@8]&&[result[@"captured_monotonic_us"] isEqual:@800]);
 NSData *png=[[NSData alloc]initWithBase64EncodedString:result[@"data_base64"] options:0];CGImageSourceRef source=CGImageSourceCreateWithData((__bridge CFDataRef)png,NULL);CHECK(source);CGImageRef image=CGImageSourceCreateImageAtIndex(source,0,NULL);CHECK(image&&CGImageGetWidth(image)==64&&CGImageGetHeight(image)==64);
 uint8_t decoded[64*64*4];CGColorSpaceRef color=CGColorSpaceCreateDeviceRGB();CGContextRef context=CGBitmapContextCreate(decoded,64,64,8,256,color,(CGBitmapInfo)kCGImageAlphaNoneSkipLast);CHECK(context);CGContextDrawImage(context,CGRectMake(0,0,64,64),image);CHECK(decoded[0]==31&&decoded[1]==31&&decoded[2]==31);CGContextRelease(context);CGColorSpaceRelease(color);CGImageRelease(image);CFRelease(source);
 NSDictionary *t=r[@"timing"];double previous=0;for(NSString *key in @[@"worker_started_monotonic_us",@"copy_completed_monotonic_us",@"png_completed_monotonic_us",@"worker_completed_monotonic_us"]){double value=[t[key] doubleValue];CHECK(value>=previous);previous=value;}
 [pool releasePixel:b];[pool releasePixel:c];CHECK([pool.statistics[@"leases"] unsignedIntegerValue]==0);puts("PASS frozen actual pixels, real lease prevents slot reuse, original metadata and monotonic stages");
}
static void cancellationAndException(void){
 for(int mode=0;mode<2;mode++){
  VNFramePool *pool=[VNFramePool new];CVPixelBufferRef p=[pool acquireWidth:64 height:64];paint(p,0);dispatch_semaphore_t entered=dispatch_semaphore_create(0),resume=dispatch_semaphore_create(0);
  [VNAsyncScreenshot setTestBarrier:^{dispatch_semaphore_signal(entered);dispatch_semaphore_wait(resume,DISPATCH_TIME_FOREVER);if(mode==1)@throw [NSException exceptionWithName:@"test" reason:@"private sentinel" userInfo:nil];}];
  VNAsyncScreenshot *job=[[VNAsyncScreenshot alloc]initWithPixel:p metadata:meta() requestID:@"cancel" release:^{[pool releasePixel:p];}];CHECK(dispatch_semaphore_wait(entered,dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC))==0);
  if(mode==0)[job invalidate];CHECK(![job waitFor:.01]);CHECK([pool.statistics[@"leases"] unsignedIntegerValue]==1);dispatch_semaphore_signal(resume);CHECK([job waitFor:2]);NSDictionary *r=job.resultIfFinished;CHECK([r[@"error"] isEqual:(mode?@"pixel_copy_failed":@"cancelled")]);CHECK([r[@"body"] length]==0);CHECK([pool.statistics[@"leases"] unsignedIntegerValue]==0);
 }
 [VNAsyncScreenshot setTestBarrier:nil];puts("PASS cancellation and actual exception settle only after lease release; no private exception text");
}
static void boundedPNG(void){
 VNFramePool *pool=[VNFramePool new];CVPixelBufferRef p=[pool acquireWidth:1280 height:800];CHECK(p);CHECK(CVPixelBufferLockBaseAddress(p,0)==0);uint8_t *bytes=CVPixelBufferGetBaseAddress(p);uint32_t state=1234567;for(size_t i=0;i<CVPixelBufferGetBytesPerRow(p)*800;i++){state^=state<<13;state^=state>>17;state^=state<<5;bytes[i]=(uint8_t)state;}CVPixelBufferUnlockBaseAddress(p,0);
 VNAsyncScreenshot *job=[[VNAsyncScreenshot alloc]initWithPixel:p metadata:meta() requestID:@"limit" release:^{[pool releasePixel:p];}];CHECK([job waitFor:4]);NSDictionary *r=job.resultIfFinished;CHECK([r[@"error"] isEqual:@"encoded_size_limit"]);CHECK([r[@"body"] length]==0);CHECK(r[@"diagnostic"][@"encoded_bytes"]==NSNull.null);CHECK([pool.statistics[@"leases"] unsignedIntegerValue]==0);puts("PASS actual incompressible PNG bounded sink overflow, no invented final length");
}
int main(void){@autoreleasepool{actualPixelsAndLeases();cancellationAndException();boundedPNG();}return 0;}
