#import "DisplayedFrame.h"
#include <math.h>
static bool validRect(VNDisplayRect r){return isfinite(r.x)&&isfinite(r.y)&&isfinite(r.width)&&isfinite(r.height)&&r.width>0&&r.height>0&&r.width<=32768&&r.height<=32768;}
bool VNDisplayGeometryValid(VNDisplayGeometry g){
    return g.sourceWidth>0&&g.sourceHeight>0&&g.sourceWidth<=8192&&g.sourceHeight<=8192&&isfinite(g.drawableWidth)&&isfinite(g.drawableHeight)&&g.drawableWidth>0&&g.drawableHeight>0&&g.drawableWidth<=32768&&g.drawableHeight<=32768&&validRect(g.viewBounds)&&validRect(g.backingBounds)&&validRect(g.viewport)&&g.viewport.x>=0&&g.viewport.y>=0&&g.viewport.x+g.viewport.width<=g.drawableWidth&&g.viewport.y+g.viewport.height<=g.drawableHeight;
}
static bool sameRect(VNDisplayRect a,VNDisplayRect b){return a.x==b.x&&a.y==b.y&&a.width==b.width&&a.height==b.height;}
bool VNDisplayGeometryEqual(VNDisplayGeometry a,VNDisplayGeometry b){return a.sourceWidth==b.sourceWidth&&a.sourceHeight==b.sourceHeight&&a.drawableWidth==b.drawableWidth&&a.drawableHeight==b.drawableHeight&&a.viewFlipped==b.viewFlipped&&sameRect(a.viewBounds,b.viewBounds)&&sameRect(a.backingBounds,b.backingBounds)&&sameRect(a.viewport,b.viewport);}
bool VNDisplayMapBackingPoint(VNDisplayGeometry g,double x,double y,size_t *gx,size_t *gy){
    if(!gx||!gy)return false;*gx=*gy=0;
    if(!VNDisplayGeometryValid(g)||!isfinite(x)||!isfinite(y))return false;
    double dx=(x-g.backingBounds.x)*g.drawableWidth/g.backingBounds.width;
    double dy=(g.viewFlipped?(y-g.backingBounds.y):(g.backingBounds.y+g.backingBounds.height-y))*g.drawableHeight/g.backingBounds.height;
    if(dx<g.viewport.x||dy<g.viewport.y||dx>=g.viewport.x+g.viewport.width||dy>=g.viewport.y+g.viewport.height)return false;
    double sx=floor((dx-g.viewport.x)*g.sourceWidth/g.viewport.width),sy=floor((dy-g.viewport.y)*g.sourceHeight/g.viewport.height);
    if(!isfinite(sx)||!isfinite(sy)||sx<0||sy<0||sx>=g.sourceWidth||sy>=g.sourceHeight)return false;
    *gx=(size_t)sx;*gy=(size_t)sy;return true;
}
static NSDictionary *rectJSON(VNDisplayRect r){return @{@"x":@(r.x),@"y":@(r.y),@"width":@(r.width),@"height":@(r.height)};}
@implementation VNDisplayedFrameStore {
    NSString *_instance;
    uint64_t _revision,_serial;
    double _lastPresented;
    NSDictionary *_snapshot;
    NSMutableArray<NSDictionary *> *_history;
}
- (instancetype)init{if((self=[super init])){_instance=[[NSUUID.UUID.UUIDString lowercaseString] stringByReplacingOccurrencesOfString:@"-" withString:@""];_revision=1;_history=[NSMutableArray new];}return self;}
- (uint64_t)revision{return _revision;}
- (NSString *)instance{return _instance;}
- (void)invalidate{_snapshot=nil;[_history removeAllObjects];if(_revision<UINT64_MAX)_revision++;}
- (NSDictionary *)candidateGeometry:(VNDisplayGeometry)g generation:(uint64_t)generation sequence:(uint64_t)sequence windowEpoch:(uint64_t)windowEpoch capturedMonotonicUS:(int64_t)captured{
    if(_revision==UINT64_MAX||!VNDisplayGeometryValid(g)||!generation||!sequence||!windowEpoch||captured<=0)return nil;
    return @{@"protocol_version":@1,@"display_instance":_instance,@"invalidation_epoch":@(_revision),@"window_epoch":@(windowEpoch),@"source_generation":@(generation),@"source_sequence":@(sequence),@"source_width":@(g.sourceWidth),@"source_height":@(g.sourceHeight),@"source_captured_monotonic_us":@(captured),@"drawable_width":@(g.drawableWidth),@"drawable_height":@(g.drawableHeight),@"view_bounds_points":rectJSON(g.viewBounds),@"backing_bounds_pixels":rectJSON(g.backingBounds),@"viewport_pixels":rectJSON(g.viewport),@"view_is_flipped":g.viewFlipped?@YES:@NO};
}
- (BOOL)commitCandidate:(NSDictionary *)candidate presentedTime:(double)presented receivedMonotonic:(double)received{
    if(!candidate||_revision==UINT64_MAX||_serial==UINT64_MAX||![candidate[@"display_instance"] isEqual:_instance]||[candidate[@"invalidation_epoch"] unsignedLongLongValue]!=_revision||!isfinite(presented)||presented<=0||presented<=_lastPresented||!isfinite(received)||received<=0)return NO;
    NSMutableDictionary *value=[candidate mutableCopy];value[@"presentation_serial"]=@(++_serial);value[@"metal_presented_time_seconds"]=@(presented);value[@"callback_received_monotonic_seconds"]=@(received);_lastPresented=presented;_snapshot=[value copy];[_history addObject:_snapshot];while(_history.count>8)[_history removeObjectAtIndex:0];return YES;
}
- (NSDictionary *)snapshot{return _snapshot;}
- (NSDictionary *)diagnosticToken:(NSDictionary *)token now:(double)now{
    NSDictionary *matched=nil;for(NSDictionary *f in [_history reverseObjectEnumerator])if([VNDisplayedFrameToken(f) isEqual:token]){matched=f;break;}
    double received=[matched[@"callback_received_monotonic_seconds"] doubleValue];
    return @{@"monotonic_seconds":isfinite(now)?@(now):[NSNull null],@"record_found":matched?@YES:@NO,@"current":matched?(matched==_snapshot?@YES:@NO):[NSNull null],@"callback_received_monotonic_seconds":matched?@(received):[NSNull null],@"callback_age_ms":matched&&isfinite(now)?@((now-received)*1000):[NSNull null]};
}
- (NSDictionary *)validateToken:(NSDictionary *)token now:(double)now{return [self validateToken:token now:now failure:NULL];}
- (NSDictionary *)validateToken:(NSDictionary *)token now:(double)now failure:(NSString **)failure{
    if(failure)*failure=@"display_invalid_token";
    if(!isfinite(now)||now<=0||![token isKindOfClass:NSDictionary.class]||token.count!=8)return nil;
    for(NSString *key in @[@"presentation_serial",@"window_epoch",@"invalidation_epoch",@"source_generation",@"source_sequence",@"width",@"height"]){
        id value=token[key];if(![value isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)value)==CFBooleanGetTypeID()||[value doubleValue]<=0||[value doubleValue]!=floor([value doubleValue]))return nil;
    }
    BOOL expiredMatch=NO,serialSeen=NO;
    for(NSDictionary *frame in [_history reverseObjectEnumerator]){
        double received=[frame[@"callback_received_monotonic_seconds"] doubleValue];
        BOOL matches=[VNDisplayedFrameToken(frame) isEqual:token];
        if([frame[@"presentation_serial"] isEqual:token[@"presentation_serial"]])serialSeen=YES;
        if(frame!=_snapshot&&(now<received||now-received>0.250)){if(matches)expiredMatch=YES;continue;}
        if(matches){if(failure)*failure=nil;return @{@"valid":@YES,@"frame":frame,@"current":(frame==_snapshot?@YES:@NO)};}
    }
    // Classification only after the original validation rejected. History miss
    // does not assert that this token ever existed, or that it was evicted.
    if(failure){
        if(!_snapshot)*failure=@"display_no_snapshot";
        else if(![token[@"display_instance"] isEqual:_instance])*failure=@"display_instance_mismatch";
        else if(![token[@"invalidation_epoch"] isEqual:_snapshot[@"invalidation_epoch"]]||![token[@"window_epoch"] isEqual:_snapshot[@"window_epoch"]])*failure=@"display_epoch_mismatch";
        else if(expiredMatch)*failure=@"display_historical_expired";
        else *failure=serialSeen?@"display_record_mismatch":@"display_history_miss";
    }
    return nil;
}
@end

