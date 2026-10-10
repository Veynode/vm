#import "FrameAcquisition.h"
#import "DisplayedFrame.h"
#include "InputFocus.h"
#include <math.h>
#include <stdlib.h>
#include <string.h>
BOOL VNFrameUnsignedInteger(id n,BOOL zero){
    if(![n isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)n)==CFBooleanGetTypeID())return NO;
    const char *t=[n objCType];if(!t||!t[0]||t[1]||!strchr("cCsSiIlLqQ",t[0]))return NO;
    if(!strchr("CSILQ",t[0])&&[n longLongValue]<0)return NO;
    return zero||[n unsignedLongLongValue]!=0;
}
BOOL VNFrameReceiptIdentifier(id r){return [r isKindOfClass:NSString.class]&&[r length]==32&&[r rangeOfCharacterFromSet:[[NSCharacterSet characterSetWithCharactersInString:@"0123456789abcdef"] invertedSet]].location==NSNotFound;}
@implementation VNFrameAcquisition {
    NSLock *_lock;NSMutableDictionary<NSString *,NSDictionary *> *_records;BOOL _active;uint64_t _epoch,_generation;
}
- (instancetype)init{if((self=[super init])){_lock=[NSLock new];_records=[NSMutableDictionary new];}return self;}
- (void)activateEpoch:(uint64_t)e generation:(uint64_t)g{[_lock lock];[_records removeAllObjects];_epoch=e;_generation=g;_active=e!=0;[_lock unlock];}
- (void)deactivate{[_lock lock];_active=NO;[_records removeAllObjects];[_lock unlock];}
- (BOOL)requireFocusReady:(BOOL)ready applicationActive:(BOOL)active windowKey:(BOOL)key firstResponder:(BOOL)responder{BOOL allowed=ready&&VNInputMayDispatch(active,key,responder);if(!allowed)[self deactivate];return allowed;}
- (void)invalidateFrames{[_lock lock];[_records removeAllObjects];[_lock unlock];}
- (void)prune:(double)now{for(NSString *key in [_records allKeys]){NSDictionary *r=_records[key];if(now<[r[@"acquired"] doubleValue]||now>=[r[@"deadline"] doubleValue])[_records removeObjectForKey:key];}}
- (NSString *)issueFromStore:(VNDisplayedFrameStore *)store context:(NSDictionary *)context now:(double)now{
    NSDictionary *frame=[store snapshot];if(!frame||!isfinite(now)||now<=0||!VNFrameUnsignedInteger(context[@"epoch"],NO)||!VNFrameUnsignedInteger(context[@"generation"],YES))return nil;
    [_lock lock];[self prune:now];NSString *identifier=nil;
    if(_active&&[context[@"epoch"] unsignedLongLongValue]==_epoch&&[context[@"generation"] unsignedLongLongValue]==_generation&&_records.count<34){
        unsigned char bytes[16];arc4random_buf(bytes,sizeof(bytes));char hex[33];for(unsigned i=0;i<16;i++)snprintf(hex+i*2,3,"%02x",bytes[i]);identifier=@(hex);
        // No retry on collision. arc4random_buf fails closed at the platform.
        if(_records[identifier])identifier=nil;
        else _records[identifier]=@{@"frame":frame,@"token":VNDisplayedFrameToken(frame),@"epoch":@(_epoch),@"generation":@(_generation),@"acquired":@(now),@"deadline":@(now+.200)};
    }[_lock unlock];return identifier;
}
- (BOOL)claim:(NSString *)identifier token:(NSDictionary *)token epoch:(uint64_t)e sequence:(uint64_t)s generation:(uint64_t)g now:(double)now{
    if(!VNFrameReceiptIdentifier(identifier)||!isfinite(now)||now<=0||!s)return NO;
    [_lock lock];[self prune:now];NSDictionary *r=_records[identifier];BOOL valid=_active&&r&&!r[@"sequence"]&&_epoch==e&&_generation==g&&[r[@"epoch"] unsignedLongLongValue]==e&&[r[@"generation"] unsignedLongLongValue]==g&&[r[@"token"] isEqual:token];
    if(valid){NSMutableDictionary *claimed=[r mutableCopy];claimed[@"sequence"]=@(s);_records[identifier]=[claimed copy];}[_lock unlock];return valid;
}
- (NSDictionary *)consume:(NSString *)identifier token:(NSDictionary *)token epoch:(uint64_t)e sequence:(uint64_t)s store:(VNDisplayedFrameStore *)store now:(double)now failure:(NSString **)failure{
    if(failure)*failure=@"acquisition_invalid_request";
    if(!VNFrameReceiptIdentifier(identifier)||!isfinite(now)||now<=0||!e||!s)return nil;
    [_lock lock];NSDictionary *r=_records[identifier],*result=nil;
    if(failure)*failure=@"acquisition_unavailable";
    if(_active&&r&&_epoch==e&&[r[@"epoch"] unsignedLongLongValue]==e&&[r[@"generation"] unsignedLongLongValue]==_generation&&[r[@"sequence"] unsignedLongLongValue]==s&&[r[@"token"] isEqual:token]){
        // One attempted matched validation consumes the receipt even on expiry or
        // invalidation. Wrong/foreign tuples cannot erase another in-flight event.
        [_records removeObjectForKey:identifier];double deadline=[r[@"deadline"] doubleValue];
        if(now<[r[@"acquired"] doubleValue]||now>=deadline){if(failure)*failure=@"acquisition_expired";}
        else{NSDictionary *current=[store snapshot],*original=r[@"frame"];BOOL same=current!=nil;
            for(NSString *k in @[@"display_instance",@"window_epoch",@"invalidation_epoch",@"source_generation",@"source_width",@"source_height"]){if(![current[k] isEqual:original[k]])same=NO;}
            uint64_t remaining=(uint64_t)floor((deadline-now)*1000000.0);
            if(!same){if(failure)*failure=@"acquisition_invalidated";}
            else if(remaining==0||remaining>200000){if(failure)*failure=@"acquisition_expired";}
            else{result=@{@"valid":@YES,@"frame":original,@"current":(current==original?@YES:@NO),@"acquisition":@{@"version":@1,@"epoch":@(e),@"sequence":@(s),@"remaining_us":@(remaining)}};if(failure)*failure=nil;}
        }
    }[self prune:now];[_lock unlock];return result;
}
- (void)cancel:(NSString *)r{if(!VNFrameReceiptIdentifier(r))return;[_lock lock];[_records removeObjectForKey:r];[_lock unlock];}
- (NSDictionary *)statistics{[_lock lock];NSDictionary *s=@{@"protocol":@1,@"capacity":@34,@"outstanding":@(_records.count),@"authority_active":_active?@YES:@NO,@"delivery_limit_ms":@200};[_lock unlock];return s;}
@end
