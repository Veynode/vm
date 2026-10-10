#import <Foundation/Foundation.h>
// Finite rolling aggregates; no frame identity, pixel data or presentation claim.
typedef NS_ENUM(NSUInteger, VNCopyKind) { VNCopySource = 0, VNCopyEncoderResize = 1 };
@interface VNCopyTiming : NSObject
- (void)recordKind:(VNCopyKind)kind cpuSeconds:(double)cpu commitWaitSeconds:(double)wait gpuStartSeconds:(double)start gpuEndSeconds:(double)end succeeded:(BOOL)success;
- (NSDictionary *)statistics;
@end
