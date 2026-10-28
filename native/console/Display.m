#import "Display.h"
#import "FramePool.h"
#import "Presented.h"
#import "DisplayedFrame.h"
#import "NativeControl.h"
#import "InputFrameDiagnostic.h"
#import "FrameAcquisition.h"
#import "DrawTiming.h"
#import "Media.h"
#include "FramePacing.h"
#include "EncodedGeometry.h"
#include "InputFocus.h"
#import <AppKit/AppKit.h>
#import <MetalKit/MetalKit.h>
#import <IOSurface/IOSurface.h>

static VNFramePool *pool;
static NSLock *publicationLock;
static CVPixelBufferRef latest;
static uint64_t sourceGeneration,sourceSequence;
static int64_t sourceCapturedMono;
static VNDisplayedFrameStore *displayed;
static BOOL enabled,visible;
// The maximum refresh of the screen the window is on, at most VN_VISIBLE_FPS.
static unsigned screenFPS=60;
static VNNativeControl *nativeControl;
static VNInputFrameDiagnostic *inputFrameDiagnostic;
static VNFrameAcquisition *frameAcquisition;
static BOOL frameAcquisitionRequested;
static VNDrawTiming *drawTiming;
static void initializeState(void){static dispatch_once_t once;dispatch_once(&once,^{pool=[VNFramePool new];publicationLock=[NSLock new];displayed=[VNDisplayedFrameStore new];drawTiming=[[VNDrawTiming alloc] initWithEnabled:NO];});}
CVPixelBufferRef VeynodeDisplayAcquire(NSUInteger w,NSUInteger h){initializeState();return [pool acquireWidth:w height:h];}
BOOL VeynodeDisplayRetain(CVPixelBufferRef pixel){initializeState();return [pool retainPixel:pixel];}
void VeynodeDisplayRelease(CVPixelBufferRef pixel){[pool releasePixel:pixel];}
void VeynodeDisplayPublish(CVPixelBufferRef pixel,uint64_t generation,uint64_t sequence,int64_t capturedMonotonicUS){
    initializeState();[publicationLock lock];
    if(enabled&&[pool retainPixel:pixel]){CVPixelBufferRef old=latest;latest=pixel;sourceGeneration=generation;sourceSequence=sequence;sourceCapturedMono=capturedMonotonicUS;if(old)[pool releasePixel:old];}
    [publicationLock unlock];
}
void VeynodeDisplayInvalidate(void){
    [nativeControl suspendWithReason:@"source_invalidated"];
    initializeState();[publicationLock lock];if(latest)[pool releasePixel:latest];latest=NULL;sourceGeneration=sourceSequence=0;sourceCapturedMono=0;[displayed invalidate];[frameAcquisition invalidateFrames];[publicationLock unlock];
}
BOOL VeynodeDisplayVisible(void){initializeState();[publicationLock lock];BOOL result=enabled&&visible;[publicationLock unlock];return result;}
unsigned VeynodeDisplayVisibleFPS(void){initializeState();[publicationLock lock];unsigned result=enabled&&visible?screenFPS:0;[publicationLock unlock];return result;}
static NSDictionary *nativeInputStatus(void){
    return nativeControl?[nativeControl status]:@{@"native_frame_acquisition_protocol":@1,@"native_frame_acquisition_negotiated":@NO,@"native_frame_acquisition_enabled":@NO,@"native_input_protocol":@1,@"native_input_supported":@YES,@"native_input_available":@NO,@"input_enabled":@NO,@"display_only":@YES,@"native_input_state":@"view_only",@"native_input_ready":@NO,@"native_release_confirmed":@YES,@"native_input_epoch":[NSNull null],@"native_input_queued_transitions":@0,@"native_input_pending_move":@NO,@"native_input_transition_limit":@32};
}
NSDictionary *VeynodeDisplayCapabilities(void){
    NSMutableDictionary *caps=[@{@"native_window":(MTLCreateSystemDefaultDevice()!=nil?@YES:@NO),@"display_instance":[displayed instance],@"native_window_protocol":@2,@"displayed_frame_protocol":@1,@"display_only":@YES,@"input_enabled":@NO,@"display_path":@"owned-BGRA-MetalKit-no-video-encoding",@"capture_limit_fps":@VN_VISIBLE_FPS,@"native_window_capture_limit_fps":@VN_VISIBLE_FPS,@"background_capture_limit_fps":@VN_BACKGROUND_FPS,@"encoder_limit_fps":@VN_VISIBLE_FPS,@"idle_encode_interval_ms":@1000,@"display_refresh_preference_hz":@VN_VISIBLE_FPS,@"physical_presentation_measurement":@"unavailable",@"presented_time_sample_limit":@120,@"presented_time_source":@"CAMetalDrawable.addPresentedHandler/presentedTime",@"owned_slot_limit":@3,@"display_gpu_inflight_limit":@1} mutableCopy];
    [caps addEntriesFromDictionary:nativeInputStatus()];return caps;
}

// Only events delivered to this application's own focused view are considered.
// No global hook, synthetic host input, accessibility permission or SPICE input.
@interface VNReadOnlyView : MTKView
@property(copy) void (^inputEvent)(NSEvent *);
@end
@implementation VNReadOnlyView
- (BOOL)acceptsFirstResponder{return YES;}
- (void)keyDown:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)keyUp:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)flagsChanged:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)mouseDown:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)mouseUp:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)mouseMoved:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)mouseDragged:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)rightMouseDown:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)rightMouseUp:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)rightMouseDragged:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)otherMouseDown:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)otherMouseUp:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)otherMouseDragged:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
- (void)scrollWheel:(NSEvent *)e{if(self.inputEvent)self.inputEvent(e);}
@end

