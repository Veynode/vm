#import <Foundation/Foundation.h>
#include <stdint.h>

// Passive, default-off draw timing observation (see draw-timing-diagnostic.md).
// Every method is a no-op when disabled. Uses its own lock and never takes
// publicationLock, so it cannot change draw, ACK, pacing, inflight or input decisions.
typedef NS_ENUM(NSUInteger, VNDrawBranch) {
    VNDrawBranchInflight=0,
    VNDrawBranchNoWindow,
    VNDrawBranchInvisible,
    VNDrawBranchUnchangedSource,
    VNDrawBranchNoDrawableOrPass,
    VNDrawBranchCommandOrEncoderFailed,
    VNDrawBranchTextureFailed,
    VNDrawBranchRetiredGeneration,
    VNDrawBranchSubmittedFrame,
    VNDrawBranchSubmittedClear,
    VNDrawBranchCount
};

typedef NS_ENUM(NSUInteger, VNDrawCompletionGate) {
    VNDrawGateApplied=0,
    VNDrawGateEpochChanged,
    VNDrawGateRevisionChanged,
    VNDrawGateCommandFailed,
    VNDrawGateCount
};

@interface VNDrawTiming : NSObject
- (instancetype)initWithEnabled:(BOOL)enabled;
- (BOOL)enabled;
// Exactly one of these per drawInMTKView: call, after admission has been decided:
// early returns use recordCallbackBranch:; attempts use attempt:terminal: or attempt:committedAt:clear:.
- (void)recordCallbackBranch:(VNDrawBranch)branch;
// Called after drawable/pass acquisition. Pass width/height 0 when no drawable. Returns 0 when disabled.
- (uint64_t)admitAttemptWithEntry:(double)entry acquireBegin:(double)acquireBegin acquireEnd:(double)acquireEnd windowEpoch:(uint64_t)windowEpoch generation:(uint64_t)generation sequence:(uint64_t)sequence revision:(uint64_t)revision drawableWidth:(NSUInteger)width drawableHeight:(NSUInteger)height clear:(BOOL)clear leaseLost:(BOOL)leaseLost;
// Non-submitted terminals only: NoDrawableOrPass, CommandOrEncoderFailed, TextureFailed, RetiredGeneration.
- (void)attempt:(uint64_t)attemptID terminal:(VNDrawBranch)branch;
- (void)attempt:(uint64_t)attemptID committedAt:(double)commitEnd clear:(BOOL)clear;
// Metal completion handler thread, before dispatch to main.
- (void)attempt:(uint64_t)attemptID completionEnteredAt:(double)entered status:(NSInteger)status gpuStart:(double)gpuStart gpuEnd:(double)gpuEnd;
// Main thread block that clears inflight.
- (void)attempt:(uint64_t)attemptID inflightClearedAt:(double)cleared gate:(VNDrawCompletionGate)gate;
// Metal presented handler thread, before dispatch to main.
- (void)attempt:(uint64_t)attemptID presentedEnteredAt:(double)entered metalPresented:(double)presented;
// Main thread block that records the presentation sample.
- (void)attempt:(uint64_t)attemptID presentedMainAt:(double)recorded;
- (NSDictionary *)statistics;
@end
