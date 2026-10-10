#import "FrameAcquisition.h"
#import "DisplayedFrame.h"
#include <math.h>
#define CHECK(...) NSCAssert((__VA_ARGS__),@"%s",#__VA_ARGS__)
static VNDisplayGeometry geometry(void){return (VNDisplayGeometry){.sourceWidth=640,.sourceHeight=480,.drawableWidth=640,.drawableHeight=480,.viewBounds={0,0,640,480},.backingBounds={0,0,640,480},.viewport={0,0,640,480}};}
static void publish(VNDisplayedFrameStore *s,uint64_t n,double t){CHECK([s commitCandidate:[s candidateGeometry:geometry() generation:1 sequence:n windowEpoch:1 capturedMonotonicUS:n] presentedTime:t-.01 receivedMonotonic:t]);}
int main(void){@autoreleasepool{
    VNDisplayedFrameStore *s=[VNDisplayedFrameStore new];publish(s,1,10);
    VNFrameAcquisition *r=[VNFrameAcquisition new];NSDictionary *context=@{@"epoch":@(UINT64_MAX),@"generation":@3};
    CHECK(![r issueFromStore:s context:context now:20]);[r activateEpoch:UINT64_MAX generation:3];
    NSDictionary *original=[s snapshot],*token=VNDisplayedFrameToken(original);NSString *receipt=[r issueFromStore:s context:context now:20];CHECK(VNFrameReceiptIdentifier(receipt));
    VNFrameAcquisition *foreign=[VNFrameAcquisition new];[foreign activateEpoch:UINT64_MAX generation:3];CHECK(![foreign consume:receipt token:token epoch:UINT64_MAX sequence:1 store:s now:20.001 failure:nil]);
    CHECK(![r consume:receipt token:token epoch:UINT64_MAX sequence:1 store:s now:20.001 failure:nil]); // unclaimed
    CHECK(![r claim:receipt token:token epoch:7 sequence:1 generation:3 now:20.001]);
    CHECK([r claim:receipt token:token epoch:UINT64_MAX sequence:UINT64_MAX generation:3 now:20.002]);
    CHECK(![r claim:receipt token:token epoch:UINT64_MAX sequence:UINT64_MAX generation:3 now:20.003]);
    publish(s,2,20.004);CHECK(![s validateToken:token now:20.005]);
    NSMutableDictionary *wrong=[token mutableCopy];wrong[@"source_sequence"]=@99;
    CHECK(![r consume:receipt token:wrong epoch:UINT64_MAX sequence:UINT64_MAX store:s now:20.005 failure:nil]);
    CHECK(![r consume:receipt token:token epoch:UINT64_MAX sequence:1 store:s now:20.005 failure:nil]);
    NSDictionary *proof=[r consume:receipt token:token epoch:UINT64_MAX sequence:UINT64_MAX store:s now:20.006 failure:nil];
    CHECK(proof&&proof[@"frame"]==original&&![proof[@"current"] boolValue]);CHECK([proof[@"frame"][@"callback_received_monotonic_seconds"] isEqual:@10]);
    CHECK([proof[@"acquisition"] count]==4&&[proof[@"acquisition"][@"remaining_us"] unsignedLongLongValue]<=194000);
    CHECK([proof[@"acquisition"][@"sequence"] unsignedLongLongValue]==UINT64_MAX);
    CHECK(![r consume:receipt token:token epoch:UINT64_MAX sequence:UINT64_MAX store:s now:20.007 failure:nil]);
    // Retain only receipt's original snapshot even when the ordinary8history evicts it.
    token=VNDisplayedFrameToken([s snapshot]);receipt=[r issueFromStore:s context:context now:21];CHECK([r claim:receipt token:token epoch:UINT64_MAX sequence:2 generation:3 now:21.001]);
    for(int i=3;i<13;i++)publish(s,i,21+i*.001);
    CHECK(![s validateToken:token now:21.02]);proof=[r consume:receipt token:token epoch:UINT64_MAX sequence:2 store:s now:21.02 failure:nil];CHECK(proof);
    // Absolute boundary: no claim/consume at deadline, no renewal after claim.
    token=VNDisplayedFrameToken([s snapshot]);receipt=[r issueFromStore:s context:context now:22];CHECK([r claim:receipt token:token epoch:UINT64_MAX sequence:3 generation:3 now:22.19]);NSString *reason=nil;
    CHECK(![r consume:receipt token:token epoch:UINT64_MAX sequence:3 store:s now:22.2 failure:&reason]&&[reason isEqual:@"acquisition_expired"]);
    receipt=[r issueFromStore:s context:context now:23];CHECK(![r claim:receipt token:token epoch:UINT64_MAX sequence:4 generation:3 now:23.2]);
    // Source/layout/window invalidation cannot inherit receipt eligibility.
    receipt=[r issueFromStore:s context:context now:24];CHECK([r claim:receipt token:token epoch:UINT64_MAX sequence:5 generation:3 now:24.01]);[s invalidate];publish(s,13,24.02);
    CHECK(![r consume:receipt token:token epoch:UINT64_MAX sequence:5 store:s now:24.03 failure:&reason]&&[reason isEqual:@"acquisition_invalidated"]);
    token=VNDisplayedFrameToken([s snapshot]);receipt=[r issueFromStore:s context:context now:25];[r deactivate];CHECK(![r claim:receipt token:token epoch:UINT64_MAX sequence:6 generation:3 now:25.01]);
    [r activateEpoch:UINT64_MAX generation:4];CHECK(![r issueFromStore:s context:context now:25.02]);context=@{@"epoch":@(UINT64_MAX),@"generation":@4};
    NSMutableSet *ids=[NSMutableSet new];for(int i=0;i<34;i++){NSString *x=[r issueFromStore:s context:context now:26];CHECK(x);[ids addObject:x];}CHECK(ids.count==34&&! [r issueFromStore:s context:context now:26.01]);
    CHECK([[r statistics][@"outstanding"] intValue]==34);for(NSString *x in ids)[r cancel:x];CHECK([[r statistics][@"outstanding"] intValue]==0);
    receipt=[r issueFromStore:s context:context now:27];[r invalidateFrames];CHECK(![r claim:receipt token:token epoch:UINT64_MAX sequence:7 generation:4 now:27.01]);
    CHECK(!VNFrameUnsignedInteger(@YES,YES)&&!VNFrameUnsignedInteger(@1.0,NO)&&!VNFrameUnsignedInteger(@(-1),YES)&&VNFrameUnsignedInteger(@(UINT64_MAX),NO));
    // Concurrent revocation cannot revive a consumed or cancelled receipt.
    for(int i=0;i<100;i++){receipt=[r issueFromStore:s context:context now:28];CHECK(receipt);dispatch_group_t group=dispatch_group_create();NSString *owned=receipt;dispatch_group_async(group,dispatch_get_global_queue(0,0),^{[r cancel:owned];});dispatch_group_async(group,dispatch_get_global_queue(0,0),^{[r claim:owned token:token epoch:UINT64_MAX sequence:8 generation:4 now:28.001];});CHECK(dispatch_group_wait(group,dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC))==0);CHECK(![r consume:receipt token:token epoch:UINT64_MAX sequence:8 store:s now:28.002 failure:nil]);}
    CHECK([[r statistics][@"outstanding"] intValue]==0);
    for(int i=0;i<100;i++){[r activateEpoch:UINT64_MAX generation:4];NSString *owned=[r issueFromStore:s context:context now:29];CHECK([r claim:owned token:token epoch:UINT64_MAX sequence:9 generation:4 now:29.001]);dispatch_group_t group=dispatch_group_create();__block NSDictionary *consumed=nil;dispatch_group_async(group,dispatch_get_global_queue(0,0),^{consumed=[r consume:owned token:token epoch:UINT64_MAX sequence:9 store:s now:29.002 failure:nil];});dispatch_group_async(group,dispatch_get_global_queue(0,0),^{[r deactivate];});CHECK(dispatch_group_wait(group,dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC))==0);CHECK(!consumed||[consumed[@"valid"] boolValue]);CHECK([[r statistics][@"outstanding"] intValue]==0);CHECK(![r consume:owned token:token epoch:UINT64_MAX sequence:9 store:s now:29.003 failure:nil]);}
    // Actual validation-admission method closes the notification/timer window:
    // a claimed receipt and still-ready control cannot override current focus.
    for(int which=0;which<4;which++){[r activateEpoch:UINT64_MAX generation:4];NSString *owned=[r issueFromStore:s context:context now:30];CHECK([r claim:owned token:token epoch:UINT64_MAX sequence:10 generation:4 now:30.001]);CHECK(![r requireFocusReady:which!=0 applicationActive:which!=1 windowKey:which!=2 firstResponder:which!=3]);CHECK([[r statistics][@"outstanding"] intValue]==0);CHECK([r requireFocusReady:YES applicationActive:YES windowKey:YES firstResponder:YES]);CHECK(![r consume:owned token:token epoch:UINT64_MAX sequence:10 store:s now:30.002 failure:nil]);CHECK(![r issueFromStore:s context:context now:30.003]);}
    puts("PASS actual frame store/receipt registry: old-current race and history eviction preserve original snapshot; exact one-use binding/u64;200ms boundary;invalidation/cancel/34capacity/concurrent claim-cancel/consume-deactivate; synchronous ready/focus rejection with no rearm; no VM/GPU/input.");return 0;
}}
