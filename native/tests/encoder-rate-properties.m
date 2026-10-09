// Actual hardware-session property metadata/readback; no frames or capture.
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
static NSDictionary *propertySnapshot(VTCompressionSessionRef session) {
    NSMutableDictionary *values=[NSMutableDictionary new];
    for(NSString *key in @[(__bridge NSString *)kVTCompressionPropertyKey_MaxAllowedFrameQP,
                          (__bridge NSString *)kVTCompressionPropertyKey_MinAllowedFrameQP,
                          (__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate,
                          (__bridge NSString *)kVTCompressionPropertyKey_DataRateLimits]) {
        CFTypeRef raw=NULL;
        OSStatus status=VTSessionCopyProperty(session,(__bridge CFStringRef)key,NULL,&raw);
        id value=CFBridgingRelease(raw);
        // A failed read is preserved explicitly, never interpreted as a default.
        values[key]=@{@"status":@(status),@"value":value?:[NSNull null]};
    }
    return values;
}
static NSDictionary *hardwareSnapshot(VTCompressionSessionRef session) {
    CFTypeRef value=NULL;
    OSStatus status=VTSessionCopyProperty(session,kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,NULL,&value);
    BOOL hardware=status==noErr&&value&&CFGetTypeID(value)==CFBooleanGetTypeID()&&CFBooleanGetValue(value);
    if(value)CFRelease(value);
    return @{@"status":@(status),@"verified":hardware?@YES:@NO};
}
int main(void){@autoreleasepool{
    NSMutableArray *rows=[NSMutableArray new];BOOL passed=YES;
    for(NSString *codec in @[@"h264",@"hevc"]){
        BOOL hevc=[codec isEqual:@"hevc"];
        int32_t width=hevc?1920:1280,height=hevc?1080:720;
        VTCompressionSessionRef session=NULL;
        NSDictionary *attributes=@{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey:@(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),(__bridge NSString *)kCVPixelBufferWidthKey:@(width),(__bridge NSString *)kCVPixelBufferHeightKey:@(height),(__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey:@{}};
        OSStatus created=VTCompressionSessionCreate(NULL,width,height,hevc?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,(__bridge CFDictionaryRef)@{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:@YES},(__bridge CFDictionaryRef)attributes,NULL,NULL,NULL,&session);
        NSMutableDictionary *row=[@{@"codec":codec,@"width":@(width),@"height":@(height),@"creation_status":@(created)} mutableCopy];
        if(created!=noErr||!session){passed=NO;[rows addObject:row];continue;}
        NSDictionary *before=hardwareSnapshot(session);row[@"hardware_before"]=before;
        if(![before[@"verified"] boolValue]){passed=NO;VTCompressionSessionInvalidate(session);CFRelease(session);[rows addObject:row];continue;}
        CFDictionaryRef properties=NULL;OSStatus supported=VTSessionCopySupportedPropertyDictionary(session,&properties);
        row[@"supported_property_status"]=@(supported);NSDictionary *all=CFBridgingRelease(properties);
        NSMutableDictionary *selected=[NSMutableDictionary new];
        for(NSString *key in @[(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate,(__bridge NSString *)kVTCompressionPropertyKey_DataRateLimits,(__bridge NSString *)kVTCompressionPropertyKey_MaxAllowedFrameQP,(__bridge NSString *)kVTCompressionPropertyKey_MinAllowedFrameQP])selected[key]=all[key]?:[NSNull null];
        row[@"supported_properties"]=selected;row[@"defaults"]=propertySnapshot(session);
        // Same values as the direct production encoder, deliberately no QP,
        // DataRateLimits, CBR/VBR, low-latency flag or EncodeFrame invocation.
        NSDictionary *setup=@{(__bridge NSString *)kVTCompressionPropertyKey_ColorPrimaries:(__bridge NSString *)kCVImageBufferColorPrimaries_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_TransferFunction:(__bridge NSString *)kCVImageBufferTransferFunction_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_YCbCrMatrix:(__bridge NSString *)kCVImageBufferYCbCrMatrix_ITU_R_709_2,(__bridge NSString *)kVTCompressionPropertyKey_RealTime:@YES,(__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering:@NO,(__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate:@8000000,(__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate:@30,(__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval:@30,(__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel:(__bridge NSString *)(hevc?kVTProfileLevel_HEVC_Main_AutoLevel:kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
        NSMutableArray *setters=[NSMutableArray new];
        for(NSString *key in setup){OSStatus status=VTSessionSetProperty(session,(__bridge CFStringRef)key,(__bridge CFTypeRef)setup[key]);[setters addObject:@{@"key":key,@"value":setup[key],@"status":@(status)}];if(status!=noErr)passed=NO;}
        row[@"setup_setters_in_call_order"]=setters;
        OSStatus prepare=VTCompressionSessionPrepareToEncodeFrames(session);row[@"prepare_status"]=@(prepare);if(prepare!=noErr)passed=NO;
        row[@"after_setup"]=propertySnapshot(session);row[@"hardware_after"]=hardwareSnapshot(session);if(![row[@"hardware_after"][@"verified"] boolValue])passed=NO;
        VTCompressionSessionInvalidate(session);CFRelease(session);[rows addObject:row];
    }
    NSDictionary *report=@{@"schema":@1,@"passed":passed?@YES:@NO,@"scope":@"Actual required-hardware VT property-only diagnostic; zero frame submissions, capture, window or bitrate efficacy acceptance",@"frame_submissions":@0,@"codecs":rows};
    NSError *error=nil;NSData *json=[NSJSONSerialization dataWithJSONObject:report options:NSJSONWritingPrettyPrinted|NSJSONWritingSortedKeys error:&error];if(!json)return 2;
    fwrite(json.bytes,1,json.length,stdout);fputc('\n',stdout);return passed?0:1;
}}
