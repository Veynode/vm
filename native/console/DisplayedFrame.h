#import <Foundation/Foundation.h>
#include <stdbool.h>
#include <stdint.h>

typedef struct { double x,y,width,height; } VNDisplayRect;
typedef struct {
    size_t sourceWidth,sourceHeight;
    double drawableWidth,drawableHeight;
    VNDisplayRect viewBounds,backingBounds,viewport;
    bool viewFlipped;
} VNDisplayGeometry;
bool VNDisplayGeometryValid(VNDisplayGeometry value);
bool VNDisplayGeometryEqual(VNDisplayGeometry a,VNDisplayGeometry b);
// Input is a backing-coordinate point from NSView convertPointToBacking:.
// Output is a top-left-origin Guest physical pixel; letterbox/outside rejects.
bool VNDisplayMapBackingPoint(VNDisplayGeometry geometry,double x,double y,size_t *guestX,size_t *guestY);

// All methods are serialized by the publication lock. Values returned are
// immutable snapshots. This is observation provenance, never input authority.
@interface VNDisplayedFrameStore : NSObject
@property(readonly) uint64_t revision;
- (void)invalidate;
- (NSString *)instance;
- (NSDictionary *)validateToken:(NSDictionary *)token now:(double)now;
- (NSDictionary *)candidateGeometry:(VNDisplayGeometry)geometry generation:(uint64_t)generation sequence:(uint64_t)sequence windowEpoch:(uint64_t)windowEpoch capturedMonotonicUS:(int64_t)captured;
- (BOOL)commitCandidate:(NSDictionary *)candidate presentedTime:(double)presented receivedMonotonic:(double)received;
- (NSDictionary *)snapshot;
@end

NSDictionary *VNDisplayedFrameToken(NSDictionary *frame);
bool VNDisplayedFrameGeometry(NSDictionary *frame,VNDisplayGeometry *geometry);
