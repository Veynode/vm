#import "DisplayedFrame.h"
#include <math.h>
#define CHECK(x) NSCAssert((x),@#x)
int main(void){@autoreleasepool{
    VNDisplayGeometry g={.sourceWidth=2560,.sourceHeight=1440,.drawableWidth=1920,.drawableHeight=1200,.viewBounds={0,0,960,600},.backingBounds={0,0,1920,1200},.viewport={0,60,1920,1080},.viewFlipped=false};
    size_t x=0,y=0;
    CHECK(VNDisplayGeometryValid(g));
    CHECK(VNDisplayMapBackingPoint(g,960,600,&x,&y)&&x==1280&&y==720);
    CHECK(VNDisplayMapBackingPoint(g,0,1140,&x,&y)&&x==0&&y==0);
    CHECK(VNDisplayMapBackingPoint(g,1919.5,60.5,&x,&y)&&x==2559&&y==1439);
    CHECK(!VNDisplayMapBackingPoint(g,1920,600,&x,&y));
    CHECK(!VNDisplayMapBackingPoint(g,960,60,&x,&y));
    CHECK(!VNDisplayMapBackingPoint(g,960,1170,&x,&y));
    CHECK(!VNDisplayMapBackingPoint(g,NAN,600,&x,&y));
    CHECK(!VNDisplayMapBackingPoint(g,960,INFINITY,&x,&y));
    VNDisplayGeometry nonzero=g;nonzero.viewBounds=(VNDisplayRect){5,7,960,600};nonzero.backingBounds=(VNDisplayRect){10,14,1920,1200};
    CHECK(VNDisplayMapBackingPoint(nonzero,970,614,&x,&y)&&x==1280&&y==720);
    nonzero.viewFlipped=true;
    CHECK(VNDisplayMapBackingPoint(nonzero,10,74,&x,&y)&&x==0&&y==0);
    CHECK(!VNDisplayMapBackingPoint(nonzero,10,14,&x,&y));
    VNDisplayGeometry nonRetina=g;nonRetina.backingBounds=(VNDisplayRect){0,0,960,600};
    CHECK(VNDisplayMapBackingPoint(nonRetina,480,300,&x,&y)&&x==1280&&y==720);
    VNDisplayGeometry bad=g;bad.sourceWidth=0;CHECK(!VNDisplayGeometryValid(bad));
    bad=g;bad.drawableHeight=NAN;CHECK(!VNDisplayGeometryValid(bad));
    bad=g;bad.backingBounds.width=0;CHECK(!VNDisplayGeometryValid(bad));
    bad=g;bad.viewport.x=-1;CHECK(!VNDisplayGeometryValid(bad));
    bad=g;bad.viewport.width=1921;CHECK(!VNDisplayGeometryValid(bad));
    bad=g;bad.viewBounds.height=-2;CHECK(!VNDisplayGeometryValid(bad));
    CHECK(VNDisplayGeometryEqual(g,g)&&!VNDisplayGeometryEqual(g,nonzero));

    VNDisplayedFrameStore *store=[VNDisplayedFrameStore new];
    NSDictionary *candidate=[store candidateGeometry:g generation:7 sequence:8 windowEpoch:2 capturedMonotonicUS:1000];
    CHECK(candidate&&[candidate[@"display_instance"] length]==32);
    VNDisplayedFrameStore *other=[VNDisplayedFrameStore new];
    CHECK(![other commitCandidate:candidate presentedTime:9 receivedMonotonic:10]);
    CHECK(![candidate isKindOfClass:NSMutableDictionary.class]);
    CHECK(![store snapshot]);
    CHECK(![store commitCandidate:candidate presentedTime:0 receivedMonotonic:10]);
    CHECK(![store commitCandidate:candidate presentedTime:NAN receivedMonotonic:10]);
    CHECK(![store commitCandidate:candidate presentedTime:9 receivedMonotonic:INFINITY]);
    CHECK([store commitCandidate:candidate presentedTime:9 receivedMonotonic:10]);
    NSDictionary *first=[store snapshot];
    CHECK([first[@"presentation_serial"] intValue]==1&&[first[@"source_captured_monotonic_us"] intValue]==1000);
    CHECK(![store commitCandidate:candidate presentedTime:9 receivedMonotonic:11]);
    CHECK([store snapshot]==first); // Static frame retains its original capture/presentation times.
    NSDictionary *verified=[store validateToken:VNDisplayedFrameToken(first) now:999];
    NSDictionary *decoded=[NSJSONSerialization JSONObjectWithData:[NSJSONSerialization dataWithJSONObject:verified options:0 error:nil] options:0 error:nil];
    CHECK(CFGetTypeID((__bridge CFTypeRef)decoded[@"valid"])==CFBooleanGetTypeID());
    CHECK(CFGetTypeID((__bridge CFTypeRef)decoded[@"current"])==CFBooleanGetTypeID());
    CHECK([store validateToken:VNDisplayedFrameToken(first) now:999]); // Current static never fabricated fresh.
    NSDictionary *second=[store candidateGeometry:g generation:7 sequence:9 windowEpoch:2 capturedMonotonicUS:1500];
    CHECK([store commitCandidate:second presentedTime:10 receivedMonotonic:10.1]);
    CHECK([store validateToken:VNDisplayedFrameToken(first) now:10.2]);
    verified=[store validateToken:VNDisplayedFrameToken(first) now:10.2];
    decoded=[NSJSONSerialization JSONObjectWithData:[NSJSONSerialization dataWithJSONObject:verified options:0 error:nil] options:0 error:nil];
    CHECK(CFGetTypeID((__bridge CFTypeRef)decoded[@"current"])==CFBooleanGetTypeID()&&![decoded[@"current"] boolValue]);
    NSString *reason=nil;CHECK(![store validateToken:VNDisplayedFrameToken(first) now:10.251 failure:&reason]);CHECK([reason isEqual:@"display_historical_expired"]);
    NSMutableDictionary *wrong=[VNDisplayedFrameToken([store snapshot]) mutableCopy];wrong[@"width"]=@1;CHECK(![store validateToken:wrong now:10.2 failure:&reason]);CHECK([reason isEqual:@"display_record_mismatch"]);
    wrong=[VNDisplayedFrameToken([store snapshot]) mutableCopy];wrong[@"window_epoch"]=@YES;CHECK(![store validateToken:wrong now:10.2 failure:&reason]);CHECK([reason isEqual:@"display_invalid_token"]);
    wrong=[VNDisplayedFrameToken([store snapshot]) mutableCopy];wrong[@"display_instance"]=[other instance];CHECK(![store validateToken:wrong now:10.2 failure:&reason]);CHECK([reason isEqual:@"display_instance_mismatch"]);
    wrong=[VNDisplayedFrameToken([store snapshot]) mutableCopy];wrong[@"window_epoch"]=@99;CHECK(![store validateToken:wrong now:10.2 failure:&reason]);CHECK([reason isEqual:@"display_epoch_mismatch"]);
    for(int i=0;i<9;i++)CHECK([store commitCandidate:second presentedTime:10.01+i*.001 receivedMonotonic:10.101+i*.001]);
    CHECK(![store validateToken:VNDisplayedFrameToken(first) now:10.2 failure:&reason]);CHECK([reason isEqual:@"display_history_miss"]); // Count limit independent of age.
    CHECK([store validateToken:VNDisplayedFrameToken([store snapshot]) now:999 failure:&reason]&&!reason);
    [store invalidate];CHECK(![store snapshot]);CHECK(![store validateToken:VNDisplayedFrameToken(first) now:10.2 failure:&reason]);CHECK([reason isEqual:@"display_no_snapshot"]);
    CHECK(![store commitCandidate:candidate presentedTime:11 receivedMonotonic:12]); // Late callback cannot revive retired geometry/source/window.
    NSDictionary *fresh=[store candidateGeometry:nonzero generation:9 sequence:1 windowEpoch:3 capturedMonotonicUS:1500];
    CHECK([store commitCandidate:fresh presentedTime:12 receivedMonotonic:13]);
    CHECK([first[@"source_generation"] intValue]==7); // Prior immutable snapshot stays unchanged.
    CHECK([[store snapshot][@"source_generation"] intValue]==9);
    CHECK(![store commitCandidate:fresh presentedTime:11 receivedMonotonic:14]);
    [store invalidate];[store invalidate];
    CHECK(![store commitCandidate:fresh presentedTime:15 receivedMonotonic:16]&&![store snapshot]);
    CHECK(![store candidateGeometry:bad generation:1 sequence:1 windowEpoch:1 capturedMonotonicUS:1]);
    CHECK(![store candidateGeometry:g generation:0 sequence:1 windowEpoch:1 capturedMonotonicUS:1]);
    CHECK(![store candidateGeometry:g generation:1 sequence:1 windowEpoch:1 capturedMonotonicUS:0]);
    puts("PASS immutable displayed-frame state, late-callback invalidation, static-time preservation and exact Retina/nonzero/letterbox/flip/edge mappings (pure tests; no presentation or input).");return 0;
}}
