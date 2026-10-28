#import <Foundation/Foundation.h>
#import <CoreVideo/CoreVideo.h>

// Capture-thread hooks. Buffer acquisition returns one cache ownership lease;
// callers MUST balance Release, never raw CFRelease, even after invalidation.
CVPixelBufferRef VeynodeDisplayAcquire(NSUInteger width, NSUInteger height);
BOOL VeynodeDisplayRetain(CVPixelBufferRef pixel);
void VeynodeDisplayRelease(CVPixelBufferRef pixel);
void VeynodeDisplayPublish(CVPixelBufferRef pixel, uint64_t generation, uint64_t sequence, int64_t capturedMonotonicUS);
void VeynodeDisplayInvalidate(void);
BOOL VeynodeDisplayVisible(void);
// The visible window's screen refresh (at most 240), or 0 while it is not visible.
unsigned VeynodeDisplayVisibleFPS(void);
NSDictionary *VeynodeDisplayCapabilities(void);
void VeynodeDisplayCommand(NSString *method, NSDictionary *frame, NSNumber *diagnosticID, NSDictionary *receiptFields, void (^completion)(NSDictionary *, NSString *));
int VeynodeDisplaySelfTest(void);
int VeynodeConsoleMain(int argc, char **argv);
