#import <Foundation/Foundation.h>
// Main-thread-owned bounded measurement policy; no synthetic GPU/presentation
// claims. Production values must originate in MTLDrawable's presented callback.
@interface VNPresentedSamples : NSObject
- (void)recordTime:(double)time generation:(uint64_t)generation sequence:(uint64_t)sequence windowEpoch:(uint64_t)windowEpoch;
- (NSDictionary *)statistics;
@end
