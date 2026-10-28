// Apple Virtualization Linux desktop -> hardware VideoToolbox -> framed Annex B.
// The guest agent sends changed BGRX rectangles of its DRM scanout over the
// worker's vsock 5001 bridge (protocol in guest/veynode-agent/desktop_linux.go);
// this helper patches them into a host canvas and encodes once per guest frame.
// Only one helper can hold the bridge, so one process serves every viewer: each
// track is its own compression session over the same canvas, with its own
// crop, output size and bitrate.
//
// Usage: vz-desktop-stream PRIVATE_UNIX_SOCKET --codec hevc|h264 [--tracks] [--audio-fd N]
// stdout: u32 BE size, u64 BE pts_us, u8 keyframe, [u8 track with --tracks,] Annex B access unit
// --audio-fd: a stream socket that gets the guest's audio, each record u32 BE
// size, then u32 BE rate, u8 channels, s16le PCM. Audio is best effort: a
// writer thread sends it, and records past a short backlog are dropped.
// stdin, one command per line:
//   stop
//   {"input":[...]}             forwarded to the guest, as are
//   {"audio":bool}, {"mic":"<base64>"}  (guest/veynode-agent/desktop_audio_linux.go;
//                               a macOS Guest: native/worker/ScreenSharing.swift)
//   [@T ]keyframe
//   [@T ]bitrate N              bits/s, clamped to 250000..40000000
//   [@T ]crop X Y W H           source pixels, clamped to the source, W/H made even;
//                               "crop 0 0 0 0" is the full screen
//   [@T ]scale W H              even output size, at least 0.4x the view per side;
//                               never larger than the crop; "scale 0 0" is the view size
//   track add T | track remove T
//   fps N                       at most N guest frames a second (1..240, the
//                               viewer's screen refresh; default 240), every track
// T is 0..7 and defaults to 0. Track 0 always exists; other tracks need --tracks.
// stderr: JSON diagnostics. "view" gives each track's source rectangle and
// output size; "rate" answers bitrate and reports a refused command.
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
#include <limits.h>
#include "../console/FramePacing.h"

static const size_t maxAU = 4 * 1024 * 1024, maxSide = 4096, tile = 64, maxLine = 2 * 1024 * 1024; // a JSON line may carry 200 KiB of clipboard text
enum { maxTracks = 8 };
static const size_t maxRecord = 1 + 8 + maxSide * tile * 4;
// Same bounds as crates/media-local/src/rate_control.rs MIN_BPS/MAX_BPS.
static const uint64_t minBitrate = 250000, maxBitrate = 40000000, defaultBitrate = 8000000;
typedef struct {
    BOOL used, forceKeyframe, stillActive, stillFailed;
    // The speed-first session while a still screen is carried by the full-quality one (enterStill).
    VTCompressionSessionRef encoder, parked;
    VTPixelTransferSessionRef transfer;
    CVPixelBufferPoolRef cropPool, pool;
    uint64_t bitrate;
    // Requested: a zero crop size is the full screen, a zero scale the view size.
    size_t cropX, cropY, cropW, cropH, scaleW, scaleH;
    // In effect: the source rectangle (x, y, w, h) and the encoded size.
    size_t x, y, w, h, outW, outH;
} Track;
static Track tracks[maxTracks];
static CVPixelBufferPoolRef sourcePool;
static BOOL useHEVC, multiTrack, dirty, failed, stopping, settled = YES;
static int64_t lastStill;
static size_t sourceWidth, sourceHeight;
static uint8_t *canvas;
static int64_t epoch, lastChange;
// A guest frame that ends before its slot at fps a second waits for it.
static const uint64_t maxFPS = VN_VISIBLE_FPS;
static uint64_t streamFPS = maxFPS;
static VNFramePacing framePacing;
// deferred: a finished guest frame waits for its slot. inFrame: rectangles of the next frame are
// arriving, so the canvas is mid-frame and the next frame end encodes instead.
static BOOL deferred, inFrame;
static void frameDue(void);
static NSLock *outputLock;

