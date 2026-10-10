#import "InputFrameDiagnostic.h"
#include <math.h>
#include <string.h>
static BOOL integralNumber(id n){if(![n isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)n)==CFBooleanGetTypeID())return NO;const char *t=[n objCType];return t&&t[0]&&t[1]==0&&strchr("cCsSiIlLqQ",t[0])!=NULL;}
static BOOL nonnegativeInteger(id n){if(!integralNumber(n))return NO;return strchr("CSILQ",[n objCType][0])!=NULL||[n longLongValue]>=0;}
BOOL VNInputDiagnosticIDValid(id n){return nonnegativeInteger(n)&&[n unsignedLongLongValue]>=1&&[n unsignedLongLongValue]<=9007199254740991ULL;}
static BOOL safeToken(NSDictionary *t){
    if(![t isKindOfClass:NSDictionary.class]||t.count!=8)return NO;
    NSString *s=t[@"display_instance"];if(![s isKindOfClass:NSString.class]||s.length!=32||[s rangeOfCharacterFromSet:[[NSCharacterSet characterSetWithCharactersInString:@"0123456789abcdef"] invertedSet]].location!=NSNotFound)return NO;
    for(NSString *k in @[@"presentation_serial",@"window_epoch",@"invalidation_epoch",@"source_generation",@"source_sequence",@"width",@"height"]){id n=t[k];if(!nonnegativeInteger(n))return NO;}
    return YES;
}
static NSDictionary *safeObservation(NSDictionary *o){
    if(![o isKindOfClass:NSDictionary.class])return nil;
    NSMutableDictionary *v=[NSMutableDictionary new];
    for(NSString *k in @[@"monotonic_seconds",@"callback_received_monotonic_seconds",@"callback_age_ms"]){id n=o[k];if(n==NSNull.null)v[k]=n;else if([n isKindOfClass:NSNumber.class]&&CFGetTypeID((__bridge CFTypeRef)n)!=CFBooleanGetTypeID()&&isfinite([n doubleValue]))v[k]=n;else return nil;}
    for(NSString *k in @[@"record_found",@"current"]){id n=o[k];if(n&&(n==NSNull.null||CFGetTypeID((__bridge CFTypeRef)n)==CFBooleanGetTypeID()))v[k]=n;else return nil;}
    return [v copy];
}
@implementation VNInputFrameDiagnostic {
    NSLock *_lock;NSMutableArray<NSDictionary *> *_records;BOOL _enabled;uint64_t _next,_evicted,_unmatched,_duplicates;
}
- (instancetype)initWithEnabled:(BOOL)enabled{if((self=[super init])){_enabled=enabled;_lock=[NSLock new];_records=[NSMutableArray new];}return self;}
- (BOOL)enabled{return _enabled;}
- (NSNumber *)acquireToken:(NSDictionary *)token observation:(NSDictionary *)observation epoch:(uint64_t)epoch{
    observation=safeObservation(observation);if(!_enabled||!safeToken(token)||!observation)return nil;
    [_lock lock];NSNumber *identifier=nil;
    if(_next<9007199254740991ULL){identifier=@(++_next);if(_records.count==16){[_records removeObjectAtIndex:0];_evicted++;}[_records addObject:@{@"diagnostic_id":identifier,@"token":[token copy],@"acquisition":[observation copy],@"enqueue_epoch":@(epoch)}];}
    [_lock unlock];return identifier;
}
- (void)dispatched:(NSNumber *)identifier sequence:(uint64_t)sequence epoch:(uint64_t)epoch now:(double)now queueAge:(double)age{
    if(!_enabled||!VNInputDiagnosticIDValid(identifier)||!isfinite(now)||now<=0||!isfinite(age))return;
    [_lock lock];for(NSUInteger i=0;i<_records.count;i++)if([_records[i][@"diagnostic_id"] isEqual:identifier]){NSMutableDictionary *r=[_records[i] mutableCopy];r[@"dispatch"]=@{@"sequence":@(sequence),@"epoch":@(epoch),@"monotonic_seconds":@(now),@"queue_age_ms":@(age*1000)};_records[i]=[r copy];break;}[_lock unlock];
}
- (void)validated:(NSNumber *)identifier token:(NSDictionary *)token observation:(NSDictionary *)observation accepted:(BOOL)accepted reason:(NSString *)reason{
    observation=safeObservation(observation);if(!_enabled||!VNInputDiagnosticIDValid(identifier)||!observation)return;
    NSArray *reasons=@[@"display_invalid_token",@"display_no_snapshot",@"display_instance_mismatch",@"display_epoch_mismatch",@"display_history_miss",@"display_historical_expired",@"display_record_mismatch",@"acquisition_invalid_request",@"acquisition_unavailable",@"acquisition_expired",@"acquisition_invalidated"];
    [_lock lock];BOOL matched=NO;
    for(NSUInteger i=0;i<_records.count;i++)if([_records[i][@"diagnostic_id"] isEqual:identifier]&&[_records[i][@"token"] isEqual:token]){
        matched=YES;if(_records[i][@"validation"]){_duplicates++;break;}
        NSMutableDictionary *r=[_records[i] mutableCopy];NSMutableDictionary *v=[observation mutableCopy];v[@"accepted"]=accepted?@YES:@NO;v[@"reason"]=reason?([reasons containsObject:reason]?reason:@"unclassified"):[NSNull null];r[@"validation"]=[v copy];_records[i]=[r copy];break;
    }
    if(!matched)_unmatched++;[_lock unlock];
}
- (NSDictionary *)statistics{[_lock lock];NSDictionary *r=@{@"version":@1,@"enabled":_enabled?@YES:@NO,@"capacity":@16,@"issued":@(_next),@"evicted":@(_evicted),@"unmatched_validations":@(_unmatched),@"duplicate_validations":@(_duplicates),@"records":[_records copy],@"authority":@NO};[_lock unlock];return r;}
@end
