#import "ScreenshotDiagnostic.h"
#include <assert.h>
int main(void) { @autoreleasepool {
    assert(VNScreenshotEncodingFailure(YES,8*1024*1024,YES)==nil);
    NSDictionary *large=VNScreenshotEncodingFailure(YES,8*1024*1024+1,YES);
    assert([large[@"stage"] isEqual:@"encoded_size_limit"]);
    assert([large[@"encoded_bytes"] unsignedIntegerValue]==8*1024*1024+1);
    assert([large[@"maximum_encoded_bytes"] unsignedIntegerValue]==8*1024*1024);
    assert([VNScreenshotEncodingFailure(NO,33,YES)[@"stage"] isEqual:@"png_finalize_failed"]);
    assert([VNScreenshotEncodingFailure(YES,33,NO)[@"stage"] isEqual:@"cache_invalid_after_encode"]);
    NSDictionary *early=VNScreenshotFailure(@"cache_invalid_before_copy",nil);
    assert(early[@"encoded_bytes"]==[NSNull null]);
    for (NSDictionary *d in @[large,early]) {
        NSData *encoded=[NSJSONSerialization dataWithJSONObject:d options:0 error:nil];
        assert(encoded && encoded.length<256);
        NSDictionary *decoded=[NSJSONSerialization JSONObjectWithData:encoded options:0 error:nil];
        assert(decoded.count==4 && [decoded isEqual:d]);
    }
    NSMutableData *pixels=[NSMutableData dataWithLength:2*1024*1024];memset(pixels.mutableBytes,255,pixels.length);
    NSString *base64=[pixels base64EncodedStringWithOptions:0];
    NSData *response=VNScreenshotResponseData(@"test",@{@"data_base64":base64});
    assert(response.length<3*1024*1024);
    NSDictionary *parsed=[NSJSONSerialization JSONObjectWithData:response options:0 error:nil];
    assert([parsed[@"result"][@"data_base64"] isEqual:base64]);
    puts("screenshot diagnostic: exact limit, encode failure, stale cache, bounded JSON PASS");
} return 0; }
