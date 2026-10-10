#import <Foundation/Foundation.h>
#import <CoreVideo/CoreVideo.h>

// Caller transfers one real immutable-frame lease. Release runs exactly once
// after CPU snapshot copying (or failure), not merely after a timeout flag.
@interface VNAsyncScreenshot : NSObject
- (instancetype)initWithPixel:(CVPixelBufferRef)pixel metadata:(NSDictionary *)metadata requestID:(NSString *)requestID release:(void (^)(void))release;
- (NSDictionary *)resultIfFinished;
- (void)invalidate;
- (BOOL)waitFor:(double)seconds;
#ifdef VEYNODE_SCREENSHOT_TESTING
+ (void)setTestBarrier:(void (^)(void))barrier;
#endif
@end