@interface VNDisplay : NSObject <NSApplicationDelegate,NSWindowDelegate,MTKViewDelegate>
@property NSWindow *window;
@property NSTextField *inputLabel;
@property NSTimer *inputTimer;
@property NSMutableSet<NSString *> *heldKeys;
@property NSMutableSet<NSString *> *heldButtons;
@property BOOL inputWasReady;
@property double lastBegin;
- (void)inputEvent:(NSEvent *)event;
- (void)updateInput;
- (void)suspendInput;
- (void)suspendInputWithReason:(NSString *)reason;
@property VNReadOnlyView *view;
@property id<MTLDevice> device;
@property id<MTLCommandQueue> commands;
@property id<MTLRenderPipelineState> pipeline;
@property BOOL inflight;
@property NSUInteger submitted,completed,failed;
@property VNPresentedSamples *presented;
@property uint64_t lastGeneration,lastSequence,windowEpoch;
@property CGSize lastDrawableSize;
@property BOOL layoutObserved,observedFlipped;
@property NSRect observedViewBounds,observedBackingBounds;
@property CGSize observedDrawable;
@property double observedBackingScale;
- (NSString *)prepareMetal;
- (NSString *)show;
- (void)close;
- (NSDictionary *)status;
- (void)updateVisibility;
- (void)refreshLayout;
- (void)invalidateLayout;
@end
static NSString *displayMetalFailure(NSString *phase,NSError *error){
    // Only errors from our fixed shader/device/pipeline setup. No Guest pixels,
    // paths, request bodies or NSError userInfo are included in diagnostics.
    NSString *domain=error.domain?:@"none",*message=error.localizedDescription?:@"Metal returned nil without NSError";
    if(domain.length>128)domain=[domain substringToIndex:128];
    if(message.length>2048)message=[message substringToIndex:2048];
    NSData *json=[NSJSONSerialization dataWithJSONObject:@{@"event":@"native-display-error",@"phase":phase,@"error_domain":domain,@"error_code":@(error.code),@"message":message} options:0 error:nil];
    fwrite(json.bytes,1,json.length,stderr);fputc('\n',stderr);fflush(stderr);
    return [@"native_display_" stringByAppendingString:[phase stringByAppendingString:@"_failed"]];
}
@implementation VNDisplay
- (instancetype)init{if((self=[super init])){_presented=[VNPresentedSamples new];_heldKeys=[NSMutableSet new];_heldButtons=[NSMutableSet new];}return self;}
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)app{return NO;}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)app{[self close];return NSTerminateCancel;}
- (NSString *)prepareMetal {
    self.pipeline=nil;self.device=MTLCreateSystemDefaultDevice();self.commands=[self.device newCommandQueue];
    NSString *source=@"#include <metal_stdlib>\nusing namespace metal;struct O{float4 p[[position]];float2 uv;};vertex O veynode_display_vertex(uint i[[vertex_id]]){float2 p[3]={float2(-1,-1),float2(3,-1),float2(-1,3)};O o;o.p=float4(p[i],0,1);o.uv=float2((p[i].x+1)/2,(1-p[i].y)/2);return o;}fragment float4 veynode_display_fragment(O i[[stage_in]],texture2d<float> t[[texture(0)]]){constexpr sampler s(coord::normalized,address::clamp_to_edge,filter::linear);return float4(t.sample(s,i.uv).rgb,1);}";
    if(!self.device)return displayMetalFailure(@"device",nil);
    if(!self.commands)return displayMetalFailure(@"command_queue",nil);
    NSError *error=nil;id<MTLLibrary> lib=[self.device newLibraryWithSource:source options:nil error:&error];
    if(!lib)return displayMetalFailure(@"library_compile",error);
    MTLRenderPipelineDescriptor *desc=[MTLRenderPipelineDescriptor new];desc.vertexFunction=[lib newFunctionWithName:@"veynode_display_vertex"];desc.fragmentFunction=[lib newFunctionWithName:@"veynode_display_fragment"];desc.colorAttachments[0].pixelFormat=MTLPixelFormatBGRA8Unorm;
    if(!desc.vertexFunction||!desc.fragmentFunction)return displayMetalFailure(@"function_lookup",nil);
    error=nil;self.pipeline=[self.device newRenderPipelineStateWithDescriptor:desc error:&error];
    if(!self.pipeline)return displayMetalFailure(@"render_pipeline",error);
    return nil;
}
- (NSString *)show {
    NSAssert([NSThread isMainThread],@"AppKit thread");
    if(self.window){[self.window makeKeyAndOrderFront:nil];return nil;}
    NSString *failure=[self prepareMetal];if(failure)return failure;
    self.view=[[VNReadOnlyView alloc] initWithFrame:NSMakeRect(0,0,960,640) device:self.device];self.view.delegate=self;
    self.view.colorPixelFormat=MTLPixelFormatBGRA8Unorm;self.view.clearColor=MTLClearColorMake(0.03,0.03,0.03,1);self.view.framebufferOnly=YES;self.view.preferredFramesPerSecond=VN_VISIBLE_FPS;self.view.enableSetNeedsDisplay=NO;self.view.paused=NO;
    __weak VNDisplay *weak=self;self.view.inputEvent=^(NSEvent *event){[weak inputEvent:event];};
    NSView *container=[[NSView alloc] initWithFrame:NSMakeRect(0,0,960,640)];
    self.view.autoresizingMask=NSViewWidthSizable|NSViewHeightSizable;[container addSubview:self.view];
    self.inputLabel=[NSTextField labelWithString:@""];self.inputLabel.hidden=YES;self.inputLabel.frame=NSMakeRect(10,610,935,20);self.inputLabel.autoresizingMask=NSViewWidthSizable|NSViewMinYMargin;self.inputLabel.accessibilityLabel=@"Native input state";[container addSubview:self.inputLabel];
    self.window=[[NSWindow alloc] initWithContentRect:container.frame styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskClosable|NSWindowStyleMaskResizable|NSWindowStyleMaskMiniaturizable backing:NSBackingStoreBuffered defer:NO];
    self.layoutObserved=NO;self.windowEpoch++;self.lastGeneration=UINT64_MAX;self.lastSequence=UINT64_MAX;self.lastDrawableSize=CGSizeZero;
    self.window.title=@"Veynode VM Console";self.window.releasedWhenClosed=NO;self.window.delegate=self;self.window.contentView=container;self.window.acceptsMouseMovedEvents=YES;
    self.inputTimer=[NSTimer timerWithTimeInterval:0.05 repeats:YES block:^(NSTimer *timer){[weak updateInput];}];[[NSRunLoop mainRunLoop] addTimer:self.inputTimer forMode:NSRunLoopCommonModes];
    [publicationLock lock];enabled=YES;[publicationLock unlock];
    NSTitlebarAccessoryViewController *media=[NSTitlebarAccessoryViewController new];media.view=VNMediaControls();media.layoutAttribute=NSLayoutAttributeTrailing;[self.window addTitlebarAccessoryViewController:media];VNMediaWindowChanged(YES,NO);
    [self.window center];[self.window makeKeyAndOrderFront:nil];[NSApp activateIgnoringOtherApps:YES];[self updateVisibility];return nil;
}
- (void)close {
    [self suspendInputWithReason:@"window_closed"];[self.inputTimer invalidate];self.inputTimer=nil;
    NSAssert([NSThread isMainThread],@"AppKit thread");
    [publicationLock lock];enabled=NO;visible=NO;if(latest)[pool releasePixel:latest];latest=NULL;sourceGeneration=sourceSequence=0;sourceCapturedMono=0;[displayed invalidate];[frameAcquisition invalidateFrames];[publicationLock unlock];
    if(self.window)VNMediaWindowChanged(NO,NO);
    self.windowEpoch++;self.view.paused=YES;self.view.delegate=nil;self.window.delegate=nil;[self.window close];self.window=nil;self.view=nil;
    // A previously submitted GPU command retains its own frame lease until its
    // completion handler; closing a window never overwrites/recycles that slot.
}
- (BOOL)windowShouldClose:(NSWindow *)window {[self close];return NO;}
- (void)invalidateLayout {
    [self suspendInputWithReason:@"layout_changed"];
    [publicationLock lock];[displayed invalidate];[frameAcquisition invalidateFrames];[publicationLock unlock];
    self.lastGeneration=UINT64_MAX;self.lastSequence=UINT64_MAX;
}
- (void)refreshLayout {
    if(!self.view||!self.window)return;
    NSRect viewBounds=self.view.bounds,backingBounds=[self.view convertRectToBacking:self.view.bounds];
    CGSize drawable=self.view.drawableSize;BOOL flipped=self.view.flipped;double scale=self.window.backingScaleFactor;
    if(!self.layoutObserved||!NSEqualRects(viewBounds,self.observedViewBounds)||!NSEqualRects(backingBounds,self.observedBackingBounds)||!CGSizeEqualToSize(drawable,self.observedDrawable)||flipped!=self.observedFlipped||scale!=self.observedBackingScale){
        [self invalidateLayout];self.layoutObserved=YES;self.observedViewBounds=viewBounds;self.observedBackingBounds=backingBounds;self.observedDrawable=drawable;self.observedFlipped=flipped;self.observedBackingScale=scale;
    }
}
- (void)windowDidResize:(NSNotification *)notification{[self invalidateLayout];[self refreshLayout];}
- (void)windowDidChangeBackingProperties:(NSNotification *)notification{[self invalidateLayout];[self refreshLayout];}
- (void)updateVisibility {
    BOOL current=self.window&&self.window.visible&&!self.window.miniaturized&&(self.window.occlusionState&NSWindowOcclusionStateVisible);
    NSInteger maximum=self.window.screen.maximumFramesPerSecond;unsigned fps=maximum>0?(unsigned)MIN(maximum,(NSInteger)VN_VISIBLE_FPS):60;
    if(self.view&&self.view.preferredFramesPerSecond!=(NSInteger)fps)self.view.preferredFramesPerSecond=fps;
    [publicationLock lock];screenFPS=fps;BOOL changed=visible!=(enabled&&current);visible=enabled&&current;if(changed){[displayed invalidate];[frameAcquisition invalidateFrames];}[publicationLock unlock];if(changed){[self suspendInputWithReason:@"visibility_changed"];self.lastGeneration=UINT64_MAX;self.lastSequence=UINT64_MAX;}
}
- (void)windowDidChangeOcclusionState:(NSNotification *)notification{[self updateVisibility];}
- (void)windowDidChangeScreen:(NSNotification *)notification{[self updateVisibility];}
- (void)windowDidMiniaturize:(NSNotification *)notification{[self updateVisibility];VNMediaWindowChanged(YES,YES);}
- (void)windowDidDeminiaturize:(NSNotification *)notification{[self updateVisibility];VNMediaWindowChanged(YES,NO);}
- (void)windowDidBecomeKey:(NSNotification *)notification{VNMediaWindowBecameKey();}
- (void)suspendInput{[self suspendInputWithReason:@"explicit"];}
- (void)suspendInputWithReason:(NSString *)reason{[nativeControl suspendWithReason:reason];[self.heldKeys removeAllObjects];[self.heldButtons removeAllObjects];self.inputWasReady=NO;}
- (void)windowDidResignKey:(NSNotification *)notification{[self suspendInputWithReason:@"window_focus_lost"];}
- (void)applicationDidResignActive:(NSNotification *)notification{[self suspendInputWithReason:@"application_focus_lost"];}
// Unsaid states are the input lifecycle itself: waiting for focus, connecting,
// active, releasing and suspended after focus moved away.
static NSString *VNInputNotice(NSDictionary *s){
    if(![s[@"native_input_available"] boolValue])return @"View Only";
    NSString *state=s[@"native_input_state"]?:@"view_only";
    if([@[@"view_only",@"connecting",@"beginning",@"active",@"suspending",@"suspended"] containsObject:state])return nil;
    return [state isEqual:@"cleanup_unconfirmed"]?@"Input Disabled — releasing held keys was not confirmed":@"Input Disabled";
}
// Input follows focus: a focused, visible console window begins an activation
// and losing focus suspends it, releasing anything held in the guest.
- (void)beginIfFocused:(NSDictionary *)s{
    if(![s[@"native_input_available"] boolValue]||![@[@"view_only",@"suspended"] containsObject:s[@"native_input_state"]])return;
    if(!VNInputMayBegin(NSApp.active,self.window.keyWindow)||!self.window.visible||self.window.miniaturized)return;
    // Keys physically held before the activation are not owned synthetic presses.
    if(NSEvent.modifierFlags&(NSEventModifierFlagShift|NSEventModifierFlagControl|NSEventModifierFlagOption|NSEventModifierFlagCommand))return;
    double now=NSProcessInfo.processInfo.systemUptime;if(now-self.lastBegin<1)return;self.lastBegin=now;
    if(self.window.firstResponder!=self.view)[self.window makeFirstResponder:self.view];
    [nativeControl begin];
}
- (void)updateInput{
    NSDictionary *s=nativeInputStatus();BOOL ready=[s[@"input_enabled"] boolValue];
    if(ready&&!VNInputMayBegin(NSApp.active,self.window.keyWindow)){[self suspendInputWithReason:@"dispatch_focus_lost"];s=nativeInputStatus();ready=NO;}
    if(!ready){[self beginIfFocused:s];s=nativeInputStatus();}
    if(ready&&!self.inputWasReady)[self.window makeFirstResponder:self.view];
    if(ready&&!VNInputMayDispatch(NSApp.active,self.window.keyWindow,self.view&&self.window.firstResponder==self.view)){[self suspendInputWithReason:@"dispatch_focus_lost"];s=nativeInputStatus();ready=NO;}
    if(!ready){[self.heldKeys removeAllObjects];[self.heldButtons removeAllObjects];}
    self.inputWasReady=ready;
    // Input following focus is the normal case and goes unsaid, as in any
    // window; a line above the picture appears only when typing cannot work.
    NSString *notice=VNInputNotice(s);
    if(self.inputLabel.hidden!=(notice==nil)||(notice&&![notice isEqual:self.inputLabel.stringValue])){
        self.inputLabel.stringValue=notice?:@"";self.inputLabel.hidden=notice==nil;
        NSRect frame=self.view.superview.bounds;if(notice)frame.size.height-=40;self.view.frame=frame;
        self.window.title=notice?[@"Veynode VM Console — " stringByAppendingString:notice]:@"Veynode VM Console";
    }
}
- (void)inputEvent:(NSEvent *)e{
    if(!VNInputMayDispatch(NSApp.active,self.window.keyWindow,self.view&&self.window.firstResponder==self.view)){[self suspendInputWithReason:@"dispatch_focus_lost"];return;}
    if(![[nativeControl status][@"input_enabled"] boolValue])return;
    [self refreshLayout];NSDictionary *controlContext=[nativeControl acquisitionContext];
    [publicationLock lock];NSDictionary *frame=[displayed snapshot];double acquired=NSProcessInfo.processInfo.systemUptime;NSDictionary *acquisition=inputFrameDiagnostic.enabled?[displayed diagnosticToken:VNDisplayedFrameToken(frame) now:acquired]:nil;NSString *receipt=frameAcquisitionRequested?[frameAcquisition issueFromStore:displayed context:controlContext now:acquired]:nil;[publicationLock unlock];
    if(frameAcquisitionRequested&&!receipt){[self suspendInputWithReason:@"enqueue_rejected"];return;}
    @try {
    if(!frame){[self suspendInputWithReason:@"displayed_frame_missing"];return;}
    NSDictionary *token=VNDisplayedFrameToken(frame);NSDictionary *event=nil;
    if(e.type==NSEventTypeKeyDown||e.type==NSEventTypeKeyUp||e.type==NSEventTypeFlagsChanged){
        NSString *q=VNNativeQCode(e.keyCode);if(!q){if(e.type!=NSEventTypeKeyUp)[self suspendInputWithReason:@"unsupported_key"];return;}
        BOOL down=e.type==NSEventTypeKeyDown;
        if(e.type==NSEventTypeFlagsChanged&&!VNNativeModifierDown(e.keyCode,e.modifierFlags,&down))return;
        if(down&&[self.heldKeys containsObject:q])return;
        if(!down&&![self.heldKeys containsObject:q])return;
        event=@{@"kind":@"key",@"qcode":q,@"down":@(down)};
        if([nativeControl enqueueEvent:event frame:token acquisition:acquisition receipt:receipt context:controlContext]){receipt=nil;if(down)[self.heldKeys addObject:q];else[self.heldKeys removeObject:q];}else[self suspendInputWithReason:@"enqueue_rejected"];return;
    }
    NSPoint point=[self.view convertPointToBacking:[self.view convertPoint:e.locationInWindow fromView:nil]];VNDisplayGeometry geometry;size_t x,y;
    if(!VNDisplayedFrameGeometry(frame,&geometry)||!VNDisplayMapBackingPoint(geometry,point.x,point.y,&x,&y)){if(self.heldButtons.count)[self suspendInputWithReason:@"pointer_outside"];return;}
    if(e.type==NSEventTypeMouseMoved||e.type==NSEventTypeLeftMouseDragged||e.type==NSEventTypeRightMouseDragged||e.type==NSEventTypeOtherMouseDragged)event=@{@"kind":@"move",@"x":@(x),@"y":@(y)};
    else if(e.type==NSEventTypeScrollWheel){double dx=e.scrollingDeltaX,dy=e.scrollingDeltaY;if(!isfinite(dx)||!isfinite(dy))return;double delta=fabs(dy)>=fabs(dx)?dy:dx;if(delta==0)return;NSString *direction=fabs(dy)>=fabs(dx)?(dy>0?@"up":@"down"):(dx>0?@"left":@"right");event=@{@"kind":@"scroll",@"direction":direction,@"ticks":@(MIN(20,MAX(1,(int)ceil(fabs(delta))))),@"x":@(x),@"y":@(y)};}
    else{if(e.buttonNumber>2)return;NSString *button=@[@"left",@"right",@"middle"][e.buttonNumber];BOOL down=e.type==NSEventTypeLeftMouseDown||e.type==NSEventTypeRightMouseDown||e.type==NSEventTypeOtherMouseDown;if(down==[self.heldButtons containsObject:button])return;event=@{@"kind":@"button",@"button":button,@"down":@(down),@"x":@(x),@"y":@(y)};if([nativeControl enqueueEvent:event frame:token acquisition:acquisition receipt:receipt context:controlContext]){receipt=nil;if(down)[self.heldButtons addObject:button];else[self.heldButtons removeObject:button];}else[self suspendInputWithReason:@"enqueue_rejected"];return;}
    if(event){if([nativeControl enqueueEvent:event frame:token acquisition:acquisition receipt:receipt context:controlContext])receipt=nil;else[self suspendInputWithReason:@"enqueue_rejected"];}
    } @finally {[frameAcquisition cancel:receipt];}
}
- (NSDictionary *)status {
    [self updateVisibility];[self refreshLayout];[self updateInput];
    NSMutableDictionary *value=[VeynodeDisplayCapabilities() mutableCopy];value[@"window_open"]=(self.window!=nil?@YES:@NO);value[@"window_visible"]=(self.window.visible?@YES:@NO);unsigned visibleFPS=VeynodeDisplayVisibleFPS();value[@"capture_current_limit_fps"]=@(visibleFPS?visibleFPS:VN_BACKGROUND_FPS);value[@"capture_native_visible"]=@(VeynodeDisplayVisible());value[@"physical_presentations"]=[NSNull null];value[@"display_gpu_inflight"]=@(self.inflight);value[@"drawables_submitted"]=@(self.submitted);value[@"gpu_commands_completed"]=@(self.completed);value[@"gpu_commands_failed"]=@(self.failed);value[@"pool"]=[pool statistics];
    BOOL appActive=NSApp.active,windowKey=self.window.keyWindow,firstResponder=self.view&&self.window.firstResponder==self.view;
    value[@"native_input_focus"]=@{@"application_active":(appActive?@YES:@NO),@"window_key":(windowKey?@YES:@NO),@"first_responder":(firstResponder?@YES:@NO),@"input_target_focused":(VNInputMayDispatch(appActive,windowKey,firstResponder)?@YES:@NO)};
    CGSize drawable=self.view?self.view.drawableSize:CGSizeZero;
    value[@"drawable_width"]=@(drawable.width);value[@"drawable_height"]=@(drawable.height);
    value[@"backing_scale"]=@(self.window?self.window.backingScaleFactor:0);
    value[@"window_content_width_points"]=@(self.view?self.view.bounds.size.width:0);
    value[@"window_content_height_points"]=@(self.view?self.view.bounds.size.height:0);
    [publicationLock lock];
    NSUInteger sw=latest?CVPixelBufferGetWidth(latest):0,sh=latest?CVPixelBufferGetHeight(latest):0;
    double scale=sw&&sh?fmin(drawable.width/sw,drawable.height/sh):0;
    value[@"display_source_width"]=@(sw);value[@"display_source_height"]=@(sh);
    value[@"viewport_width_pixels"]=@(sw*scale);value[@"viewport_height_pixels"]=@(sh*scale);
    value[@"frame_available"]=(latest!=NULL?@YES:@NO);value[@"source_generation"]=@(sourceGeneration);value[@"source_sequence"]=@(sourceSequence);NSDictionary *shown=[displayed snapshot];value[@"displayed_frame_available"]=shown?@YES:@NO;value[@"displayed_frame"]=shown?:[NSNull null];value[@"displayed_frame_authorizes_input"]=@NO;value[@"displayed_frame_age_ms"]=shown?@(MAX(0,NSProcessInfo.processInfo.systemUptime-[shown[@"callback_received_monotonic_seconds"] doubleValue])*1000):[NSNull null];[publicationLock unlock];[value addEntriesFromDictionary:[self.presented statistics]];if(inputFrameDiagnostic)value[@"input_frame_diagnostic"]=[inputFrameDiagnostic statistics];if(frameAcquisition)value[@"frame_acquisition"]=[frameAcquisition statistics];value[@"draw_timing"]=[drawTiming statistics];return value;
}
- (void)mtkView:(MTKView *)view drawableSizeWillChange:(CGSize)size{[self invalidateLayout];}
- (void)drawInMTKView:(MTKView *)view {
    // Timing is observed only when the opt-in diagnostic is enabled; every branch below is unchanged.
    VNDrawTiming *timing=drawTiming;BOOL timed=timing.enabled;double entry=timed?NSProcessInfo.processInfo.systemUptime:0;
    [self updateVisibility];[self refreshLayout];
    if(self.inflight){[timing recordCallbackBranch:VNDrawBranchInflight];return;}
    if(!self.window){[timing recordCallbackBranch:VNDrawBranchNoWindow];return;}
    if(!VeynodeDisplayVisible()){[timing recordCallbackBranch:VNDrawBranchInvisible];return;}
    [publicationLock lock];CVPixelBufferRef frame=latest;uint64_t generation=sourceGeneration,sequence=sourceSequence;int64_t captured=sourceCapturedMono;uint64_t submissionRevision=displayed.revision;
    BOOL leaseLost=NO;if(frame&&![pool retainPixel:frame]){frame=NULL;leaseLost=YES;}[publicationLock unlock];
    if(generation==self.lastGeneration&&sequence==self.lastSequence&&CGSizeEqualToSize(view.drawableSize,self.lastDrawableSize)){if(frame)[pool releasePixel:frame];[timing recordCallbackBranch:VNDrawBranchUnchangedSource];return;}
    uint64_t displayEpoch=self.windowEpoch;CGSize drawableSize=view.drawableSize;
    double acquireBegin=timed?NSProcessInfo.processInfo.systemUptime:0;
    id<CAMetalDrawable> drawable=view.currentDrawable;MTLRenderPassDescriptor *pass=view.currentRenderPassDescriptor;
    double acquireEnd=timed?NSProcessInfo.processInfo.systemUptime:0;
    if(!drawable||!pass){
        uint64_t unacquired=[timing admitAttemptWithEntry:entry acquireBegin:acquireBegin acquireEnd:acquireEnd windowEpoch:displayEpoch generation:(frame?generation:0) sequence:(frame?sequence:0) revision:submissionRevision drawableWidth:0 drawableHeight:0 clear:frame==NULL leaseLost:leaseLost];
        if(frame)[pool releasePixel:frame];[timing attempt:unacquired terminal:VNDrawBranchNoDrawableOrPass];return;
    }
    // Freeze actual target pixels after drawable acquisition, not a possibly
    // changing view size. Render viewport and observation use this same value.
    drawableSize=CGSizeMake(drawable.texture.width,drawable.texture.height);
    uint64_t attempt=[timing admitAttemptWithEntry:entry acquireBegin:acquireBegin acquireEnd:acquireEnd windowEpoch:displayEpoch generation:(frame?generation:0) sequence:(frame?sequence:0) revision:submissionRevision drawableWidth:(NSUInteger)drawableSize.width drawableHeight:(NSUInteger)drawableSize.height clear:frame==NULL leaseLost:leaseLost];
    id<MTLCommandBuffer> command=[self.commands commandBuffer];id<MTLRenderCommandEncoder> render=[command renderCommandEncoderWithDescriptor:pass];
    if(!command||!render){if(frame)[pool releasePixel:frame];self.failed++;[timing attempt:attempt terminal:VNDrawBranchCommandOrEncoderFailed];return;}
    VNDisplayGeometry geometry={0};
    if(frame){
        NSUInteger w=CVPixelBufferGetWidth(frame),h=CVPixelBufferGetHeight(frame);
        MTLTextureDescriptor *desc=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:w height:h mipmapped:NO];desc.storageMode=MTLStorageModeShared;desc.usage=MTLTextureUsageShaderRead;
        id<MTLTexture> texture=[self.device newTextureWithDescriptor:desc iosurface:CVPixelBufferGetIOSurface(frame) plane:0];
        if(!texture){[render endEncoding];[pool releasePixel:frame];self.failed++;[timing attempt:attempt terminal:VNDrawBranchTextureFailed];return;}
        double scale=fmin(drawableSize.width/w,drawableSize.height/h);double dw=w*scale,dh=h*scale;
        NSRect vb=view.bounds,bb=[view convertRectToBacking:view.bounds];
        geometry=(VNDisplayGeometry){.sourceWidth=w,.sourceHeight=h,.drawableWidth=drawableSize.width,.drawableHeight=drawableSize.height,.viewBounds={vb.origin.x,vb.origin.y,vb.size.width,vb.size.height},.backingBounds={bb.origin.x,bb.origin.y,bb.size.width,bb.size.height},.viewport={(drawableSize.width-dw)/2,(drawableSize.height-dh)/2,dw,dh},.viewFlipped=view.flipped};
        [render setViewport:(MTLViewport){(drawableSize.width-dw)/2,(drawableSize.height-dh)/2,dw,dh,0,1}];
        [render setRenderPipelineState:self.pipeline];[render setFragmentTexture:texture atIndex:0];[render drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    }
    [render endEncoding];self.inflight=YES;self.submitted++;
    // Do not newly submit a frame whose generation retired while building its
    // command. An already submitted GPU command cannot be retroactively erased;
    // next tick clears it, and its slot remains pinned through completion.
    [publicationLock lock];
    BOOL valid=!frame||(enabled&&sourceGeneration==generation);
    if(valid){
        NSDictionary *candidate=frame?[displayed candidateGeometry:geometry generation:generation sequence:sequence windowEpoch:displayEpoch capturedMonotonicUS:captured]:nil;
        // Register completion ownership only for a committed command. Metal runs the
        // completed handlers of a command buffer deallocated without commit, so a
        // handler registered before this retired-generation check released the frame
        // a second time on the !valid path below.
        [command addCompletedHandler:^(id<MTLCommandBuffer> completed){
            [timing attempt:attempt completionEnteredAt:(timed?NSProcessInfo.processInfo.systemUptime:0) status:completed.status gpuStart:(timed?completed.GPUStartTime:0) gpuEnd:(timed?completed.GPUEndTime:0)];
            if(frame)[pool releasePixel:frame];
            dispatch_async(dispatch_get_main_queue(),^{
                self.inflight=NO;VNDrawCompletionGate gate=VNDrawGateCommandFailed;
                if(completed.status==MTLCommandBufferStatusCompleted){
                    self.completed++;[publicationLock lock];
                    if(self.windowEpoch!=displayEpoch)gate=VNDrawGateEpochChanged;
                    else if(displayed.revision!=submissionRevision)gate=VNDrawGateRevisionChanged;
                    else{self.lastGeneration=generation;self.lastSequence=sequence;self.lastDrawableSize=drawableSize;gate=VNDrawGateApplied;}
                    [publicationLock unlock];
                }else{self.failed++;}
                [timing attempt:attempt inflightClearedAt:(timed?NSProcessInfo.processInfo.systemUptime:0) gate:gate];
            });
        }];
        [drawable addPresentedHandler:^(id<MTLDrawable> shown){
            double time=shown.presentedTime,received=NSProcessInfo.processInfo.systemUptime;
            [timing attempt:attempt presentedEnteredAt:received metalPresented:time];
            dispatch_async(dispatch_get_main_queue(),^{
                [self.presented recordTime:time generation:generation sequence:sequence windowEpoch:displayEpoch];
                [self updateVisibility];[self refreshLayout];
                [publicationLock lock];
                // macOS 27 runs this handler with presentedTime 0 for every drawable of
                // a visible window; the handler itself still means Core Animation took
                // it, so its own time on the same clock stands in.
                double stamp=isfinite(time)&&time>0?time:CACurrentMediaTime();
                if(enabled&&visible&&self.windowEpoch==displayEpoch&&sourceGeneration==generation)[displayed commitCandidate:candidate presentedTime:stamp receivedMonotonic:received];
                [publicationLock unlock];
                [timing attempt:attempt presentedMainAt:(timed?NSProcessInfo.processInfo.systemUptime:0)];
            });
        }];
        [command presentDrawable:drawable];[command commit];
        [timing attempt:attempt committedAt:(timed?NSProcessInfo.processInfo.systemUptime:0) clear:frame==NULL];
    }
    [publicationLock unlock];
    if(!valid){self.inflight=NO;self.submitted--;if(frame)[pool releasePixel:frame];[timing attempt:attempt terminal:VNDrawBranchRetiredGeneration];}

}
@end
static int drawTimingStatusSelfTest(void){@autoreleasepool{
    initializeState();NSCAssert(NSApp==nil,@"no AppKit application");
    drawTiming=[[VNDrawTiming alloc] initWithEnabled:YES];
    inputFrameDiagnostic=[[VNInputFrameDiagnostic alloc] initWithEnabled:YES];
    VNDisplay *headless=[VNDisplay new];
    VNDisplayGeometry g={.sourceWidth=8192,.sourceHeight=8192,.drawableWidth=8192,.drawableHeight=8192,.viewBounds={0,0,8192,8192},.backingBounds={0,0,8192,8192},.viewport={0,0,8192,8192}};
    NSDictionary *c=[displayed candidateGeometry:g generation:9007199254740991ULL sequence:9007199254740991ULL windowEpoch:9007199254740991ULL capturedMonotonicUS:9007199254740991LL];
    NSCAssert([displayed commitCandidate:c presentedTime:100 receivedMonotonic:100],@"synthetic component candidate");
    NSDictionary *token=VNDisplayedFrameToken([displayed snapshot]);
    for(int i=0;i<16;i++){NSNumber *n=[inputFrameDiagnostic acquireToken:token observation:[displayed diagnosticToken:token now:1000.123456789] epoch:9007199254740991ULL];[inputFrameDiagnostic dispatched:n sequence:9007199254740991ULL epoch:9007199254740991ULL now:1000.123456999 queueAge:.199999999];[inputFrameDiagnostic validated:n token:token observation:[displayed diagnosticToken:token now:1000.123459999] accepted:NO reason:@"display_historical_expired"];}
    for(int i=0;i<120;i++)[headless.presented recordTime:1000+i*.0123456789 generation:9007199254740991ULL sequence:9007199254740991ULL windowEpoch:9007199254740991ULL];
    // 40 fully observed attempts saturate the 32-record ring: 8 evictions, none pending.
    for(int i=0;i<40;i++){
        double base=1000+i*0.0166667;
        uint64_t a=[drawTiming admitAttemptWithEntry:base acquireBegin:base+0.00011 acquireEnd:base+0.00042 windowEpoch:9007199254740991ULL generation:9007199254740991ULL sequence:9007199254740991ULL revision:9007199254740991ULL drawableWidth:3840 drawableHeight:2160 clear:NO leaseLost:NO];
        [drawTiming attempt:a committedAt:base+0.00100 clear:NO];
        [drawTiming attempt:a completionEnteredAt:base+0.01800 status:4 gpuStart:83734.123456789+i*0.0166667 gpuEnd:83734.130123456+i*0.0166667];
        [drawTiming attempt:a inflightClearedAt:base+0.01802 gate:VNDrawGateApplied];
        [drawTiming attempt:a presentedEnteredAt:base+0.02500 metalPresented:83734.200123456+i*0.0166667];
        [drawTiming attempt:a presentedMainAt:base+0.02502];
    }
    NSMutableDictionary *status=[[headless status] mutableCopy];
    NSDictionary *draw=status[@"draw_timing"];
    NSCAssert([draw[@"records"] count]==32&&[draw[@"evicted"] unsignedLongLongValue]==8&&[draw[@"pending_evicted"] unsignedLongLongValue]==0&&[draw[@"attempts_issued"] unsignedLongLongValue]==40,@"saturated draw timing ring");
    NSCAssert([status[@"input_frame_diagnostic"][@"records"] count]==16&&[status[@"presented_time_samples"] count]==120,@"saturated rings");
    // Measured: the existing saturated baseline with a minimal disabled draw_timing object is 38121 bytes.
    // With 32 fully populated draw timing records it measured 55357 bytes; it must stay below the 56 KiB wire budget.
    NSData *wire=[NSJSONSerialization dataWithJSONObject:@{@"event":@"response",@"epoch":@0,@"id":@"draw-timing-status-bound",@"result":status} options:NSJSONWritingWithoutEscapingSlashes error:nil];
    NSCAssert(wire&&wire.length<56*1024,@"saturated status must retain wire headroom");
    printf("PASS saturated actual VNDisplay status with 32 draw timing records (8 evicted, 0 pending), 16 input diagnostics and 120 presentation samples: %lu bytes, below 56KiB; no AppKit/GPU.\n",(unsigned long)wire.length);
    drawTiming=[[VNDrawTiming alloc] initWithEnabled:NO];return 0;
}}
static VNDisplay *display;
void VeynodeDisplayCommand(NSString *method,NSDictionary *frame,NSNumber *diagnosticID,NSDictionary *receiptFields,void (^completion)(NSDictionary *,NSString *)){
    NSTimeInterval issued=NSProcessInfo.processInfo.systemUptime;
    dispatch_async(dispatch_get_main_queue(),^{@autoreleasepool{
        if(NSProcessInfo.processInfo.systemUptime-issued>1.0){completion(nil,@"native_window_request_expired");return;}
        NSString *error=nil;
        if([method isEqual:@"validate_displayed_frame"]){[display updateVisibility];[display refreshLayout];BOOL receiptFocus=YES;if(receiptFields){receiptFocus=[frameAcquisition requireFocusReady:[[nativeControl status][@"input_enabled"] boolValue] applicationActive:NSApp.active windowKey:display.window.keyWindow firstResponder:(display.view&&display.window.firstResponder==display.view)];if(!receiptFocus)[display suspendInputWithReason:@"dispatch_focus_lost"];}[publicationLock lock];NSString *reason=nil;double now=NSProcessInfo.processInfo.systemUptime;NSDictionary *r=receiptFields?(receiptFocus?[frameAcquisition consume:receiptFields[@"acquisition_receipt"] token:frame epoch:[receiptFields[@"epoch"] unsignedLongLongValue] sequence:[receiptFields[@"sequence"] unsignedLongLongValue] store:displayed now:now failure:&reason]:nil):[displayed validateToken:frame now:now failure:&reason];if(receiptFields&&!receiptFocus)reason=@"acquisition_invalidated";NSDictionary *observation=[displayed diagnosticToken:frame now:now];if(receiptFields&&r){double received=[r[@"frame"][@"callback_received_monotonic_seconds"] doubleValue];observation=@{@"monotonic_seconds":@(now),@"record_found":@YES,@"current":r[@"current"],@"callback_received_monotonic_seconds":@(received),@"callback_age_ms":@((now-received)*1000)};}if(diagnosticID)[inputFrameDiagnostic validated:diagnosticID token:frame observation:observation accepted:r!=nil reason:reason];[publicationLock unlock];completion(r,r?nil:(reason?:@"display_no_snapshot"));return;}
        if([method isEqual:@"show_window"])error=[display show];
        else if([method isEqual:@"close_window"])[display close];
        if(error)completion(nil,error);else completion([display status],nil);
    }});
}
static int inputFrameDiagnosticStatusSelfTest(void){@autoreleasepool{
    initializeState();NSCAssert(NSApp==nil,@"no AppKit application");
    VNDisplay *headless=[VNDisplay new];inputFrameDiagnostic=[[VNInputFrameDiagnostic alloc] initWithEnabled:YES];
    VNDisplayGeometry g={.sourceWidth=8192,.sourceHeight=8192,.drawableWidth=8192,.drawableHeight=8192,.viewBounds={0,0,8192,8192},.backingBounds={0,0,8192,8192},.viewport={0,0,8192,8192}};
    NSDictionary *c=[displayed candidateGeometry:g generation:9007199254740991ULL sequence:9007199254740991ULL windowEpoch:9007199254740991ULL capturedMonotonicUS:9007199254740991LL];
    NSCAssert([displayed commitCandidate:c presentedTime:100 receivedMonotonic:100],@"synthetic component candidate");
    NSDictionary *token=VNDisplayedFrameToken([displayed snapshot]);
    for(int i=0;i<16;i++){NSNumber *n=[inputFrameDiagnostic acquireToken:token observation:[displayed diagnosticToken:token now:1000.123456789] epoch:9007199254740991ULL];[inputFrameDiagnostic dispatched:n sequence:9007199254740991ULL epoch:9007199254740991ULL now:1000.123456999 queueAge:.199999999];[inputFrameDiagnostic validated:n token:token observation:[displayed diagnosticToken:token now:1000.123459999] accepted:NO reason:@"display_historical_expired"];}
    for(int i=0;i<120;i++)[headless.presented recordTime:1000+i*.0123456789 generation:9007199254740991ULL sequence:9007199254740991ULL windowEpoch:9007199254740991ULL];
    NSMutableDictionary *status=[[headless status] mutableCopy];
    NSCAssert([status[@"input_frame_diagnostic"][@"records"] count]==16&&[status[@"presented_time_samples"] count]==120,@"saturated rings");
    // Leave at least 8KiB within a conservative 64KiB envelope for the existing
    // GLib-owner counters/copy aggregates/suspensions. No wire limit is increased.
    NSData *wire=[NSJSONSerialization dataWithJSONObject:@{@"event":@"response",@"epoch":@0,@"id":@"diagnostic-status-bound",@"result":status} options:NSJSONWritingWithoutEscapingSlashes error:nil];
    NSCAssert(wire&&wire.length<56*1024,@"saturated status must retain wire headroom");
    printf("PASS saturated actual VNDisplay status: %lu bytes, 16 complete diagnostics + 120 presentation samples, below 56KiB conservative budget; no AppKit/GPU.\n",(unsigned long)wire.length);return 0;
}}
int VeynodeDisplayHeadlessSelfTest(void){@autoreleasepool{
    initializeState();NSCAssert(NSApp==nil,@"test must not initialize NSApplication or open a window");
    VNDisplay *headless=[VNDisplay new];
    for(int phase=0;phase<3;phase++){
        if(phase==1)nativeControl=[[VNNativeControl alloc] initWithSocket:@"/nonexistent/control.sock" vm:@"headless-test" worker:[@"a" stringByPaddingToLength:64 withString:@"a" startingAtIndex:0] display:[displayed instance]];
        if(phase==2)nativeControl=nil;
        for(int cycle=0;cycle<4;cycle++){
            NSDictionary *s=[headless status];NSData *wire=[NSJSONSerialization dataWithJSONObject:s options:0 error:nil];NSCAssert(wire,@"real status is serializable");
            NSDictionary *value=[NSJSONSerialization JSONObjectWithData:wire options:0 error:nil];
            for(NSString *key in @[@"window_open",@"window_visible",@"native_input_ready",@"native_input_available",@"input_enabled",@"frame_available",@"displayed_frame_available"]){NSCAssert(CFGetTypeID((__bridge CFTypeRef)value[key])==CFBooleanGetTypeID()&&![value[key] boolValue],@"headless runtime bool must remain false: %@",key);}
            NSCAssert([value[@"native_release_confirmed"] boolValue]&&[value[@"display_only"] boolValue],@"headless owns no inputs");
            NSCAssert([value[@"native_input_state"] isEqual:phase==1?@"connecting":@"view_only"],@"actual connection state retained without a label view");
            NSCAssert(headless.window==nil&&headless.view==nil&&headless.inputLabel==nil&&NSApp==nil,@"status/close never constructs UI");
            [headless close];VeynodeDisplayInvalidate();
        }
    }
    puts("PASS real headless VNDisplay status/close/invalidate lifecycle, before native channel and while unstarted connecting, typed JSON and no NSApplication/window/input");return 0;
}}
int VeynodeDisplaySelfTest(void){@autoreleasepool{
    initializeState();
    const size_t geometry[][5]={{2560,1440,1,1920,1080},{1080,1920,1,606,1080},{640,480,1,640,480},{3840,2160,1,1920,1080},{2,2,1,2,2},{1280,800,1,1280,800},{1280,800,0,1152,720},{2560,1440,0,1280,720},{3840,2160,0,1280,720},{1080,1920,0,404,720}};
    for(size_t i=0;i<sizeof(geometry)/sizeof(geometry[0]);i++){size_t w=0,h=0;NSCAssert(VNEncodedGeometry(geometry[i][0],geometry[i][1],geometry[i][2],&w,&h)&&w==geometry[i][3]&&h==geometry[i][4],@"codec aspect-fit actual geometry");}
    size_t w=99,h=99;
    NSCAssert(!VNEncodedGeometry(0,1440,true,&w,&h)&&w==0&&h==0,@"zero source fails closed");
    NSCAssert(!VNEncodedGeometry(2561,1440,true,&w,&h),@"odd source outside scanout contract");
    NSCAssert(!VNEncodedGeometry(4096,2160,true,&w,&h),@"source exceeds capture bound");
    NSCAssert(!VNEncodedGeometry(2,2160,true,&w,&h),@"too thin to retain two coded pixels");
    VNPresentedSamples *measurements=[VNPresentedSamples new];
    for(NSNumber *bad in @[@0,@(-1),@(NAN),@(INFINITY)])[measurements recordTime:bad.doubleValue generation:1 sequence:1 windowEpoch:1];
    for(int i=1;i<=130;i++)[measurements recordTime:(double)i generation:2 sequence:i windowEpoch:1];
    [measurements recordTime:130 generation:2 sequence:131 windowEpoch:1];
    [measurements recordTime:129 generation:2 sequence:132 windowEpoch:1];
    NSDictionary *stats=[measurements statistics];
    NSCAssert([stats[@"presented_time_valid_count"] intValue]==130&&[stats[@"presented_time_invalid_count"] intValue]==6,@"zero/nonfinite/nonmonotonic are not presentations");
    NSCAssert([stats[@"presented_time_samples"] count]==120&&[stats[@"presented_time_evicted_count"] intValue]==10,@"bounded presentation metadata ring");
    NSCAssert([stats[@"presented_time_samples"][0][@"source_sequence"] intValue]==11,@"preserve actual source sequence with timestamp");
    VNFramePacing pacing={0};NSUInteger frames=0;
    for(int i=0;i<600;i++){int64_t t=(int64_t)i*1000000/60;frames+=VNFramePacingAdmit(&pacing,t,VN_VISIBLE_FPS);NSCAssert(!VNFramePacingAdmit(&pacing,t,VN_VISIBLE_FPS),@"same timestamp cannot catch up");}
    NSCAssert(frames==600,@"a 240 Hz ceiling admits every draw of a 60 Hz source");
    frames=0;for(int i=0;i<1000;i++)frames+=VNFramePacingAdmit(&pacing,20000000+(int64_t)i*1000,VN_VISIBLE_FPS);
    NSCAssert(frames==241,@"rational 240 Hz ceiling has no integer drift (the slot after the last event is claimed early)");
    frames=0;for(int i=0;i<1000;i++)frames+=VNFramePacingAdmit(&pacing,22000000+(int64_t)i*1000,120);
    NSCAssert(frames==121,@"a 120 Hz screen admits 120 of a 1 kHz source, plus the next slot claimed early");
    frames=0;for(int i=0;i<1000;i++)frames+=VNFramePacingAdmit(&pacing,24000000+(int64_t)i*1000,1000);
    NSCAssert(frames==241,@"a faster screen is held to the 240 Hz ceiling");
    VNFramePacing busy={0};frames=0;for(int64_t t=40000000;t<50000000;){if(VNFramePacingAdmit(&busy,t,120)){frames++;t+=7300;}else t+=2000;}
    NSCAssert(frames>=1199&&frames<=1201,@"7.3 ms of work per frame still keeps a 120 Hz cadence");
    frames=0;for(int i=0;i<600;i++)frames+=VNFramePacingAdmit(&pacing,10000000+(int64_t)i*1000000/60,VN_BACKGROUND_FPS);
    NSCAssert(frames==300,@"background 30 Hz admits half a 60 Hz source");
    NSCAssert(VNFramePacingAdmit(&pacing,30000000,VN_BACKGROUND_FPS)&&!VNFramePacingAdmit(&pacing,30000000,VN_BACKGROUND_FPS),@"late event cannot create catch-up burst");
    NSCAssert(VNFramePacingAdmit(&pacing,30000001,60),@"a rate change starts fresh cadence");
    NSCAssert(VNFramePacingDue(&pacing,120)==0,@"another rate is due at once");
    for(unsigned fps=1;fps<=1000;fps+=fps<60?7:fps<300?60:350){VNFramePacing due={0};VNFramePacingAdmit(&due,50000000,fps);for(int i=0;i<4;i++){int64_t at=VNFramePacingDue(&due,fps);VNFramePacing early=due;NSCAssert(!VNFramePacingAdmit(&early,at-1,fps)&&VNFramePacingAdmit(&due,at,fps),@"due is the first admitted time");}}
    CVPixelBufferRef a=VeynodeDisplayAcquire(128,80),b=VeynodeDisplayAcquire(128,80),c=VeynodeDisplayAcquire(128,80);NSCAssert(a&&b&&c&&a!=b&&b!=c&&a!=c,@"three owned slots");NSCAssert(!VeynodeDisplayAcquire(128,80),@"bounded pool");
    [publicationLock lock];enabled=YES;[publicationLock unlock];
    VeynodeDisplayPublish(a,1,1,1000);NSCAssert([pool retainPixel:a],@"GPU lease");VeynodeDisplayRelease(a);VeynodeDisplayInvalidate();
    NSCAssert(!VeynodeDisplayAcquire(128,80),@"invalidated in-flight cannot be reused");[pool releasePixel:a];
    CVPixelBufferRef replacement=VeynodeDisplayAcquire(256,160);NSCAssert(replacement&&CVPixelBufferGetWidth(replacement)==256,@"retired free slot can resize");
    VeynodeDisplayPublish(b,2,1,1000);VeynodeDisplayPublish(c,2,2,1000);VeynodeDisplayRelease(b);VeynodeDisplayRelease(c);
    [publicationLock lock];enabled=NO;[publicationLock unlock];VeynodeDisplayInvalidate();VeynodeDisplayRelease(replacement);
    NSCAssert([[pool statistics][@"leases"] unsignedIntegerValue]==0,@"close/invalidation releases all except actual GPU leases");
    CVPixelBufferRef d=VeynodeDisplayAcquire(128,80);VeynodeDisplayPublish(d,3,1,1000);VeynodeDisplayRelease(d);NSCAssert(!latest,@"closed display cannot republish");
    puts("PASS HEVC/H264 landscape/portrait/small/max geometry and invalid boundaries; bounded presentation metadata policy (synthetic inputs only), rational per-screen (at most 240) and 30 Hz pacing, policy transition and no catch-up burst; actual CVPixelBuffer bounded slot leases, replacement, in-flight pin, generation invalidation and closed publication. No AppKit window, SPICE connection or GPU command.");return 0;
}}
int main(int argc,char **argv){@autoreleasepool{
    initializeState();
    BOOL frameDiagnostics=NO,drawTimingRequested=NO;
    while(argc>3){if(!strcmp(argv[argc-1],"--input-frame-diagnostic")&&!frameDiagnostics){frameDiagnostics=YES;argc--;}else if(!strcmp(argv[argc-1],"--native-frame-acquisition")&&!frameAcquisitionRequested){frameAcquisitionRequested=YES;argc--;}else if(!strcmp(argv[argc-1],"--draw-timing-diagnostic")&&!drawTimingRequested){drawTimingRequested=YES;argc--;}else break;}
    frameAcquisition=[VNFrameAcquisition new];
    inputFrameDiagnostic=[[VNInputFrameDiagnostic alloc] initWithEnabled:frameDiagnostics];
    drawTiming=[[VNDrawTiming alloc] initWithEnabled:drawTimingRequested];
    if(argc==2&&!strcmp(argv[1],"--input-frame-diagnostic-self-test"))return inputFrameDiagnosticStatusSelfTest();
    if(argc==2&&!strcmp(argv[1],"--draw-timing-status-self-test"))return drawTimingStatusSelfTest();
    if(argc==2&&!strcmp(argv[1],"--display-headless-self-test"))return VeynodeDisplayHeadlessSelfTest();
    if(argc==2&&!strcmp(argv[1],"--display-self-test"))return VeynodeDisplaySelfTest();
    if(argc==2&&!strcmp(argv[1],"--display-metal-self-test")){VNDisplay *check=[VNDisplay new];if([check prepareMetal])return 1;puts("PASS actual Metal device, command queue, fixed shader compilation, function lookup and render pipeline creation; no window or GPU command submission.");return 0;}
    if(argc<3||strcmp(argv[2],"--persistent"))return VeynodeConsoleMain(argc,argv);
    if(argc!=3&&argc!=9){fprintf(stderr,"Invalid native control configuration\n");return 2;}
    if(argc==9){if(strcmp(argv[3],"--control-socket")||strcmp(argv[5],"--vm-id")||strcmp(argv[7],"--worker-instance")){fprintf(stderr,"Invalid native control configuration\n");return 2;}nativeControl=[[VNNativeControl alloc] initWithSocket:@(argv[4]) vm:@(argv[6]) worker:@(argv[8]) display:[displayed instance]];nativeControl.frameDiagnostic=inputFrameDiagnostic;nativeControl.frameAcquisition=frameAcquisition;nativeControl.frameAcquisitionRequested=frameAcquisitionRequested;[nativeControl start];}
    [NSApplication sharedApplication];[NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];display=[VNDisplay new];NSApp.delegate=display;
    __block int code=1;
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED,0),^{code=VeynodeConsoleMain(3,argv);dispatch_async(dispatch_get_main_queue(),^{[display close];[NSApp stop:nil];[NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0 context:nil subtype:0 data1:0 data2:0] atStart:NO];});});
    [NSApp run];return code;
}}
