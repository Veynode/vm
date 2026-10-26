// Apple Virtualization Linux desktop -> hardware VideoToolbox -> framed Annex B.
// The guest agent sends changed BGRX rectangles of its DRM scanout over the
// worker's vsock 5001 bridge (protocol in guest/veynode-agent/desktop_linux.go);
// this helper patches them into a host canvas and encodes once per guest frame.
//
// Usage: vz-desktop-stream PRIVATE_UNIX_SOCKET --codec hevc|h264
// stdout: u32 BE size, u64 BE pts_us, u8 keyframe, Annex B access unit
// stdin:  "stop", "keyframe", or a JSON {"input":[...]} line forwarded to the guest
// stderr: JSON diagnostics
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <errno.h>
#include <time.h>

static const size_t maxAU = 4 * 1024 * 1024, maxSide = 4096, tile = 64, maxLine = 4096;
static const size_t maxRecord = 1 + 8 + maxSide * tile * 4;
static VTCompressionSessionRef encoder;
static CVPixelBufferPoolRef pool;
static BOOL useHEVC, forceKeyframe, dirty, failed, stopping, settled = YES;
static size_t width, height;
static uint8_t *canvas;
static int64_t epoch, lastChange;
static NSLock *outputLock;

static int64_t nowUs(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (int64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000; }
static void diagnostic(NSDictionary *value) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:NULL];
    fwrite(data.bytes, 1, data.length, stderr); fputc('\n', stderr); fflush(stderr);
}
static void fail(NSString *message) { if (!failed) diagnostic(@{@"event": @"error", @"message": message}); failed = YES; }

static BOOL writeAll(int fd, const uint8_t *bytes, size_t length) {
    while (length) {
        ssize_t n = write(fd, bytes, length);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return NO;
        bytes += n; length -= (size_t)n;
    }
    return YES;
}
static BOOL appendNAL(NSMutableData *au, const void *bytes, size_t count) {
    static const uint8_t start[] = {0, 0, 0, 1};
    if (count == 0 || au.length + count + 4 > maxAU) return NO;
    [au appendBytes:start length:4]; [au appendBytes:bytes length:count]; return YES;
}

static void compressed(void *context, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample) {
    @autoreleasepool {
        if (status != noErr || !sample || (flags & kVTEncodeInfo_FrameDropped)) return;
        CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
        CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, NO);
        BOOL key = YES;
        if (attachments && CFArrayGetCount(attachments)) {
            CFDictionaryRef dict = CFArrayGetValueAtIndex(attachments, 0);
            key = !CFDictionaryContainsKey(dict, kCMSampleAttachmentKey_NotSync) || CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync) == kCFBooleanFalse;
        }
        NSMutableData *au = [NSMutableData new];
        if (key) {
            size_t count = 0; int lengthSize = 0; const uint8_t *parameter; size_t size;
            OSStatus r = useHEVC ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, 0, NULL, NULL, &count, &lengthSize)
                                 : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, NULL, NULL, &count, &lengthSize);
            if (r != noErr) return;
            for (size_t i = 0; i < count; i++) {
                r = useHEVC ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, i, &parameter, &size, NULL, NULL)
                            : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &parameter, &size, NULL, NULL);
                if (r != noErr || !appendNAL(au, parameter, size)) return;
            }
        }
        CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
        size_t total = block ? CMBlockBufferGetDataLength(block) : 0, offset = 0;
        if (!total || total > maxAU) return;
        NSMutableData *bytes = [NSMutableData dataWithLength:total];
        if (CMBlockBufferCopyDataBytes(block, 0, total, bytes.mutableBytes) != noErr) return;
        const uint8_t *raw = bytes.bytes;
        while (offset + 4 <= total) {
            uint32_t size; memcpy(&size, raw + offset, 4); size = CFSwapInt32BigToHost(size); offset += 4;
            if (size > total - offset || !appendNAL(au, raw + offset, size)) return;
            offset += size;
        }
        CMTime pts = CMTimeConvertScale(CMSampleBufferGetPresentationTimeStamp(sample), 1000000, kCMTimeRoundingMethod_Default);
        uint8_t header[13]; uint32_t length = CFSwapInt32HostToBig((uint32_t)au.length); uint64_t stamp = CFSwapInt64HostToBig((uint64_t)MAX(pts.value, 0));
        memcpy(header, &length, 4); memcpy(header + 4, &stamp, 8); header[12] = key;
        [outputLock lock];
        BOOL ok = writeAll(STDOUT_FILENO, header, sizeof header) && writeAll(STDOUT_FILENO, au.bytes, au.length);
        [outputLock unlock];
        if (!ok) { fail(@"stdout closed"); kill(getpid(), SIGTERM); }
    }
}