static int64_t nowUs(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (int64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000; }
static void diagnostic(NSDictionary *value) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:NULL];
    fwrite(data.bytes, 1, data.length, stderr); fputc('\n', stderr); fflush(stderr);
}
static void fail(NSString *message) { if (!failed) diagnostic(@{@"event": @"error", @"message": message}); failed = YES; }
static void refuse(size_t track, NSString *command, NSString *message) {
    diagnostic(@{@"event": @"rate", @"track": @(track), @"command": command, @"error": message});
}

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

// The output refcon is the track index.
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
        uint8_t header[14]; uint32_t length = CFSwapInt32HostToBig((uint32_t)au.length); uint64_t stamp = CFSwapInt64HostToBig((uint64_t)MAX(pts.value, 0));
        memcpy(header, &length, 4); memcpy(header + 4, &stamp, 8); header[12] = key; header[13] = (uint8_t)(uintptr_t)context;
        [outputLock lock];
        BOOL ok = writeAll(STDOUT_FILENO, header, multiTrack ? 14 : 13) && writeAll(STDOUT_FILENO, au.bytes, au.length);
        [outputLock unlock];
        if (!ok) { fail(@"stdout closed"); kill(getpid(), SIGTERM); }
    }
}

static void destroyTrack(Track *t) {
    if (t->parked) { VTCompressionSessionCompleteFrames(t->parked, kCMTimeInvalid); VTCompressionSessionInvalidate(t->parked); CFRelease(t->parked); t->parked = NULL; }
    t->stillActive = t->stillFailed = NO;
    if (t->encoder) { VTCompressionSessionCompleteFrames(t->encoder, kCMTimeInvalid); VTCompressionSessionInvalidate(t->encoder); CFRelease(t->encoder); t->encoder = NULL; }
    if (t->transfer) { VTPixelTransferSessionInvalidate(t->transfer); CFRelease(t->transfer); t->transfer = NULL; }
    if (t->pool) { CVPixelBufferPoolRelease(t->pool); t->pool = NULL; }
    if (t->cropPool) { CVPixelBufferPoolRelease(t->cropPool); t->cropPool = NULL; }
}

