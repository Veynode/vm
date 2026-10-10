#import "InputFrameDiagnostic.h"
#import "DisplayedFrame.h"
#include <math.h>
#define CHECK(x) NSCAssert((x),@#x)
int main(void){@autoreleasepool{
    VNDisplayGeometry g={.sourceWidth=640,.sourceHeight=480,.drawableWidth=640,.drawableHeight=480,.viewBounds={0,0,640,480},.backingBounds={0,0,640,480},.viewport={0,0,640,480}};
    VNDisplayedFrameStore *store=[VNDisplayedFrameStore new];
    NSDictionary *c=[store candidateGeometry:g generation:1 sequence:1 windowEpoch:1 capturedMonotonicUS:1];
    CHECK([store commitCandidate:c presentedTime:9 receivedMonotonic:10]);
    NSDictionary *original=[store snapshot],*token=VNDisplayedFrameToken(original);
    VNInputFrameDiagnostic *off=[[VNInputFrameDiagnostic alloc] initWithEnabled:NO];
    NSDictionary *acquisition=[store diagnosticToken:token now:11];
    CHECK(![off acquireToken:token observation:acquisition epoch:7]);CHECK([[off statistics][@"records"] count]==0);
    VNInputFrameDiagnostic *diag=[[VNInputFrameDiagnostic alloc] initWithEnabled:YES];
    NSNumber *identifier=[diag acquireToken:token observation:acquisition epoch:7];CHECK([identifier isEqual:@1]);
    CHECK([store validateToken:token now:11]); // A genuinely current static frame is still allowed.
    [diag dispatched:identifier sequence:31 epoch:7 now:11.001 queueAge:.001];
    NSDictionary *c2=[store candidateGeometry:g generation:1 sequence:2 windowEpoch:1 capturedMonotonicUS:2];
    CHECK([store commitCandidate:c2 presentedTime:10 receivedMonotonic:11.002]);
    NSString *reason=nil;NSDictionary *result=[store validateToken:token now:11.003 failure:&reason];
    CHECK(!result&&[reason isEqual:@"display_historical_expired"]); // Original policy rejection is unchanged.
    [diag validated:identifier token:token observation:[store diagnosticToken:token now:11.003] accepted:result!=nil reason:reason];
    NSDictionary *r=[diag statistics][@"records"][0];
    CHECK([r[@"acquisition"][@"current"] boolValue]);CHECK(![r[@"validation"][@"current"] boolValue]);
    CHECK([r[@"acquisition"][@"callback_received_monotonic_seconds"] isEqual:@10]);
    CHECK(fabs([r[@"acquisition"][@"callback_age_ms"] doubleValue]-1000)<.001);
    CHECK(fabs([r[@"validation"][@"callback_age_ms"] doubleValue]-1003)<.001);
    CHECK([r[@"dispatch"][@"sequence"] isEqual:@31]&&[r[@"dispatch"][@"epoch"] isEqual:@7]);
    CHECK(![r[@"validation"][@"accepted"] boolValue]);CHECK([original[@"callback_received_monotonic_seconds"] isEqual:@10]);
    CHECK(![store validateToken:token now:11.003]);
    // Same token, separate event IDs do not merge. Unknown/mismatched IDs cannot overwrite another record.
    NSNumber *second=[diag acquireToken:token observation:acquisition epoch:7];CHECK([second isEqual:@2]);
    [diag validated:@99 token:token observation:acquisition accepted:YES reason:nil];
    [diag validated:second token:VNDisplayedFrameToken([store snapshot]) observation:acquisition accepted:YES reason:nil];
    CHECK([[diag statistics][@"unmatched_validations"] intValue]==2);
    [diag validated:identifier token:token observation:acquisition accepted:YES reason:nil];
    CHECK([[diag statistics][@"duplicate_validations"] intValue]==1);
    CHECK(![[diag statistics][@"records"][0][@"validation"][@"accepted"] boolValue]);
    [store invalidate];CHECK(![store validateToken:token now:11.004 failure:&reason]);
    [diag validated:second token:token observation:[store diagnosticToken:token now:11.004] accepted:NO reason:reason];
    CHECK(![[diag statistics][@"records"][1][@"validation"][@"record_found"] boolValue]);
    // No input payload can leak through observation dictionaries.
    NSMutableDictionary *extra=[acquisition mutableCopy];extra[@"qcode"]=@"secret";extra[@"event"]=@"private";
    for(int i=0;i<40;i++)CHECK([diag acquireToken:token observation:extra epoch:7]);
    NSDictionary *stats=[diag statistics];CHECK([stats[@"records"] count]==16&&[stats[@"evicted"] intValue]==26);
    NSData *json=[NSJSONSerialization dataWithJSONObject:stats options:0 error:nil];CHECK(json.length<16384);
    NSString *text=[[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding];CHECK(![text containsString:@"secret"]&&![text containsString:@"private"]);
    NSDictionary *decoded=[NSJSONSerialization JSONObjectWithData:json options:0 error:nil];CHECK(CFGetTypeID((__bridge CFTypeRef)decoded[@"enabled"])==CFBooleanGetTypeID());
    CHECK(VNInputDiagnosticIDValid(@1)&&VNInputDiagnosticIDValid(@9007199254740991ULL));
    NSMutableDictionary *fullWidth=[token mutableCopy];fullWidth[@"invalidation_epoch"]=@0;fullWidth[@"source_sequence"]=@(UINT64_MAX);
    VNInputFrameDiagnostic *wide=[[VNInputFrameDiagnostic alloc] initWithEnabled:YES];CHECK([wide acquireToken:fullWidth observation:acquisition epoch:UINT64_MAX]);
    CHECK([[wide statistics][@"records"][0][@"enqueue_epoch"] unsignedLongLongValue]==UINT64_MAX);
    CHECK([wide statistics][@"records"][0][@"token"][@"source_sequence"]&&[[wide statistics][@"records"][0][@"token"] isEqual:fullWidth]);
    // Diagnostic observation does not authorize zero or change the production validator.
    CHECK(![store validateToken:fullWidth now:11]);
    CHECK(!VNInputDiagnosticIDValid(@YES)&&!VNInputDiagnosticIDValid(@1.0)&&!VNInputDiagnosticIDValid(@0)&&!VNInputDiagnosticIDValid(@(-1))&&!VNInputDiagnosticIDValid(@9007199254740992ULL)&&!VNInputDiagnosticIDValid(@"1")&&!VNInputDiagnosticIDValid(NSNull.null));
    for(NSString *reason in @[@"acquisition_invalid_request",@"acquisition_unavailable",@"acquisition_expired",@"acquisition_invalidated"]){NSNumber *n=[wide acquireToken:fullWidth observation:acquisition epoch:UINT64_MAX];[wide validated:n token:fullWidth observation:acquisition accepted:NO reason:reason];CHECK([[[[wide statistics][@"records"] lastObject][@"validation"] objectForKey:@"reason"] isEqual:reason]);}
    puts("PASS actual frame-store old-current replacement still rejects; exact IDs/epoch/sequence, original ages, invalidation, bounded ring/default off/strict IDs/no input contents. No window, VM, GPU or input dispatch.");return 0;
}}
