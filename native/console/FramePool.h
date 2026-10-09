#import <Foundation/Foundation.h>
#import <CoreVideo/CoreVideo.h>

// Exactly three actual CVPixelBuffer storage slots. Each logical lease pins its
// slot against reuse. Capture, latest publication and display GPU hold distinct
// leases; dropping latest publication never releases an in-flight GPU lease.
@interface VNFramePool : NSObject
- (CVPixelBufferRef)acquireWidth:(NSUInteger)width height:(NSUInteger)height;
- (BOOL)retainPixel:(CVPixelBufferRef)pixel;
- (void)releasePixel:(CVPixelBufferRef)pixel;
- (NSDictionary *)statistics;
@end