static CVPixelBufferPoolRef createPool(size_t w, size_t h) {
    NSDictionary *attributes = @{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
                                 (__bridge NSString *)kCVPixelBufferWidthKey: @(w), (__bridge NSString *)kCVPixelBufferHeightKey: @(h),
                                 (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey: @{}};
    CVPixelBufferPoolRef created = NULL;
    return CVPixelBufferPoolCreate(NULL, NULL, (__bridge CFDictionaryRef)attributes, &created) == kCVReturnSuccess ? created : NULL;
}

// The crop is clamped to the source and kept even (at least 2x2); the output
// is the scale, or the view size, never larger than the view.
static void resolveView(Track *t) {
    if (!t->cropW) { t->x = t->y = 0; t->w = sourceWidth; t->h = sourceHeight; }
    else {
        t->x = MIN(t->cropX, sourceWidth - 2); t->y = MIN(t->cropY, sourceHeight - 2);
        t->w = MIN(t->cropW, sourceWidth - t->x) & ~(size_t)1; t->h = MIN(t->cropH, sourceHeight - t->y) & ~(size_t)1;
    }
    t->outW = t->scaleW ? MIN(t->scaleW, t->w) : t->w; t->outH = t->scaleH ? MIN(t->scaleH, t->h) : t->h;
}
static BOOL fullView(const Track *t) { return t->x == 0 && t->y == 0 && t->w == sourceWidth && t->h == sourceHeight; }

static void viewEvent(size_t i) {
    Track *t = &tracks[i];
    diagnostic(@{@"event": @"view", @"track": @(i), @"x": @(t->x), @"y": @(t->y), @"w": @(t->w), @"h": @(t->h),
                 @"src_w": @(sourceWidth), @"src_h": @(sourceHeight), @"out_w": @(t->outW), @"out_h": @(t->outH), @"bitrate": @(t->bitrate)});
}

// Nil on success. The speed-first session, or with still the full-quality one: Quality only counts when
// set before the session is prepared, and it lifts a photographic keyframe from SSIM 0.94 to 0.99
// (1920x1080: 300 KB to 1.1 MB), so it is only for a screen that stopped changing.
static void discard(VTCompressionSessionRef session) { VTCompressionSessionInvalidate(session); CFRelease(session); }
static NSString *newSession(size_t i, Track *t, BOOL still, VTCompressionSessionRef *out) {
    NSDictionary *spec = @{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: @YES};
    NSDictionary *attributes = @{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
                                 (__bridge NSString *)kCVPixelBufferWidthKey: @(t->outW), (__bridge NSString *)kCVPixelBufferHeightKey: @(t->outH),
                                 (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey: @{}};
    VTCompressionSessionRef session = NULL;
    OSStatus r = VTCompressionSessionCreate(NULL, (int32_t)t->outW, (int32_t)t->outH, useHEVC ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264,
                                            (__bridge CFDictionaryRef)spec, (__bridge CFDictionaryRef)attributes, NULL, compressed, (void *)(uintptr_t)i, &session);
    if (r != noErr) return [NSString stringWithFormat:@"hardware %@ encoder unavailable at %zux%zu: %d", useHEVC ? @"HEVC" : @"H.264", t->outW, t->outH, (int)r];
    // Same latency settings as the QEMU console helper (native/tools/spice-console.m).
    NSDictionary *properties = @{(__bridge NSString *)kVTCompressionPropertyKey_RealTime: @YES,
                                 (__bridge NSString *)kVTCompressionPropertyKey_AllowFrameReordering: @NO,
                                 (__bridge NSString *)kVTCompressionPropertyKey_AverageBitRate: @(t->bitrate),
                                 (__bridge NSString *)kVTCompressionPropertyKey_ExpectedFrameRate: @60,
                                 // Written into the stream: untagged, decoders assume BT.601 and shift the colours.
                                 (__bridge NSString *)kVTCompressionPropertyKey_ColorPrimaries: (__bridge NSString *)kCVImageBufferColorPrimaries_ITU_R_709_2,
                                 (__bridge NSString *)kVTCompressionPropertyKey_TransferFunction: (__bridge NSString *)kCVImageBufferTransferFunction_ITU_R_709_2,
                                 (__bridge NSString *)kVTCompressionPropertyKey_YCbCrMatrix: (__bridge NSString *)kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                                 (__bridge NSString *)kVTCompressionPropertyKey_MaxKeyFrameInterval: @120,
                                 (__bridge NSString *)kVTCompressionPropertyKey_ProfileLevel: (__bridge NSString *)(useHEVC ? kVTProfileLevel_HEVC_Main_AutoLevel : kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel)};
    for (NSString *key in properties) {
        if ((r = VTSessionSetProperty(session, (__bridge CFStringRef)key, (__bridge CFTypeRef)properties[key])) != noErr) {
            discard(session); return [NSString stringWithFormat:@"encoder property rejected %@: %d", key, (int)r];
        }
    }
    VTSessionSetProperty(session, kVTCompressionPropertyKey_MaxFrameDelayCount, (__bridge CFTypeRef)@0);
    VTSessionSetProperty(session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, still ? kCFBooleanFalse : kCFBooleanTrue);
    if (still) VTSessionSetProperty(session, kVTCompressionPropertyKey_Quality, (__bridge CFTypeRef)@0.9);
    if (VTCompressionSessionPrepareToEncodeFrames(session) != noErr) { discard(session); return @"encoder preparation failed"; }
    CFTypeRef hardware = NULL;
    BOOL verified = VTSessionCopyProperty(session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hardware) == noErr
                    && hardware && CFGetTypeID(hardware) == CFBooleanGetTypeID() && CFBooleanGetValue(hardware);
    if (hardware) CFRelease(hardware);
    if (!verified) { discard(session); return @"encoder did not confirm hardware acceleration"; }
    *out = session;
    return nil;
}

// Nil on success. A new session's first frame is a keyframe.
static NSString *buildTrack(size_t i) {
    Track *t = &tracks[i];
    destroyTrack(t); resolveView(t);
    NSString *problem = newSession(i, t, NO, &t->encoder);
    if (problem) { t->encoder = NULL; return problem; }
    if (!(t->pool = createPool(t->outW, t->outH))) { destroyTrack(t); return @"pixel buffer pool failed"; }
    if (!fullView(t) && !(t->cropPool = createPool(t->w, t->h))) { destroyTrack(t); return @"pixel buffer pool failed"; }
    if (t->outW != t->w || t->outH != t->h) {
        if (VTPixelTransferSessionCreate(NULL, &t->transfer) != noErr) { t->transfer = NULL; destroyTrack(t); return @"scaling session failed"; }
        VTSessionSetProperty(t->transfer, kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_Normal);
    }
    t->forceKeyframe = YES;
    viewEvent(i);
    return nil;
}

static void removeTrack(size_t i) { destroyTrack(&tracks[i]); tracks[i] = (Track){0}; }

static CVPixelBufferRef copyCanvas(CVPixelBufferPoolRef from, size_t x, size_t y, size_t w, size_t h) {
    CVPixelBufferRef pixel = NULL;
    if (CVPixelBufferPoolCreatePixelBuffer(NULL, from, &pixel) != kCVReturnSuccess) return NULL;
    CVPixelBufferLockBaseAddress(pixel, 0);
    uint8_t *base = CVPixelBufferGetBaseAddress(pixel); size_t stride = CVPixelBufferGetBytesPerRow(pixel);
    for (size_t row = 0; row < h; row++) memcpy(base + row * stride, canvas + ((y + row) * sourceWidth + x) * 4, w * 4);
    CVPixelBufferUnlockBaseAddress(pixel, 0);
    return pixel;
}

// Copy into fresh pooled buffers so no encoder reads pixels that the next guest
// rectangle is overwriting. Full-screen tracks share one copy of the canvas; a
// cropped track copies its rectangle; a scaled one downsamples that copy.
static void encodeTrack(size_t i, CVPixelBufferRef *full) {
    Track *t = &tracks[i];
    CVPixelBufferRef input = NULL, pixel = NULL;
    if (fullView(t)) {
        if (!*full && !(*full = copyCanvas(sourcePool, 0, 0, sourceWidth, sourceHeight))) { fail(@"pixel buffer pool exhausted"); return; }
        input = CVPixelBufferRetain(*full);
    } else if (!(input = copyCanvas(t->cropPool, t->x, t->y, t->w, t->h))) { fail(@"pixel buffer pool exhausted"); return; }
    if (t->transfer) {
        if (CVPixelBufferPoolCreatePixelBuffer(NULL, t->pool, &pixel) != kCVReturnSuccess) { CVPixelBufferRelease(input); fail(@"pixel buffer pool exhausted"); return; }
        OSStatus r = VTPixelTransferSessionTransferImage(t->transfer, input, pixel);
        CVPixelBufferRelease(input);
        if (r != noErr) { CVPixelBufferRelease(pixel); fail([NSString stringWithFormat:@"scaling failed: %d", (int)r]); return; }
    } else {
        pixel = input;
    }
    NSDictionary *options = t->forceKeyframe ? @{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame: @YES} : nil;
    OSStatus r = VTCompressionSessionEncodeFrame(t->encoder, pixel, CMTimeMake(nowUs() - epoch, 1000000), kCMTimeInvalid, (__bridge CFDictionaryRef)options, NULL, NULL);
    CVPixelBufferRelease(pixel);
    if (r != noErr) { fail([NSString stringWithFormat:@"encode failed: %d", (int)r]); return; }
    t->forceKeyframe = NO;
}

// One track, or every track when only is maxTracks.
static void encode(size_t only) {
    if (!canvas) return;
    CVPixelBufferRef full = NULL;
    for (size_t i = 0; i < maxTracks && !failed; i++)
        if (tracks[i].used && tracks[i].encoder && (only == maxTracks || only == i)) encodeTrack(i, &full);
    if (full) CVPixelBufferRelease(full);
    if (only == maxTracks) dirty = NO;
}

// Back to the speed-first session; its next frame is a keyframe (the two reference chains differ).
static void leaveStill(Track *t) {
    if (!t->stillActive) return;
    VTCompressionSessionRef still = t->encoder; t->encoder = t->parked; t->parked = still;
    t->stillActive = NO; t->forceKeyframe = YES;
}

// A screen that stopped changing: one keyframe at full quality, from the second session. Not above about
// 4.2 Mpx (a photographic keyframe would near the 4 MiB access unit bound) and a failed creation leaves the
// speed-first session alone; NO then means the caller refines with the speed-first session instead.
static BOOL enterStill(size_t i) {
    Track *t = &tracks[i];
    // The bitrate control is the network's say: a viewer it has backed off for would stall on a 1 MB keyframe.
    if (t->stillActive || t->stillFailed || !t->encoder || t->outW * t->outH > 4200000 || t->bitrate < defaultBitrate) return NO;
    VTCompressionSessionRef still = NULL;
    if (!t->parked) {
        NSString *problem = newSession(i, t, YES, &still);
        if (problem) { t->stillFailed = YES; return NO; }
    } else { still = t->parked; t->parked = NULL; }
    t->parked = t->encoder; t->encoder = still; t->stillActive = YES; t->forceKeyframe = YES;
    return YES;
}

// Rebuild after a crop or scale change; a refused size puts the old view back.
static void rebuild(size_t i, Track previous, NSString *command) {
    if (!canvas) return;
    NSString *problem = buildTrack(i);
    if (!problem) { encode(i); return; }
    refuse(i, command, problem);
    Track *t = &tracks[i];
    t->cropX = previous.cropX; t->cropY = previous.cropY; t->cropW = previous.cropW; t->cropH = previous.cropH;
    t->scaleW = previous.scaleW; t->scaleH = previous.scaleH;
    if ((problem = buildTrack(i))) { if (i == 0) fail(problem); else { refuse(i, command, @"track removed"); removeTrack(i); } return; }
    encode(i);
}

static void setBitrate(size_t i, uint64_t requested) {
    Track *t = &tracks[i];
    t->bitrate = MIN(MAX(requested, minBitrate), maxBitrate);
    NSMutableDictionary *event = [@{@"event": @"rate", @"track": @(i), @"bitrate": @(t->bitrate), @"requested": @(requested)} mutableCopy];
    // The full-quality session is outside rate control: the rate goes to the speed-first one, parked or not.
    VTCompressionSessionRef speedFirst = t->stillActive ? t->parked : t->encoder;
    if (speedFirst) {
        OSStatus r = VTSessionSetProperty(speedFirst, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFTypeRef)@(t->bitrate));
        CFTypeRef raw = NULL;
        if (r == noErr) r = VTSessionCopyProperty(speedFirst, kVTCompressionPropertyKey_AverageBitRate, NULL, &raw);
        id readback = CFBridgingRelease(raw);
        if (r != noErr) event[@"error"] = [NSString stringWithFormat:@"bitrate property rejected: %d", (int)r];
        else event[@"readback"] = readback ?: [NSNull null];
    }
    diagnostic(event);
}

static void setCrop(size_t i, uint64_t x, uint64_t y, uint64_t w, uint64_t h) {
    if (!canvas) { refuse(i, @"crop", @"crop before guest mode"); return; }
    BOOL full = !x && !y && !w && !h;
    if (!full && (w < 2 || h < 2 || x >= sourceWidth || y >= sourceHeight)) { refuse(i, @"crop", @"crop outside bound"); return; }
    Track *t = &tracks[i], previous = *t;
    t->cropX = full ? 0 : (size_t)x; t->cropY = full ? 0 : (size_t)y; t->cropW = full ? 0 : (size_t)w; t->cropH = full ? 0 : (size_t)h;
    rebuild(i, previous, @"crop");
}

// Checked against the view when asked; a later, smaller crop clamps it.
static void setScale(size_t i, uint64_t w, uint64_t h) {
    if (!canvas) { refuse(i, @"scale", @"scale before guest mode"); return; }
    Track *t = &tracks[i], previous = *t;
    BOOL native = !w && !h;
    if (!native && (!w || !h || (w & 1) || (h & 1) || w > t->w || h > t->h || w * 5 < t->w * 2 || h * 5 < t->h * 2)) { refuse(i, @"scale", @"scale outside bound"); return; }
    t->scaleW = (size_t)w; t->scaleH = (size_t)h;
    rebuild(i, previous, @"scale");
}

static void addTrack(size_t i) {
    if (!multiTrack) { refuse(i, @"track", @"tracks need --tracks"); return; }
    if (i == 0 || tracks[i].used) { refuse(i, @"track", @"track already exists"); return; }
    tracks[i] = (Track){.used = YES, .bitrate = defaultBitrate};
    if (!canvas) return;
    NSString *problem = buildTrack(i);
    if (problem) { refuse(i, @"track", problem); removeTrack(i); return; }
    encode(i);
}

// Unsigned decimal fields separated by single spaces, nothing else.
static BOOL parseFields(const uint8_t *bytes, size_t length, uint64_t *values, int count) {
    size_t at = 0;
    for (int i = 0; i < count; i++) {
        if (i && (at >= length || bytes[at++] != ' ')) return NO;
        size_t digits = 0; uint64_t value = 0;
        while (at < length && bytes[at] >= '0' && bytes[at] <= '9' && digits < 10) { value = value * 10 + (bytes[at++] - '0'); digits++; }
        if (!digits) return NO;
        values[i] = value;
    }
    return at == length;
}
static BOOL hasPrefix(const uint8_t *bytes, size_t length, const char *word) {
    size_t n = strlen(word); return length > n && !memcmp(bytes, word, n);
}

// NO for a malformed line; a well-formed but refused one is reported and kept going.
static BOOL trackCommand(const uint8_t *bytes, size_t line) {
    uint64_t fields[4];
    if (hasPrefix(bytes, line, "fps ") && parseFields(bytes + 4, line - 4, fields, 1)) {
        if (fields[0] < 1 || fields[0] > maxFPS) refuse(0, @"fps", @"outside 1..240");
        else streamFPS = fields[0];
        return YES;
    }
    if (hasPrefix(bytes, line, "track add ") && parseFields(bytes + 10, line - 10, fields, 1) && fields[0] < maxTracks) { addTrack((size_t)fields[0]); return YES; }
    if (hasPrefix(bytes, line, "track remove ") && parseFields(bytes + 13, line - 13, fields, 1) && fields[0] < maxTracks) {
        if (fields[0] == 0 || !tracks[fields[0]].used) refuse((size_t)fields[0], @"track", @"track 0 stays and others must exist");
        else { removeTrack((size_t)fields[0]); diagnostic(@{@"event": @"view", @"track": @(fields[0]), @"removed": @YES}); }
        return YES;
    }
    size_t i = 0;
    if (line > 3 && bytes[0] == '@' && bytes[1] >= '0' && bytes[1] < '0' + (int)maxTracks && bytes[2] == ' ') { i = (size_t)(bytes[1] - '0'); bytes += 3; line -= 3; }
    BOOL known = (line == 8 && !memcmp(bytes, "keyframe", 8))
                 || (hasPrefix(bytes, line, "bitrate ") && parseFields(bytes + 8, line - 8, fields, 1))
                 || (hasPrefix(bytes, line, "crop ") && parseFields(bytes + 5, line - 5, fields, 4))
                 || (hasPrefix(bytes, line, "scale ") && parseFields(bytes + 6, line - 6, fields, 2));
    if (!known) return NO;
    if (!tracks[i].used) { refuse(i, @"track", multiTrack ? @"no such track" : @"tracks need --tracks"); return YES; }
    if (bytes[0] == 'k') { tracks[i].forceKeyframe = YES; if (tracks[i].encoder) encode(i); }
    else if (bytes[0] == 'b') setBitrate(i, fields[0]);
    else if (bytes[0] == 'c') setCrop(i, fields[0], fields[1], fields[2], fields[3]);
    else setScale(i, fields[0], fields[1]);
    return YES;
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

static _Atomic int audioFD = -1;
static const size_t maxAudio = 5 + 64 * 1024;
static dispatch_queue_t audioQueue;
static _Atomic int audioBacklog;
static void forwardAudio(const uint8_t *body, size_t length) {
    if (audioFD < 0 || audioBacklog >= 8) return;
    NSMutableData *record = [NSMutableData dataWithLength:4];
    uint32_t size = CFSwapInt32HostToBig((uint32_t)length);
    memcpy(record.mutableBytes, &size, 4);
    [record appendBytes:body length:length];
    audioBacklog++;
    dispatch_async(audioQueue, ^{
        // A failed write ends the audio, never the video.
        if (audioFD >= 0 && !writeAll(audioFD, record.bytes, record.length)) { close(audioFD); audioFD = -1; }
        audioBacklog--;
    });
}

static BOOL handleRecord(const uint8_t *body, size_t length) {
    uint8_t kind = body[0]; body++; length--;
    if (kind == 4) {
        uint8_t channels = length > 4 ? body[4] : 0;
        if (length < 6 || length > maxAudio || channels < 1 || channels > 2 || (length - 5) % (2 * channels)) { fail(@"guest audio outside bound"); return NO; }
        forwardAudio(body, length);
        return YES;
    }
    if (kind == 1) {
        NSDictionary *event = [NSJSONSerialization JSONObjectWithData:[NSData dataWithBytes:body length:length] options:0 error:NULL];
        if (![event isKindOfClass:NSDictionary.class]) { fail(@"malformed guest event"); return NO; }
        if ([event[@"event"] isEqual:@"mode"]) {
            NSNumber *w = event[@"width"], *h = event[@"height"];
            if (![w isKindOfClass:NSNumber.class] || ![h isKindOfClass:NSNumber.class] || w.unsignedLongValue < 16 || h.unsignedLongValue < 16
                || w.unsignedLongValue > maxSide || h.unsignedLongValue > maxSide || ![event[@"format"] isEqual:@"bgrx"]) { fail(@"guest mode outside bound"); return NO; }
            sourceWidth = w.unsignedLongValue & ~1UL; sourceHeight = h.unsignedLongValue & ~1UL;
            free(canvas); canvas = calloc(sourceWidth * sourceHeight, 4);
            if (sourcePool) CVPixelBufferPoolRelease(sourcePool);
            if (!canvas || !(sourcePool = createPool(sourceWidth, sourceHeight))) { fail(@"canvas allocation failed"); return NO; }
            diagnostic(@{@"event": @"mode", @"width": @(sourceWidth), @"height": @(sourceHeight), @"codec": useHEVC ? @"hevc" : @"h264"});
            // Crops and scales carry over, clamped to the new mode.
            for (size_t i = 0; i < maxTracks; i++) {
                if (!tracks[i].used) continue;
                NSString *problem = buildTrack(i);
                if (problem && i == 0) { fail(problem); return NO; }
                if (problem) { refuse(i, @"track", problem); removeTrack(i); }
            }
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
        for (size_t row = 0; row < h && y + row < sourceHeight; row++) {
            if (x >= sourceWidth) break;
            memcpy(canvas + ((y + row) * sourceWidth + x) * 4, rows + row * w * 4, MIN(w, sourceWidth - x) * 4);
        }
        dirty = YES; inFrame = YES; lastChange = nowUs();
        return YES;
    }
    if (kind == 3) {
        inFrame = NO;
        if (dirty) frameDue();
        return YES;
    }
    fail(@"unknown guest record");
    return NO;
}

// Encode the guest's latest frame if its slot has come; else it stays dirty for the poll timeout.
static void frameDue(void) {
    deferred = !VNFramePacingAdmit(&framePacing, nowUs(), (unsigned)streamFPS);
    if (deferred) return;
    for (size_t i = 0; i < maxTracks; i++) leaveStill(&tracks[i]);
    encode(maxTracks); settled = NO;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        BOOL usage = argc < 4 || strcmp(argv[2], "--codec") || (strcmp(argv[3], "hevc") && strcmp(argv[3], "h264"));
        for (int i = 4; !usage && i < argc; i++) {
            if (!strcmp(argv[i], "--tracks") && !multiTrack) multiTrack = YES;
            else if (!strcmp(argv[i], "--audio-fd") && audioFD < 0 && i + 1 < argc) {
                char *end; long fd = strtol(argv[++i], &end, 10);
                if (*end || fd < 3 || fd > INT_MAX || fcntl((int)fd, F_GETFD) < 0) usage = YES;
                else audioFD = (int)fd;
            } else usage = YES;
        }
        if (usage) {
            fprintf(stderr, "Usage: vz-desktop-stream PRIVATE_UNIX_SOCKET --codec hevc|h264 [--tracks] [--audio-fd N]\n");
            return 2;
        }
        signal(SIGPIPE, SIG_IGN);
        audioQueue = dispatch_queue_create("audio", DISPATCH_QUEUE_SERIAL);
        useHEVC = !strcmp(argv[3], "hevc");
        tracks[0] = (Track){.used = YES, .bitrate = defaultBitrate};
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
            // A still screen gets one more encode after it settles: the first encode of a change is
            // speed-first and can be soft, so a full-quality keyframe (at most every 3 s), else a refinement.
            int timeout = -1;
            if (deferred && !inFrame && canvas) {
                frameDue();
                if (deferred) timeout = (int)MAX(1, (framePacing.next - nowUs()) / 1000 + 1);
            }
            if (!settled && canvas && !deferred) {
                int64_t wait = 300000 - (nowUs() - lastChange);
                if (wait <= 0) {
                    int64_t now = nowUs();
                    BOOL allowed = now - lastStill >= 3000000;
                    for (size_t i = 0; i < maxTracks; i++) {
                        if (!tracks[i].used || !tracks[i].encoder) continue;
                        if (allowed && enterStill(i)) lastStill = now;
                        encode(i);
                    }
                    settled = YES; continue;
                }
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
                    if (!newline) { if (commands.length > maxLine) { fail(@"stdin line exceeds 2 MiB"); } break; }
                    size_t line = (size_t)(newline - bytes);
                    if (line == 4 && !memcmp(bytes, "stop", 4)) { stopping = YES; break; }
                    if (line > 1 && line <= maxLine && bytes[0] == '{') { if (!writeAll(guest, bytes, line + 1)) fail(@"guest bridge closed"); }
                    else if (!trackCommand(bytes, line)) fail(@"invalid stdin command");
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
        for (size_t i = 0; i < maxTracks; i++) destroyTrack(&tracks[i]);
        if (sourcePool) CVPixelBufferPoolRelease(sourcePool);
        close(guest);
        free(canvas);
        diagnostic(@{@"event": @"stopped"});
        return 0;
    }
}
