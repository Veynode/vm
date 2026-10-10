#import <Foundation/Foundation.h>

@class VNInputFrameDiagnostic, VNFrameAcquisition;

// One persistent local controller connection. No SPICE/QMP/input synthesis.
// Public methods are nonblocking and safe on AppKit's main thread.
@interface VNNativeControl : NSObject
- (instancetype)initWithSocket:(NSString *)socket vm:(NSString *)vm worker:(NSString *)worker display:(NSString *)display;
@property(nonatomic,strong) VNInputFrameDiagnostic *frameDiagnostic;
@property(nonatomic,strong) VNFrameAcquisition *frameAcquisition;
@property(nonatomic) BOOL frameAcquisitionRequested;
- (NSDictionary *)acquisitionContext;
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame acquisition:(NSDictionary *)acquisition receipt:(NSString *)receipt context:(NSDictionary *)context;
- (void)start;
- (void)takeControl;
- (void)returnAgent;
- (void)suspend;
// Fixed diagnostic labels only; never event contents or arbitrary peer messages.
- (void)suspendWithReason:(NSString *)reason;
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame;
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame acquisition:(NSDictionary *)acquisition;
- (NSDictionary *)status;
@end
NSString *VNNativeQCode(unsigned short keyCode);

BOOL VNNativeModifierDown(unsigned short keyCode,NSUInteger flags,BOOL *down);
