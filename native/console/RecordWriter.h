#import <Foundation/Foundation.h>

// One framed stdout owner. Queue budgets include the currently writing record.
@interface VNWriteReceipt : NSObject
- (NSDictionary *)status;
@end
@interface VNRecordWriter : NSObject
- (instancetype)initWithFD:(int)fd;
- (VNWriteReceipt *)enqueueKind:(uint8_t)kind epoch:(uint64_t)epoch body:(NSData *)body;
- (BOOL)failed;
- (NSUInteger)pending;
- (BOOL)drainFor:(double)seconds;
- (void)invalidate;
@end
