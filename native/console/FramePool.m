#import "FramePool.h"
#import <IOSurface/IOSurface.h>

@implementation VNFramePool {
    NSLock *_lock;
    CVPixelBufferRef _pixels[3];
    NSUInteger _leases[3];
    NSUInteger _dropped;
}
- (instancetype)init { if((self=[super init]))_lock=[NSLock new];return self; }
- (void)dealloc { for(int i=0;i<3;i++)if(_pixels[i])CVPixelBufferRelease(_pixels[i]); }
- (CVPixelBufferRef)acquireWidth:(NSUInteger)width height:(NSUInteger)height {
    if(!width||!height||width>3840||height>2160||width%2||height%2)return NULL;
    [_lock lock];
    for(int i=0;i<3;i++)if(!_leases[i]) {
        if(_pixels[i]&&(CVPixelBufferGetWidth(_pixels[i])!=width||CVPixelBufferGetHeight(_pixels[i])!=height)){CVPixelBufferRelease(_pixels[i]);_pixels[i]=NULL;}
        if(!_pixels[i]) {
            NSDictionary *attrs=@{(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{},(__bridge NSString *)kCVPixelBufferMetalCompatibilityKey:@YES};
            if(CVPixelBufferCreate(NULL,width,height,kCVPixelFormatType_32BGRA,(__bridge CFDictionaryRef)attrs,&_pixels[i])!=kCVReturnSuccess){[_lock unlock];return NULL;}
        }
        _leases[i]=1;CVPixelBufferRef result=_pixels[i];[_lock unlock];return result;
    }
    _dropped++;[_lock unlock];return NULL;
}
- (BOOL)retainPixel:(CVPixelBufferRef)pixel {
    [_lock lock];for(int i=0;i<3;i++)if(pixel&&pixel==_pixels[i]&&_leases[i]>0&&_leases[i]<8){_leases[i]++;[_lock unlock];return YES;}
    [_lock unlock];return NO;
}
- (void)releasePixel:(CVPixelBufferRef)pixel {
    [_lock lock];BOOL found=NO;for(int i=0;i<3;i++)if(pixel&&pixel==_pixels[i]&&_leases[i]){_leases[i]--;found=YES;break;}
    [_lock unlock];NSCAssert(found,@"unbalanced owned-frame lease");
}
- (NSDictionary *)statistics {
    [_lock lock];NSUInteger occupied=0,leases=0,allocated=0,bytes=0;
    for(int i=0;i<3;i++){if(_leases[i])occupied++;leases+=_leases[i];if(_pixels[i]){allocated++;bytes+=CVPixelBufferGetDataSize(_pixels[i]);}}
    NSDictionary *result=@{@"slot_limit":@3,@"occupied_slots":@(occupied),@"leases":@(leases),@"allocated_slots":@(allocated),@"allocated_bytes":@(bytes),@"pool_exhausted_drops":@(_dropped)};[_lock unlock];return result;
}
@end
