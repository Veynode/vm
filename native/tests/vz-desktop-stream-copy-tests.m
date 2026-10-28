// copyCanvas tests for native/tools/vz-desktop-stream.m: a pooled buffer that
// only takes the tiles changed since its own last fill must still equal the
// canvas, whatever buffers the encoder holds. No VT session or Guest.
#define main vzDesktopStreamMain
#include "../tools/vz-desktop-stream.m"
#undef main
#include <assert.h>

static void sendRecord(NSData *record) { assert(handleRecord(record.bytes, record.length)); }
static void mode(size_t w, size_t h) {
    NSMutableData *record = [NSMutableData dataWithBytes:"\x01" length:1];
    [record appendData:[NSJSONSerialization dataWithJSONObject:@{@"event": @"mode", @"width": @(w), @"height": @(h), @"format": @"bgrx"} options:0 error:NULL]];
    sendRecord(record);
}
static void rectangle(size_t x, size_t y, size_t w, size_t h) {
    NSMutableData *record = [NSMutableData dataWithLength:9 + w * h * 4];
    uint8_t *bytes = record.mutableBytes;
    bytes[0] = 2;
    uint16_t fields[4] = {(uint16_t)x, (uint16_t)y, (uint16_t)w, (uint16_t)h};
    for (int i = 0; i < 4; i++) { bytes[1 + i * 2] = fields[i] >> 8; bytes[2 + i * 2] = fields[i] & 0xff; }
    arc4random_buf(bytes + 9, w * h * 4);
    sendRecord(record);
}
static void expectView(CVPixelBufferRef pixel, size_t x, size_t y, size_t w, size_t h, const char *what) {
    CVPixelBufferLockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly);
    const uint8_t *base = CVPixelBufferGetBaseAddress(pixel); size_t stride = CVPixelBufferGetBytesPerRow(pixel);
    for (size_t row = 0; row < h; row++)
        if (memcmp(base + row * stride, canvas + ((y + row) * sourceWidth + x) * 4, w * 4)) {
            fprintf(stderr, "FAIL %s: row %zu differs from the canvas\n", what, row); exit(1);
        }
    CVPixelBufferUnlockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly);
}

// Random guest rectangles (any size, some straddling bands or past the even
// canvas) between copies, while up to three earlier buffers stay held.
static void exercise(size_t guestW, size_t guestH, size_t cropX, size_t cropY, size_t cropW, size_t cropH, int rounds) {
    mode(guestW, guestH);
    CVPixelBufferPoolRef cropPool = createPool(cropW, cropH);
    uint64_t cropBase = ++changeSeq;
    CVPixelBufferRef held[6] = {0};
    for (int round = 0; round < rounds; round++) {
        for (uint32_t n = arc4random_uniform(4); n; n--) {
            size_t w = 1 + arc4random_uniform((uint32_t)MIN(guestW, 300)), h = 1 + arc4random_uniform(tile);
            rectangle(arc4random_uniform((uint32_t)(guestW - w + 1)), arc4random_uniform((uint32_t)(guestH - h + 1)), w, h);
        }
        CVPixelBufferRef full = copyCanvas(sourcePool, sourceBase, 0, 0, sourceWidth, sourceHeight);
        CVPixelBufferRef crop = copyCanvas(cropPool, cropBase, cropX, cropY, cropW, cropH);
        assert(full && crop);
        expectView(full, 0, 0, sourceWidth, sourceHeight, "full view");
        expectView(crop, cropX, cropY, cropW, cropH, "crop");
        size_t slot = (size_t)round % 3 * 2;
        if (held[slot]) { CVPixelBufferRelease(held[slot]); CVPixelBufferRelease(held[slot + 1]); held[slot] = held[slot + 1] = NULL; }
        if (arc4random_uniform(2)) { held[slot] = full; held[slot + 1] = crop; }
        else { CVPixelBufferRelease(full); CVPixelBufferRelease(crop); }
    }
    for (int i = 0; i < 6; i++) if (held[i]) CVPixelBufferRelease(held[i]);
    CVPixelBufferPoolRelease(cropPool);
}

int main(void) {
    @autoreleasepool {
        exercise(1921, 1081, 100, 70, 640, 400, 400);
        uint64_t before = copiedBytes, frames = copies, whole = wholeCopies;
        // A mode change mid-stream: new canvas, pools and tile grid.
        exercise(1280, 800, 0, 64, 1280, 128, 400);
        assert(copies - frames == 800);
        // Each pool's first few buffers are fresh and copied whole; the rest
        // take only what changed, so far less than whole copies would.
        uint64_t full = (uint64_t)(1280 * 800 + 1280 * 128) * 4 * 400;
        printf("copies %llu, whole %llu, %.1f%% of whole-view bytes\n", copies - frames, wholeCopies - whole, 100.0 * (copiedBytes - before) / full);
        assert(wholeCopies - whole < 20 && copiedBytes - before < full / 2);
        // Still screen: a buffer filled after the last change copies nothing.
        CVPixelBufferRef a = copyCanvas(sourcePool, sourceBase, 0, 0, sourceWidth, sourceHeight);
        CVPixelBufferRelease(a);
        uint64_t settled = copiedBytes;
        for (int i = 0; i < 8; i++) { CVPixelBufferRef b = copyCanvas(sourcePool, sourceBase, 0, 0, sourceWidth, sourceHeight); expectView(b, 0, 0, sourceWidth, sourceHeight, "still"); CVPixelBufferRelease(b); }
        assert(copiedBytes == settled);
        puts("PASS vz-desktop-stream dirty-tile copies match the canvas (offline, no VT session)");
        return 0;
    }
}