static void destroyEncoder(void) {
    if (encoder) { VTCompressionSessionCompleteFrames(encoder, kCMTimeInvalid); VTCompressionSessionInvalidate(encoder); CFRelease(encoder); encoder = NULL; }
    if (pool) { CVPixelBufferPoolRelease(pool); pool = NULL; }
}

static BOOL createEncoder(void) {
    destroyEncoder();
    NSDictionary *spec = @{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: @YES};
    NSDictionary *attributes = @{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
                                 (__bridge NSString *)kCVPixelBufferWidthKey: @(width), (__bridge NSString *)kCVPixelBufferHeightKey: @(height),
                                 (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey: @{}};
    OSStatus r = VTCompressionSessionCreate(NULL, (int32_t)width, (int32_t)height, useHEVC ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264,
                                            (__bridge CFDictionaryRef)spec, (__bridge CFDictionaryRef)attributes, NULL, compressed, NULL, &encoder);
    if (r != noErr) { encoder = NULL; fail([NSString stringWithFormat:@"hardware %@ encoder unavailable at %zux%zu: %d", useHEVC ? @"HEVC" : @"H.264", width, height, (int)r]); return NO; }
    // Same latency settings as the QEMU console helper (native/tools/spice-console.m).
    NSDictionary *properties = @{(__bridge NSString *)kVTCompressionPropertyKey_RealTime: @YES,
                                 (__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering: @NO,
                                 (__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate: @8000000,
                                 (__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate: @60,
                                 (__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval: @120,
                                 (__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel: (__bridge NSString *)(useHEVC ? kVTProfileLevel_HEVC_Main_AutoLevel : kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
    for (NSString *key in properties) {
        if ((r = VTSessionSetProperty(encoder, (__bridge CFStringRef)key, (__bridge CFTypeRef)properties[key])) != noErr) {
            fail([NSString stringWithFormat:@"encoder property rejected %@: %d", key, (int)r]); destroyEncoder(); return NO;
        }
    }
    VTSessionSetProperty(encoder, kVTCompressionPropertyKey_MaxFrameDelayCount, (__bridge CFTypeRef)@0);
    VTSessionSetProperty(encoder, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue);
    if (VTCompressionSessionPrepareToEncodeFrames(encoder) != noErr) { fail(@"encoder preparation failed"); destroyEncoder(); return NO; }
    CFTypeRef hardware = NULL;
    BOOL verified = VTSessionCopyProperty(encoder, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hardware) == noErr
                    && hardware && CFGetTypeID(hardware) == CFBooleanGetTypeID() && CFBooleanGetValue(hardware);
    if (hardware) CFRelease(hardware);
    if (!verified) { fail(@"encoder did not confirm hardware acceleration"); destroyEncoder(); return NO; }
    NSDictionary *poolAttributes = @{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
                                     (__bridge NSString *)kCVPixelBufferWidthKey: @(width), (__bridge NSString *)kCVPixelBufferHeightKey: @(height),
                                     (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey: @{}};
    if (CVPixelBufferPoolCreate(NULL, NULL, (__bridge CFDictionaryRef)poolAttributes, &pool) != kCVReturnSuccess) { fail(@"pixel buffer pool failed"); destroyEncoder(); return NO; }
    forceKeyframe = YES;
    return YES;
}

// Copy the canvas into a fresh pooled buffer so the encoder never reads
// pixels that the next guest rectangle is overwriting.
static void encode(void) {
    if (!encoder || !canvas) return;
    CVPixelBufferRef pixel = NULL;
    if (CVPixelBufferPoolCreatePixelBuffer(NULL, pool, &pixel) != kCVReturnSuccess) { fail(@"pixel buffer pool exhausted"); return; }
    CVPixelBufferLockBaseAddress(pixel, 0);
    uint8_t *base = CVPixelBufferGetBaseAddress(pixel); size_t stride = CVPixelBufferGetBytesPerRow(pixel);
    for (size_t y = 0; y < height; y++) memcpy(base + y * stride, canvas + y * width * 4, width * 4);
    CVPixelBufferUnlockBaseAddress(pixel, 0);
    NSDictionary *options = forceKeyframe ? @{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame: @YES} : nil;
    int64_t now = nowUs();
    OSStatus r = VTCompressionSessionEncodeFrame(encoder, pixel, CMTimeMake(now - epoch, 1000000), kCMTimeInvalid, (__bridge CFDictionaryRef)options, NULL, NULL);
    CVPixelBufferRelease(pixel);
    if (r != noErr) { fail([NSString stringWithFormat:@"encode failed: %d", (int)r]); return; }
    forceKeyframe = NO; dirty = NO;
}

static int connectPrivate(const char *path) {
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    if (strlen(path) >= sizeof address.sun_path) return -1;
    strcpy(address.sun_path, path);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    // The worker bridge connects to the guest on demand; give it a moment.
    for (int attempt = 0; attempt < 50; attempt++) {
        if (connect(fd, (struct sockaddr *)&address, sizeof address) == 0) return fd;
        if (errno != ENOENT && errno != ECONNREFUSED) break;
        usleep(100000);
    }
    close(fd);
    return -1;
}

static BOOL handleRecord(const uint8_t *body, size_t length) {
    uint8_t kind = body[0]; body++; length--;
    if (kind == 1) {
        NSDictionary *event = [NSJSONSerialization JSONObjectWithData:[NSData dataWithBytes:body length:length] options:0 error:NULL];
        if (![event isKindOfClass:NSDictionary.class]) { fail(@"malformed guest event"); return NO; }
        if ([event[@"event"] isEqual:@"mode"]) {
            NSNumber *w = event[@"width"], *h = event[@"height"];
            if (![w isKindOfClass:NSNumber.class] || ![h isKindOfClass:NSNumber.class] || w.unsignedLongValue < 16 || h.unsignedLongValue < 16
                || w.unsignedLongValue > maxSide || h.unsignedLongValue > maxSide || ![event[@"format"] isEqual:@"bgrx"]) { fail(@"guest mode outside bound"); return NO; }
            width = w.unsignedLongValue & ~1UL; height = h.unsignedLongValue & ~1UL;
            free(canvas); canvas = calloc(width * height, 4);
            if (!canvas || !createEncoder()) return NO;
            diagnostic(@{@"event": @"mode", @"width": @(width), @"height": @(height), @"codec": useHEVC ? @"hevc" : @"h264"});
        } else if ([event[@"event"] isEqual:@"error"]) {
            fail([NSString stringWithFormat:@"guest: %@", event[@"message"]]); return NO;
        } else {
            diagnostic(@{@"event": @"guest", @"detail": event});
        }
        return YES;
    }
    if (kind == 2) {
        if (!canvas || length < 8) { fail(@"rectangle before mode"); return NO; }
        size_t x = (size_t)body[0] << 8 | body[1], y = (size_t)body[2] << 8 | body[3], w = (size_t)body[4] << 8 | body[5], h = (size_t)body[6] << 8 | body[7];
        if (w == 0 || h == 0 || h > tile || length != 8 + w * h * 4) { fail(@"rectangle outside bound"); return NO; }
        const uint8_t *rows = body + 8;
        // The canvas may be a pixel narrower or shorter than the guest mode (even sizes); clip.
        for (size_t row = 0; row < h && y + row < height; row++) {
            if (x >= width) break;
            memcpy(canvas + ((y + row) * width + x) * 4, rows + row * w * 4, MIN(w, width - x) * 4);
        }
        dirty = YES; lastChange = nowUs();
        return YES;
    }
    if (kind == 3) {
        if (dirty) { encode(); settled = NO; }
        return YES;
    }
    fail(@"unknown guest record");
    return NO;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc != 4 || strcmp(argv[2], "--codec") || (strcmp(argv[3], "hevc") && strcmp(argv[3], "h264"))) {
            fprintf(stderr, "Usage: vz-desktop-stream PRIVATE_UNIX_SOCKET --codec hevc|h264\n");
            return 2;
        }
        signal(SIGPIPE, SIG_IGN);
        useHEVC = !strcmp(argv[3], "hevc");
        outputLock = [NSLock new];
        epoch = nowUs();
        int guest = connectPrivate(argv[1]);
        if (guest < 0) { fail(@"cannot connect to the guest stream bridge"); return 1; }
        static const char start[] = "{\"method\":\"start\"}\n";
        if (!writeAll(guest, (const uint8_t *)start, sizeof start - 1)) { fail(@"guest bridge closed"); return 1; }
        fcntl(STDIN_FILENO, F_SETFL, fcntl(STDIN_FILENO, F_GETFL) | O_NONBLOCK);
        NSMutableData *incoming = [NSMutableData new], *commands = [NSMutableData new];
        uint8_t buffer[256 * 1024];
        while (!failed && !stopping) {
            // A still screen gets one more encode after it settles: the
            // first encode of a change is speed-first and can be soft.
            int timeout = -1;
            if (!settled && encoder) {
                int64_t wait = 300000 - (nowUs() - lastChange);
                if (wait <= 0) { encode(); settled = YES; continue; }
                timeout = (int)(wait / 1000) + 1;
            }
            struct pollfd fds[2] = {{.fd = guest, .events = POLLIN}, {.fd = STDIN_FILENO, .events = POLLIN}};
            int ready = poll(fds, 2, timeout);
            if (ready < 0 && errno == EINTR) continue;
            if (ready < 0) { fail(@"poll failed"); break; }
            if (fds[1].revents) {
                ssize_t n = read(STDIN_FILENO, buffer, sizeof buffer);
                if (n == 0) break;
                if (n > 0) [commands appendBytes:buffer length:(size_t)n];
                for (;;) {
                    const uint8_t *bytes = commands.bytes;
                    const uint8_t *newline = memchr(bytes, '\n', commands.length);
                    if (!newline) { if (commands.length > maxLine) { fail(@"stdin line exceeds 4 KiB"); } break; }
                    size_t line = (size_t)(newline - bytes);
                    if (line == 4 && !memcmp(bytes, "stop", 4)) { stopping = YES; break; }
                    if (line == 8 && !memcmp(bytes, "keyframe", 8)) { forceKeyframe = YES; if (encoder) encode(); }
                    else if (line > 1 && line <= maxLine && bytes[0] == '{') { if (!writeAll(guest, bytes, line + 1)) fail(@"guest bridge closed"); }
                    else fail(@"invalid stdin command");
                    [commands replaceBytesInRange:NSMakeRange(0, line + 1) withBytes:NULL length:0];
                }
            }
            if (fds[0].revents) {
                ssize_t n = read(guest, buffer, sizeof buffer);
                if (n == 0) { fail(@"guest closed the desktop stream"); break; }
                if (n < 0) { if (errno != EINTR && errno != EAGAIN) fail(@"guest read failed"); continue; }
                [incoming appendBytes:buffer length:(size_t)n];
                size_t offset = 0;
                const uint8_t *bytes = incoming.bytes;
                while (incoming.length - offset >= 4) {
                    uint32_t length; memcpy(&length, bytes + offset, 4); length = CFSwapInt32BigToHost(length);
                    if (length < 1 || length > maxRecord) { fail(@"guest record outside bound"); break; }
                    if (incoming.length - offset < 4 + (size_t)length) break;
                    if (!handleRecord(bytes + offset + 4, length)) break;
                    offset += 4 + length;
                }
                [incoming replaceBytesInRange:NSMakeRange(0, offset) withBytes:NULL length:0];
            }
        }
        destroyEncoder();
        close(guest);
        free(canvas);
        diagnostic(@{@"event": @"stopped"});
        return 0;
    }
}
