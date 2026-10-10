#import <Foundation/Foundation.h>
@class VNDisplayedFrameStore;
BOOL VNFrameUnsignedInteger(id value,BOOL allowZero);
BOOL VNFrameReceiptIdentifier(id value);
// Caller holds publication lock for issue/consume. Never calls back into control
// or performs I/O. Lock order: publication/condition -> this registry only.
@interface VNFrameAcquisition : NSObject
- (void)activateEpoch:(uint64_t)epoch generation:(uint64_t)generation;
- (void)deactivate;
// Synchronous validation-admission focus check; never activates or focuses UI.
- (BOOL)requireFocusReady:(BOOL)ready applicationActive:(BOOL)active windowKey:(BOOL)key firstResponder:(BOOL)responder;
- (void)invalidateFrames;
- (NSString *)issueFromStore:(VNDisplayedFrameStore *)store context:(NSDictionary *)context now:(double)now;
- (BOOL)claim:(NSString *)receipt token:(NSDictionary *)token epoch:(uint64_t)epoch sequence:(uint64_t)sequence generation:(uint64_t)generation now:(double)now;
- (NSDictionary *)consume:(NSString *)receipt token:(NSDictionary *)token epoch:(uint64_t)epoch sequence:(uint64_t)sequence store:(VNDisplayedFrameStore *)store now:(double)now failure:(NSString **)failure;
- (void)cancel:(NSString *)receipt;
- (NSDictionary *)statistics;
@end
