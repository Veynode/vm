#import "DrawTiming.h"
#include <math.h>
#include <string.h>

#define VN_DRAW_CAPACITY 32

// Numbers only. Unreached stages are NAN and export as null, never zero.
typedef struct {
    uint64_t attemptID,submissionID,windowEpoch,generation,sequence,revision;
    NSUInteger drawableWidth,drawableHeight;
    BOOL leaseLost,clear,hasCommandStatus;
    int terminal;   // -1 pending, otherwise a VNDrawBranch
    int gate;       // -1 unset, otherwise a VNDrawCompletionGate
    NSInteger commandStatus;
    double entry,acquireBegin,acquireEnd,commitEnd,completionEntered,inflightCleared;
    double presentedEntered,presentedMain,gpuStart,gpuEnd,metalPresented;
} VNDrawAttempt;

static NSString *const branchNames[VNDrawBranchCount]={@"inflight",@"no_window",@"invisible",@"unchanged_source",@"no_drawable_or_pass",@"command_or_encoder_failed",@"texture_failed",@"retired_generation",@"submitted_frame",@"submitted_clear"};
static NSString *const gateNames[VNDrawGateCount]={@"applied",@"epoch_changed",@"revision_changed",@"command_failed"};

static double cpuStamp(double seconds,uint64_t *invalid){if(isfinite(seconds)&&seconds>0)return seconds;(*invalid)++;return NAN;}
static double rawMetal(double seconds,uint64_t *invalid){if(isfinite(seconds)&&seconds!=0)return seconds;(*invalid)++;return isfinite(seconds)?seconds:NAN;}

static BOOL attemptFinal(const VNDrawAttempt *a){
    if(a->terminal<0)return NO;
    if(a->terminal!=VNDrawBranchSubmittedFrame&&a->terminal!=VNDrawBranchSubmittedClear)return YES;
    if(isnan(a->inflightCleared))return NO;
    return !isnan(a->presentedMain)||a->gate==VNDrawGateCommandFailed;
}

static id numberOrNull(double value){return isnan(value)?NSNull.null:@(value);}
static id microsecondSeconds(double seconds){return isnan(seconds)?NSNull.null:@(round(seconds*1e6)/1e6);}
static id millisecondsBetween(double start,double end){return (isnan(start)||isnan(end))?NSNull.null:@(round((end-start)*1e6)/1e3);}

static NSDictionary *attemptRecord(const VNDrawAttempt *a){
    NSString *terminal=a->terminal<0?@"pending":branchNames[a->terminal];
    id gate=a->gate<0?NSNull.null:gateNames[a->gate];
    id drawable=(a->drawableWidth||a->drawableHeight)?@[@(a->drawableWidth),@(a->drawableHeight)]:NSNull.null;
    return @{
        @"attempt_id":@(a->attemptID),
        @"submission_id":a->submissionID?@(a->submissionID):NSNull.null,
        @"terminal":terminal,
        @"final":@(attemptFinal(a)),
        @"window_epoch":@(a->windowEpoch),
        @"generation":@(a->generation),
        @"sequence":@(a->sequence),
        @"revision":@(a->revision),
        @"drawable":drawable,
        @"lease_lost":@(a->leaseLost),
        @"entry_s":microsecondSeconds(a->entry),
        @"pre_ms":millisecondsBetween(a->entry,a->acquireBegin),
        @"wait_ms":millisecondsBetween(a->acquireBegin,a->acquireEnd),
        @"submit_ms":millisecondsBetween(a->acquireEnd,a->commitEnd),
        @"gpu_entry_ms":millisecondsBetween(a->commitEnd,a->completionEntered),
        @"inflight_ms":millisecondsBetween(a->completionEntered,a->inflightCleared),
        @"pres_entry_ms":millisecondsBetween(a->commitEnd,a->presentedEntered),
        @"pres_main_ms":millisecondsBetween(a->presentedEntered,a->presentedMain),
        @"status":a->hasCommandStatus?@(a->commandStatus):NSNull.null,
        @"gate":gate,
        @"gpu_start_s":numberOrNull(a->gpuStart),
        @"gpu_end_s":numberOrNull(a->gpuEnd),
        @"pres_s":numberOrNull(a->metalPresented)
    };
}