NSDictionary *VNDisplayedFrameToken(NSDictionary *f){
    if(!f)return nil;
    return @{@"display_instance":f[@"display_instance"],@"presentation_serial":f[@"presentation_serial"],@"window_epoch":f[@"window_epoch"],@"invalidation_epoch":f[@"invalidation_epoch"],@"source_generation":f[@"source_generation"],@"source_sequence":f[@"source_sequence"],@"width":f[@"source_width"],@"height":f[@"source_height"]};
}
static VNDisplayRect readRect(NSDictionary *d){return (VNDisplayRect){[d[@"x"] doubleValue],[d[@"y"] doubleValue],[d[@"width"] doubleValue],[d[@"height"] doubleValue]};}
bool VNDisplayedFrameGeometry(NSDictionary *f,VNDisplayGeometry *g){
    if(!f||!g)return false;
    *g=(VNDisplayGeometry){.sourceWidth=[f[@"source_width"] unsignedIntegerValue],.sourceHeight=[f[@"source_height"] unsignedIntegerValue],.drawableWidth=[f[@"drawable_width"] doubleValue],.drawableHeight=[f[@"drawable_height"] doubleValue],.viewBounds=readRect(f[@"view_bounds_points"]),.backingBounds=readRect(f[@"backing_bounds_pixels"]),.viewport=readRect(f[@"viewport_pixels"]),.viewFlipped=[f[@"view_is_flipped"] boolValue]};
    return VNDisplayGeometryValid(*g);
}
