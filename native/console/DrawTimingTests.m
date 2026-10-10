#import "DrawTiming.h"
#include <math.h>
#define CHECK(x) NSCAssert((x),@#x)

static NSDictionary *recordFor(NSDictionary *statistics,NSUInteger index){return statistics[@"records"][index];}
static NSUInteger branchTotal(NSDictionary *statistics){
    NSUInteger total=0;for(NSString *key in statistics[@"branches"])total+=[statistics[@"branches"][key] unsignedIntegerValue];return total;
}

int main(void){@autoreleasepool{
    // Disabled instance: every method is a no-op and the wire shape is still stable.
    VNDrawTiming *off=[[VNDrawTiming alloc] initWithEnabled:NO];
    [off recordCallbackBranch:VNDrawBranchInflight];
    CHECK([off admitAttemptWithEntry:1 acquireBegin:1 acquireEnd:1 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO]==0);
    [off attempt:1 terminal:VNDrawBranchNoDrawableOrPass];
    [off attempt:1 committedAt:1 clear:NO];
    [off attempt:1 completionEnteredAt:1 status:4 gpuStart:1 gpuEnd:2];
    [off attempt:1 inflightClearedAt:1 gate:VNDrawGateApplied];
    [off attempt:1 presentedEnteredAt:1 metalPresented:1];
    [off attempt:1 presentedMainAt:1];
    NSDictionary *offStats=[off statistics];
    CHECK([offStats[@"version"] integerValue]==1&&[offStats[@"enabled"] boolValue]==NO&&[offStats[@"authority"] boolValue]==NO);
    CHECK(offStats[@"records"]==nil&&offStats[@"branches"]==nil&&offStats.count==4);
    CHECK([offStats[@"capacity"] integerValue]==32);

    // Exact branch identity over a mixed sequence, including precedence-only early returns.
    VNDrawTiming *timing=[[VNDrawTiming alloc] initWithEnabled:YES];
    [timing recordCallbackBranch:VNDrawBranchInflight];
    [timing recordCallbackBranch:VNDrawBranchNoWindow];
    [timing recordCallbackBranch:VNDrawBranchInvisible];
    [timing recordCallbackBranch:VNDrawBranchUnchangedSource];
    [timing recordCallbackBranch:VNDrawBranchSubmittedFrame];            // attempt-only branch: must be ignored here
    uint64_t lost=[timing admitAttemptWithEntry:10 acquireBegin:10.001 acquireEnd:10.002 windowEpoch:3 generation:7 sequence:8 revision:9 drawableWidth:0 drawableHeight:0 clear:YES leaseLost:YES];
    [timing attempt:lost terminal:VNDrawBranchNoDrawableOrPass];
    uint64_t command=[timing admitAttemptWithEntry:11 acquireBegin:11.001 acquireEnd:11.002 windowEpoch:3 generation:7 sequence:8 revision:9 drawableWidth:1920 drawableHeight:1200 clear:NO leaseLost:NO];
    [timing attempt:command terminal:VNDrawBranchCommandOrEncoderFailed];
    uint64_t texture=[timing admitAttemptWithEntry:12 acquireBegin:12.001 acquireEnd:12.002 windowEpoch:3 generation:7 sequence:8 revision:9 drawableWidth:1920 drawableHeight:1200 clear:NO leaseLost:NO];
    [timing attempt:texture terminal:VNDrawBranchTextureFailed];
    uint64_t retired=[timing admitAttemptWithEntry:13 acquireBegin:13.001 acquireEnd:13.002 windowEpoch:3 generation:7 sequence:8 revision:9 drawableWidth:1920 drawableHeight:1200 clear:NO leaseLost:NO];
    [timing attempt:retired terminal:VNDrawBranchRetiredGeneration];
    uint64_t frameDraw=[timing admitAttemptWithEntry:14 acquireBegin:14.001 acquireEnd:14.002 windowEpoch:3 generation:7 sequence:8 revision:9 drawableWidth:1920 drawableHeight:1200 clear:NO leaseLost:NO];
    [timing attempt:frameDraw committedAt:14.003 clear:NO];
    uint64_t clearDraw=[timing admitAttemptWithEntry:15 acquireBegin:15.001 acquireEnd:15.002 windowEpoch:3 generation:0 sequence:0 revision:9 drawableWidth:1920 drawableHeight:1200 clear:YES leaseLost:NO];
    [timing attempt:clearDraw committedAt:15.003 clear:YES];
    NSDictionary *stats=[timing statistics];
    CHECK([stats[@"callbacks"] unsignedLongLongValue]==10);
    CHECK(branchTotal(stats)==[stats[@"callbacks"] unsignedLongLongValue]);
    CHECK([stats[@"branches"][@"inflight"] unsignedLongLongValue]==1&&[stats[@"branches"][@"unchanged_source"] unsignedLongLongValue]==1);
    CHECK([stats[@"branches"][@"submitted_frame"] unsignedLongLongValue]==1&&[stats[@"branches"][@"submitted_clear"] unsignedLongLongValue]==1);
    CHECK([stats[@"branches"][@"no_drawable_or_pass"] unsignedLongLongValue]==1&&[stats[@"branches"][@"retired_generation"] unsignedLongLongValue]==1);
    CHECK([stats[@"lease_lost"] unsignedLongLongValue]==1&&[stats[@"attempts_issued"] unsignedLongLongValue]==6&&[stats[@"submissions_issued"] unsignedLongLongValue]==2);
    CHECK([recordFor(stats,0)[@"terminal"] isEqual:@"no_drawable_or_pass"]&&[recordFor(stats,0)[@"lease_lost"] boolValue]&&[recordFor(stats,0)[@"drawable"] isEqual:NSNull.null]);
    CHECK([recordFor(stats,0)[@"wait_ms"] doubleValue]>0.99&&[recordFor(stats,0)[@"submit_ms"] isEqual:NSNull.null]);
    CHECK([recordFor(stats,4)[@"terminal"] isEqual:@"submitted_frame"]&&[recordFor(stats,4)[@"submission_id"] unsignedLongLongValue]==1);
    CHECK([recordFor(stats,4)[@"submit_ms"] doubleValue]>0.99&&[recordFor(stats,4)[@"final"] boolValue]==NO);
    CHECK([recordFor(stats,5)[@"submission_id"] unsignedLongLongValue]==2&&[recordFor(stats,5)[@"drawable"][0] unsignedIntegerValue]==1920);
    CHECK([recordFor(stats,4)[@"gpu_entry_ms"] isEqual:NSNull.null]&&[recordFor(stats,4)[@"status"] isEqual:NSNull.null]);

    // Complete lifecycle, stage ordering and raw Metal values, including a zero presented time.
    VNDrawTiming *full=[[VNDrawTiming alloc] initWithEnabled:YES];
    uint64_t a=[full admitAttemptWithEntry:100.000 acquireBegin:100.002 acquireEnd:100.004 windowEpoch:5 generation:11 sequence:12 revision:13 drawableWidth:2560 drawableHeight:1440 clear:NO leaseLost:NO];
    [full attempt:a committedAt:100.006 clear:NO];
    [full attempt:a completionEnteredAt:100.020 status:4 gpuStart:5000.25 gpuEnd:5000.75];
    [full attempt:a inflightClearedAt:100.021 gate:VNDrawGateApplied];
    [full attempt:a presentedEnteredAt:100.033 metalPresented:0];
    [full attempt:a presentedMainAt:100.034];
    NSDictionary *fs=[full statistics];
    NSDictionary *r=recordFor(fs,0);
    CHECK([r[@"final"] boolValue]&&[r[@"terminal"] isEqual:@"submitted_frame"]&&[r[@"gate"] isEqual:@"applied"]&&[r[@"status"] integerValue]==4);
    CHECK(fabs([r[@"pre_ms"] doubleValue]-2.0)<1e-9&&fabs([r[@"wait_ms"] doubleValue]-2.0)<1e-9&&fabs([r[@"submit_ms"] doubleValue]-2.0)<1e-9);
    CHECK(fabs([r[@"gpu_entry_ms"] doubleValue]-14.0)<1e-9&&fabs([r[@"inflight_ms"] doubleValue]-1.0)<1e-9&&fabs([r[@"pres_entry_ms"] doubleValue]-27.0)<1e-9&&fabs([r[@"pres_main_ms"] doubleValue]-1.0)<1e-9);
    CHECK([r[@"gpu_start_s"] doubleValue]==5000.25&&[r[@"gpu_end_s"] doubleValue]==5000.75);
    CHECK([r[@"pres_s"] doubleValue]==0);                       // raw zero preserved, not replaced by a substitute
    CHECK([fs[@"invalid_clock"][@"metal_presented"] unsignedLongLongValue]==1&&[fs[@"invalid_clock"][@"cpu_stage"] unsignedLongLongValue]==0);
    CHECK([fs[@"completions"][@"completed"] unsignedLongLongValue]==1&&[fs[@"completions"][@"gate_applied"] unsignedLongLongValue]==1);

    // Unreached stages are null, never zero.
    uint64_t pending=[full admitAttemptWithEntry:200 acquireBegin:200.001 acquireEnd:200.002 windowEpoch:5 generation:11 sequence:12 revision:13 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    (void)pending;
    NSDictionary *pr=recordFor([full statistics],1);
    CHECK([pr[@"terminal"] isEqual:@"pending"]&&[pr[@"final"] boolValue]==NO&&[pr[@"submit_ms"] isEqual:NSNull.null]&&[pr[@"gpu_start_s"] isEqual:NSNull.null]);

    // Invalid CPU stages are counted and left unreached, not coerced to zero.
    VNDrawTiming *bad=[[VNDrawTiming alloc] initWithEnabled:YES];
    uint64_t b=[bad admitAttemptWithEntry:NAN acquireBegin:0 acquireEnd:1 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    [bad attempt:b committedAt:INFINITY clear:NO];
    NSDictionary *bs=[bad statistics];
    CHECK([bs[@"invalid_clock"][@"cpu_stage"] unsignedLongLongValue]==3&&[recordFor(bs,0)[@"entry_s"] isEqual:NSNull.null]&&[recordFor(bs,0)[@"submit_ms"] isEqual:NSNull.null]);

    // Metal presented values use the monotonic policy: zero, NaN and regressions are invalid but raw values survive.
    VNDrawTiming *mono=[[VNDrawTiming alloc] initWithEnabled:YES];
    uint64_t m1=[mono admitAttemptWithEntry:1 acquireBegin:1.1 acquireEnd:1.2 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    uint64_t m2=[mono admitAttemptWithEntry:2 acquireBegin:2.1 acquireEnd:2.2 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    uint64_t m3=[mono admitAttemptWithEntry:3 acquireBegin:3.1 acquireEnd:3.2 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    [mono attempt:m1 presentedEnteredAt:3 metalPresented:100.5];
    [mono attempt:m2 presentedEnteredAt:4 metalPresented:100.25];          // regression: invalid, raw kept
    [mono attempt:m3 presentedEnteredAt:5 metalPresented:NAN];            // non-finite: invalid, raw null
    NSDictionary *ms=[mono statistics];
    CHECK([ms[@"invalid_clock"][@"metal_presented"] unsignedLongLongValue]==2);
    CHECK([recordFor(ms,0)[@"pres_s"] doubleValue]==100.5);
    CHECK([recordFor(ms,1)[@"pres_s"] doubleValue]==100.25);
    CHECK([recordFor(ms,2)[@"pres_s"] isEqual:NSNull.null]);

    // Ring: 40 admitted attempts saturate 32 slots. Pending evictions and late updates are counted, not reconstructed.
    VNDrawTiming *ring=[[VNDrawTiming alloc] initWithEnabled:YES];
    NSMutableArray<NSNumber *> *ids=[NSMutableArray new];
    for(int i=0;i<40;i++){
        double t=500+i*0.0166;
        uint64_t identifier=[ring admitAttemptWithEntry:t acquireBegin:t+0.0001 acquireEnd:t+0.0002 windowEpoch:2 generation:(uint64_t)i+1 sequence:(uint64_t)i+1 revision:2 drawableWidth:1280 drawableHeight:800 clear:NO leaseLost:NO];
        [ids addObject:@(identifier)];
        [ring attempt:identifier committedAt:t+0.0005 clear:NO];
    }
    NSDictionary *rs=[ring statistics];
    CHECK([rs[@"records"] count]==32&&[rs[@"evicted"] unsignedLongLongValue]==8&&[rs[@"pending_evicted"] unsignedLongLongValue]==8);
    CHECK([recordFor(rs,0)[@"attempt_id"] unsignedLongLongValue]==9&&[recordFor(rs,31)[@"attempt_id"] unsignedLongLongValue]==40);
    // ID 1 was evicted while still pending; its late completion is counted and creates no record.
    [ring attempt:[ids[0] unsignedLongLongValue] completionEnteredAt:600 status:4 gpuStart:1 gpuEnd:2];
    [ring attempt:[ids[0] unsignedLongLongValue] inflightClearedAt:600.1 gate:VNDrawGateApplied];
    NSDictionary *late=[ring statistics];
    CHECK([late[@"late_updates"] unsignedLongLongValue]==2&&[late[@"records"] count]==32&&[late[@"completions"][@"gate_applied"] unsignedLongLongValue]==1);
    CHECK([late[@"submissions_issued"] unsignedLongLongValue]==40&&branchTotal(late)==40);

    // Completion gates are counted for every outcome, including failures.
    VNDrawTiming *gates=[[VNDrawTiming alloc] initWithEnabled:YES];
    uint64_t g1=[gates admitAttemptWithEntry:1 acquireBegin:1.1 acquireEnd:1.2 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    [gates attempt:g1 committedAt:1.3 clear:NO];
    [gates attempt:g1 completionEnteredAt:1.4 status:5 gpuStart:0 gpuEnd:0];
    [gates attempt:g1 inflightClearedAt:1.5 gate:VNDrawGateCommandFailed];
    NSDictionary *gs=[gates statistics];
    CHECK([gs[@"completions"][@"failed"] unsignedLongLongValue]==1&&[gs[@"completions"][@"completed"] unsignedLongLongValue]==0);
    CHECK([recordFor(gs,0)[@"final"] boolValue]&&[recordFor(gs,0)[@"gate"] isEqual:@"command_failed"]&&[recordFor(gs,0)[@"status"] integerValue]==5);
    CHECK([recordFor(gs,0)[@"gpu_start_s"] doubleValue]==0&&[gs[@"invalid_clock"][@"metal_gpu"] unsignedLongLongValue]==2);

    // Presented stages after a command failure are late: a final record must not change.
    uint64_t gp=[gates admitAttemptWithEntry:2 acquireBegin:2.1 acquireEnd:2.2 windowEpoch:1 generation:1 sequence:1 revision:1 drawableWidth:1 drawableHeight:1 clear:NO leaseLost:NO];
    [gates attempt:gp committedAt:2.3 clear:NO];
    [gates attempt:gp completionEnteredAt:2.4 status:5 gpuStart:3 gpuEnd:4];
    [gates attempt:gp inflightClearedAt:2.5 gate:VNDrawGateCommandFailed];
    [gates attempt:gp presentedEnteredAt:2.6 metalPresented:9];
    [gates attempt:gp presentedMainAt:2.7];
    NSDictionary *gs2=[gates statistics];
    NSDictionary *fr=recordFor(gs2,1);
    CHECK([fr[@"final"] boolValue]&&[fr[@"pres_entry_ms"] isEqual:NSNull.null]&&[fr[@"pres_main_ms"] isEqual:NSNull.null]&&[fr[@"pres_s"] isEqual:NSNull.null]);
    CHECK([gs2[@"late_updates"] unsignedLongLongValue]==2&&[gs2[@"callbacks"] unsignedLongLongValue]==2);
    CHECK([gs2[@"branches"][@"submitted_frame"] unsignedLongLongValue]==2);

    puts("PASS draw timing component suite: disabled no-op, exact branch identity, admission precedence, stable IDs, saturated bounded ring, pending eviction, late updates, null-not-zero stages, invalid CPU/Metal clocks, raw Metal zero, monotonic presented policy and completion gates. No AppKit, window, GPU or input.");
    return 0;
}}
