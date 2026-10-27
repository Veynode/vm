// Fixed, non-sensitive screenshot failure metadata; no pixels, paths or dynamic errors.
#import <Foundation/Foundation.h>
#define VEYNODE_SCREENSHOT_MAX_BYTES (8u * 1024u * 1024u)
static inline NSDictionary *VNScreenshotFailure(NSString *stage, NSNumber *encodedBytes) {
    return @{ @"version": @1, @"stage": stage,
              @"encoded_bytes": encodedBytes ?: [NSNull null],
              @"maximum_encoded_bytes": @(VEYNODE_SCREENSHOT_MAX_BYTES) };
}
static inline NSDictionary *VNScreenshotEncodingFailure(BOOL encoded, NSUInteger bytes, BOOL cacheValid) {
    if (!encoded) return VNScreenshotFailure(@"png_finalize_failed", @(bytes));
    if (bytes > VEYNODE_SCREENSHOT_MAX_BYTES) return VNScreenshotFailure(@"encoded_size_limit", @(bytes));
    if (!cacheValid) return VNScreenshotFailure(@"cache_invalid_after_encode", @(bytes));
    return nil;
}

// Base64 slash escaping can otherwise double an otherwise bounded PNG response.
// This affects only screenshot JSON; content and lossless pixel bytes are unchanged.
static inline NSData *VNScreenshotResponseData(id ident, NSDictionary *result) {
    return [NSJSONSerialization dataWithJSONObject:@{@"id":ident,@"result":result}
        options:NSJSONWritingSortedKeys|NSJSONWritingWithoutEscapingSlashes error:nil];
}
