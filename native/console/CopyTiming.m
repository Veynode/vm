#import "CopyTiming.h"
#include <math.h>
#define VN_COPY_LIMIT 120
#define VN_COPY_COUNTER_MAX UINT64_C(9007199254740991)
typedef struct { double cpu, wait, gpu; } VNCopySample;
typedef struct { VNCopySample values[VN_COPY_LIMIT]; NSUInteger next,count; uint64_t attempts,succeeded,failed; BOOL saturated; } VNCopyWindow;
static double validDuration(double x) { return isfinite(x)&&x>=0?x:NAN; }
static void increment(uint64_t *value,BOOL *saturated) { if(*value<VN_COPY_COUNTER_MAX)(*value)++;else *saturated=YES; }
static NSDictionary *summary(VNCopyWindow *w,NSUInteger field) {
    double values[VN_COPY_LIMIT];NSUInteger n=0;
    for(NSUInteger i=0;i<w->count;i++) { VNCopySample s=w->values[i];double x=field==0?s.cpu:field==1?s.wait:s.gpu;if(isfinite(x))values[n++]=x*1e6; }
    // Fixed-size insertion sort: bounded work at status time, no per-frame allocation.
    for(NSUInteger i=1;i<n;i++){double x=values[i];NSUInteger j=i;while(j&&values[j-1]>x){values[j]=values[j-1];j--;}values[j]=x;}
    if(!n)return @{@"valid_count":@0,@"invalid_count":@(w->count),@"min":NSNull.null,@"p50":NSNull.null,@"p95":NSNull.null,@"p99":NSNull.null,@"max":NSNull.null};
    return @{@"valid_count":@(n),@"invalid_count":@(w->count-n),@"min":@(values[0]),@"p50":@(values[(NSUInteger)ceil(.50*n)-1]),@"p95":@(values[(NSUInteger)ceil(.95*n)-1]),@"p99":@(values[(NSUInteger)ceil(.99*n)-1]),@"max":@(values[n-1])};
}
@implementation VNCopyTiming { NSLock *_lock; VNCopyWindow _windows[2]; }
- (instancetype)init { self=[super init];if(self)_lock=[NSLock new];return self; }
- (void)recordKind:(VNCopyKind)kind cpuSeconds:(double)cpu commitWaitSeconds:(double)wait gpuStartSeconds:(double)start gpuEndSeconds:(double)end succeeded:(BOOL)success {
    if(kind>VNCopyEncoderResize)return;
    // Protect JSON units conversion from finite overflow, too.
    cpu=validDuration(cpu);wait=validDuration(wait);
    double gpu=success&&isfinite(start)&&isfinite(end)&&start>0&&end>=start?validDuration(end-start):NAN;
    if(!isfinite(cpu*1e6))cpu=NAN;if(!isfinite(wait*1e6))wait=NAN;if(!isfinite(gpu*1e6))gpu=NAN;
    [_lock lock];VNCopyWindow *w=&_windows[kind];increment(&w->attempts,&w->saturated);increment(success?&w->succeeded:&w->failed,&w->saturated);
    w->values[w->next]=(VNCopySample){cpu,wait,gpu};w->next=(w->next+1)%VN_COPY_LIMIT;if(w->count<VN_COPY_LIMIT)w->count++;[_lock unlock];
}
- (NSDictionary *)statistics {
    [_lock lock];NSMutableDictionary *out=[@{@"version":@1,@"sample_limit_per_kind":@VN_COPY_LIMIT,@"units":@"microseconds",@"percentile":@"nearest-rank",@"physical_presentation_measured":@NO,@"scope":@"copyPixels CPU interval, commit through synchronous wait return, completed command GPU execution; independent rolling windows"} mutableCopy];
    for(NSUInteger i=0;i<2;i++){VNCopyWindow *w=&_windows[i];out[i==0?@"source_copy":@"encoder_resize"]=@{@"attempts":@(w->attempts),@"succeeded":@(w->succeeded),@"failed":@(w->failed),@"counters_saturated":w->saturated?@YES:@NO,@"window_samples":@(w->count),@"cpu_total_us":summary(w,0),@"commit_wait_us":summary(w,1),@"gpu_execution_us":summary(w,2)};}
    [_lock unlock];return out;
}
@end
