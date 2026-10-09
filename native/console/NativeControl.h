#import <Foundation/Foundation.h>

// One persistent local controller connection. No SPICE/QMP/input synthesis.
// Public methods are nonblocking and safe on AppKit's main thread.
@interface VNNativeControl : NSObject
- (instancetype)initWithSocket:(NSString *)socket vm:(NSString *)vm worker:(NSString *)worker display:(NSString *)display;
- (void)start;
- (void)takeControl;
- (void)returnAgent;
- (void)suspend;
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame;
- (NSDictionary *)status;
@end
NSString *VNNativeQCode(unsigned short keyCode);

BOOL VNNativeModifierDown(unsigned short keyCode,NSUInteger flags,BOOL *down);