@implementation VNDrawTiming {
    NSLock *_lock;
    BOOL _enabled;
    VNDrawAttempt _ring[VN_DRAW_CAPACITY];
    NSUInteger _count,_head;
    uint64_t _callbacks,_branches[VNDrawBranchCount];
    uint64_t _attempts,_submissions,_evicted,_pendingEvicted,_lateUpdates,_leaseLost;
    uint64_t _invalidCPU,_invalidMetalGPU,_invalidMetalPresented;
    uint64_t _completed,_failed,_gates[VNDrawGateCount];
    double _lastMetalPresented;
}
- (instancetype)initWithEnabled:(BOOL)enabled{
    if((self=[super init])){_enabled=enabled;_lock=[NSLock new];_lastMetalPresented=0;}
    return self;
}
- (BOOL)enabled{return _enabled;}
// Callers hold _lock.
- (void)lockedCountBranch:(VNDrawBranch)branch{_callbacks++;_branches[branch]++;}
- (VNDrawAttempt *)lockedAttempt:(uint64_t)attemptID{
    if(!attemptID)return NULL;
    for(NSUInteger i=0;i<_count;i++){VNDrawAttempt *a=&_ring[(_head+i)%VN_DRAW_CAPACITY];if(a->attemptID==attemptID)return a;}
    return NULL;
}
// Stage callbacks for an attempt that left the ring are counted and never reconstructed.
- (VNDrawAttempt *)lockedStageAttempt:(uint64_t)attemptID{
    VNDrawAttempt *a=[self lockedAttempt:attemptID];
    if(!a)_lateUpdates++;
    return a;
}
- (void)recordCallbackBranch:(VNDrawBranch)branch{
    // Only the four callback-only branches are valid here; attempt terminals carry their own record.
    if(!_enabled||branch>VNDrawBranchUnchangedSource)return;
    [_lock lock];[self lockedCountBranch:branch];[_lock unlock];
}
// Presented stages arriving after a command failure are late by definition: the record is already final.
- (VNDrawAttempt *)lockedPresentedAttempt:(uint64_t)attemptID{
    VNDrawAttempt *a=[self lockedAttempt:attemptID];
    if(!a){_lateUpdates++;return NULL;}
    if(a->gate==VNDrawGateCommandFailed){_lateUpdates++;return NULL;}
    return a;
}
- (uint64_t)admitAttemptWithEntry:(double)entry acquireBegin:(double)acquireBegin acquireEnd:(double)acquireEnd windowEpoch:(uint64_t)windowEpoch generation:(uint64_t)generation sequence:(uint64_t)sequence revision:(uint64_t)revision drawableWidth:(NSUInteger)width drawableHeight:(NSUInteger)height clear:(BOOL)clear leaseLost:(BOOL)leaseLost{
    if(!_enabled)return 0;
    [_lock lock];
    if(_count==VN_DRAW_CAPACITY){
        if(!attemptFinal(&_ring[_head]))_pendingEvicted++;
        _evicted++;_head=(_head+1)%VN_DRAW_CAPACITY;_count--;
    }
    VNDrawAttempt a;memset(&a,0,sizeof a);
    a.attemptID=++_attempts;a.windowEpoch=windowEpoch;a.generation=generation;a.sequence=sequence;a.revision=revision;
    a.drawableWidth=width;a.drawableHeight=height;a.clear=clear;a.leaseLost=leaseLost;a.terminal=-1;a.gate=-1;
    a.entry=cpuStamp(entry,&_invalidCPU);a.acquireBegin=cpuStamp(acquireBegin,&_invalidCPU);a.acquireEnd=cpuStamp(acquireEnd,&_invalidCPU);
    a.commitEnd=a.completionEntered=a.inflightCleared=a.presentedEntered=a.presentedMain=NAN;
    a.gpuStart=a.gpuEnd=a.metalPresented=NAN;
    if(leaseLost)_leaseLost++;
    _ring[(_head+_count)%VN_DRAW_CAPACITY]=a;_count++;
    uint64_t identifier=a.attemptID;
    [_lock unlock];
    return identifier;
}
- (void)attempt:(uint64_t)attemptID terminal:(VNDrawBranch)branch{
    if(!_enabled||branch<VNDrawBranchNoDrawableOrPass||branch>VNDrawBranchRetiredGeneration)return;
    [_lock lock];
    [self lockedCountBranch:branch];
    VNDrawAttempt *a=[self lockedAttempt:attemptID];if(a)a->terminal=(int)branch;
    [_lock unlock];
}
- (void)attempt:(uint64_t)attemptID committedAt:(double)commitEnd clear:(BOOL)clear{
    if(!_enabled)return;
    [_lock lock];
    VNDrawBranch branch=clear?VNDrawBranchSubmittedClear:VNDrawBranchSubmittedFrame;
    [self lockedCountBranch:branch];_submissions++;
    VNDrawAttempt *a=[self lockedAttempt:attemptID];
    if(a){a->terminal=(int)branch;a->submissionID=_submissions;a->commitEnd=cpuStamp(commitEnd,&_invalidCPU);}
    [_lock unlock];
}
- (void)attempt:(uint64_t)attemptID completionEnteredAt:(double)entered status:(NSInteger)status gpuStart:(double)gpuStart gpuEnd:(double)gpuEnd{
    if(!_enabled)return;
    [_lock lock];
    double start=rawMetal(gpuStart,&_invalidMetalGPU),end=rawMetal(gpuEnd,&_invalidMetalGPU);
    VNDrawAttempt *a=[self lockedStageAttempt:attemptID];
    if(a){a->completionEntered=cpuStamp(entered,&_invalidCPU);a->commandStatus=status;a->hasCommandStatus=YES;a->gpuStart=start;a->gpuEnd=end;}
    [_lock unlock];
}
- (void)attempt:(uint64_t)attemptID inflightClearedAt:(double)cleared gate:(VNDrawCompletionGate)gate{
    if(!_enabled||gate>=VNDrawGateCount)return;
    [_lock lock];
    // Completion outcomes are cumulative even when the attempt has already left the ring.
    _gates[gate]++;
    if(gate==VNDrawGateCommandFailed)_failed++;else _completed++;
    VNDrawAttempt *a=[self lockedStageAttempt:attemptID];
    if(a){a->inflightCleared=cpuStamp(cleared,&_invalidCPU);a->gate=(int)gate;}
    [_lock unlock];
}
- (void)attempt:(uint64_t)attemptID presentedEnteredAt:(double)entered metalPresented:(double)presented{
    if(!_enabled)return;
    [_lock lock];
    // Same monotonic policy as VNPresentedSamples; raw values are kept for analysis, including zero.
    BOOL valid=isfinite(presented)&&presented>0&&presented>_lastMetalPresented;
    if(valid)_lastMetalPresented=presented;else _invalidMetalPresented++;
    VNDrawAttempt *a=[self lockedPresentedAttempt:attemptID];
    if(a){a->presentedEntered=cpuStamp(entered,&_invalidCPU);a->metalPresented=isfinite(presented)?presented:NAN;}
    [_lock unlock];
}
- (void)attempt:(uint64_t)attemptID presentedMainAt:(double)recorded{
    if(!_enabled)return;
    [_lock lock];
    VNDrawAttempt *a=[self lockedPresentedAttempt:attemptID];
    if(a)a->presentedMain=cpuStamp(recorded,&_invalidCPU);
    [_lock unlock];
}
- (NSDictionary *)statistics{
    // Default-off wire stays minimal; enabled output carries the full version-1 shape.
    if(!_enabled)return @{@"version":@1,@"enabled":@NO,@"authority":@NO,@"capacity":@VN_DRAW_CAPACITY};
    // Copy the ring under the lock; build the record dictionaries after releasing it.
    [_lock lock];
    VNDrawAttempt snapshot[VN_DRAW_CAPACITY];
    NSUInteger snapshotCount=_count;
    for(NSUInteger i=0;i<snapshotCount;i++)snapshot[i]=_ring[(_head+i)%VN_DRAW_CAPACITY];
    NSMutableDictionary *branches=[NSMutableDictionary new];
    for(NSUInteger i=0;i<VNDrawBranchCount;i++)branches[branchNames[i]]=@(_branches[i]);
    NSDictionary *value=@{
        @"version":@1,
        @"enabled":_enabled?@YES:@NO,
        @"authority":@NO,
        @"capacity":@VN_DRAW_CAPACITY,
        @"clock":@{@"cpu":@"NSProcessInfo.systemUptime",@"metal_gpu":@"MTLCommandBuffer.GPUStartTime/GPUEndTime",@"metal_presented":@"CAMetalDrawable.presentedTime"},
        @"callbacks":@(_callbacks),
        @"branches":[branches copy],
        @"lease_lost":@(_leaseLost),
        @"attempts_issued":@(_attempts),
        @"submissions_issued":@(_submissions),
        @"evicted":@(_evicted),
        @"pending_evicted":@(_pendingEvicted),
        @"late_updates":@(_lateUpdates),
        @"invalid_clock":@{@"cpu_stage":@(_invalidCPU),@"metal_gpu":@(_invalidMetalGPU),@"metal_presented":@(_invalidMetalPresented)},
        @"completions":@{@"completed":@(_completed),@"failed":@(_failed),@"gate_applied":@(_gates[VNDrawGateApplied]),@"gate_epoch_changed":@(_gates[VNDrawGateEpochChanged]),@"gate_revision_changed":@(_gates[VNDrawGateRevisionChanged])}
    };
    [_lock unlock];
    NSMutableArray *records=[NSMutableArray new];
    for(NSUInteger i=0;i<snapshotCount;i++)[records addObject:attemptRecord(&snapshot[i])];
    NSMutableDictionary *output=[value mutableCopy];
    output[@"records"]=records;
    return [output copy];
}
@end
