#import "Presented.h"
#include <math.h>
@implementation VNPresentedSamples {
    NSMutableArray *_samples;
    NSUInteger _callbacks,_valid,_invalid,_evicted;
    double _last;
}
- (instancetype)init{if((self=[super init]))_samples=[NSMutableArray new];return self;}
- (void)recordTime:(double)time generation:(uint64_t)generation sequence:(uint64_t)sequence windowEpoch:(uint64_t)windowEpoch{
    _callbacks++;
    if(!isfinite(time)||time<=0||time<=_last){_invalid++;return;}
    _last=time;_valid++;
    if(_samples.count==120){[_samples removeObjectAtIndex:0];_evicted++;}
    [_samples addObject:@{@"metal_presented_time_seconds":@(time),@"source_generation":@(generation),@"source_sequence":@(sequence),@"source_frame_available":(generation&&sequence?@YES:@NO),@"window_epoch":@(windowEpoch)}];
}
- (NSDictionary *)statistics{return @{@"presented_callback_count":@(_callbacks),@"presented_time_valid_count":@(_valid),@"presented_time_invalid_count":@(_invalid),@"presented_time_evicted_count":@(_evicted),@"presented_time_samples":[_samples copy]};}
@end
