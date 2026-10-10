#import "CopyTiming.h"
#include <assert.h>
#include <float.h>
#include <math.h>
static NSDictionary *kind(VNCopyTiming *s,NSString *name){return [s statistics][name];}
int main(void){@autoreleasepool{
 VNCopyTiming *s=[VNCopyTiming new];NSDictionary *empty=kind(s,@"source_copy");
 assert([empty[@"window_samples"] unsignedIntegerValue]==0);assert(empty[@"gpu_execution_us"][@"p50"]==NSNull.null);
 for(int i=1;i<=125;i++)[s recordKind:VNCopySource cpuSeconds:i/1e6 commitWaitSeconds:i/2e6 gpuStartSeconds:100 gpuEndSeconds:100+i/4e6 succeeded:YES];
 NSDictionary *a=kind(s,@"source_copy");assert([a[@"attempts"] intValue]==125);assert([a[@"succeeded"] intValue]==125);assert([a[@"window_samples"] intValue]==120);
 assert(fabs([a[@"cpu_total_us"][@"min"] doubleValue]-6)<1e-6);assert(fabs([a[@"cpu_total_us"][@"p50"] doubleValue]-65)<1e-6);assert(fabs([a[@"cpu_total_us"][@"p95"] doubleValue]-119)<1e-6);assert(fabs([a[@"cpu_total_us"][@"p99"] doubleValue]-124)<1e-6);assert(fabs([a[@"cpu_total_us"][@"max"] doubleValue]-125)<1e-6);
 assert([kind(s,@"encoder_resize")[@"attempts"] intValue]==0);
 [s recordKind:VNCopyEncoderResize cpuSeconds:NAN commitWaitSeconds:-1 gpuStartSeconds:0 gpuEndSeconds:1 succeeded:YES];
 [s recordKind:VNCopyEncoderResize cpuSeconds:DBL_MAX commitWaitSeconds:INFINITY gpuStartSeconds:2 gpuEndSeconds:1 succeeded:NO];
 [s recordKind:VNCopyEncoderResize cpuSeconds:0 commitWaitSeconds:0 gpuStartSeconds:1 gpuEndSeconds:INFINITY succeeded:YES];
 [s recordKind:VNCopyEncoderResize cpuSeconds:1 commitWaitSeconds:1 gpuStartSeconds:1 gpuEndSeconds:2 succeeded:NO];
 NSDictionary *b=kind(s,@"encoder_resize");assert([b[@"attempts"] intValue]==4);assert([b[@"failed"] intValue]==2);assert([b[@"gpu_execution_us"][@"invalid_count"] intValue]==4);assert(b[@"gpu_execution_us"][@"max"]==NSNull.null);assert([b[@"cpu_total_us"][@"valid_count"] intValue]==2);assert([b[@"cpu_total_us"][@"invalid_count"] intValue]==2);
 [s recordKind:(VNCopyKind)99 cpuSeconds:1 commitWaitSeconds:1 gpuStartSeconds:1 gpuEndSeconds:2 succeeded:YES];assert([kind(s,@"encoder_resize")[@"attempts"] intValue]==4);
 NSData *data=[NSJSONSerialization dataWithJSONObject:[s statistics] options:0 error:nil];assert(data&&data.length<8192);NSDictionary *round=[NSJSONSerialization JSONObjectWithData:data options:0 error:nil];assert(CFGetTypeID((__bridge CFTypeRef)round[@"physical_presentation_measured"])==CFBooleanGetTypeID());assert([round[@"sample_limit_per_kind"] intValue]==120);
 puts("PASS copy timing finite rolling bounds, nearest-rank, invalid GPU/CPU, kind isolation and typed JSON (no GPU executed)");
}return 0;}
