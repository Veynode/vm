#import "Display.h"
#import "FramePool.h"
#import "Presented.h"
#import "DisplayedFrame.h"
#import "NativeControl.h"
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
static VNNativeControl *nativeControl;
static void initializeState(void){static dispatch_once_t once;dispatch_once(&once,^{pool=[VNFramePool new];publicationLock=[NSLock new];displayed=[VNDisplayedFrameStore new];});}
CVPixelBufferRef VeynodeDisplayAcquire(NSUInteger w,NSUInteger h){initializeState();return [pool acquireWidth:w height:h];}
void VeynodeDisplayRelease(CVPixelBufferRef pixel){[pool releasePixel:pixel];}
void VeynodeDisplayPublish(CVPixelBufferRef pixel,uint64_t generation,uint64_t sequence,int64_t capturedMonotonicUS){
    initializeState();[publicationLock lock];
    if(enabled&&[pool retainPixel:pixel]){CVPixelBufferRef old=latest;latest=pixel;sourceGeneration=generation;sourceSequence=sequence;sourceCapturedMono=capturedMonotonicUS;if(old)[pool releasePixel:old];}
    [publicationLock unlock];
}
void VeynodeDisplayInvalidate(void){
    [nativeControl suspend];
    initializeState();[publicationLock lock];if(latest)[pool releasePixel:latest];latest=NULL;sourceGeneration=sourceSequence=0;sourceCapturedMono=0;[displayed invalidate];[publicationLock unlock];
}
BOOL VeynodeDisplayVisible(void){initializeState();[publicationLock lock];BOOL result=enabled&&visible;[publicationLock unlock];return result;}
static NSDictionary *nativeInputStatus(void){
    return nativeControl?[nativeControl status]:@{@"native_input_protocol":@1,@"native_input_supported":@YES,@"native_input_available":@NO,@"input_enabled":@NO,@"display_only":@YES,@"native_input_state":@"view_only",@"native_input_ready":@NO,@"native_human_hold":@NO,@"native_release_confirmed":@YES,@"native_input_epoch":[NSNull null],@"native_input_queued_transitions":@0,@"native_input_pending_move":@NO,@"native_input_transition_limit":@32};
}
NSDictionary *VeynodeDisplayCapabilities(void){
    NSMutableDictionary *caps=[@{@"native_window":(MTLCreateSystemDefaultDevice()!=nil?@YES:@NO),@"display_instance":[displayed instance],@"native_window_protocol":@2,@"displayed_frame_protocol":@1,@"display_only":@YES,@"input_enabled":@NO,@"display_path":@"owned-BGRA-MetalKit-no-video-encoding",@"capture_limit_fps":@60,@"native_window_capture_limit_fps":@60,@"background_capture_limit_fps":@30,@"encoder_limit_fps":@30,@"idle_encode_interval_ms":@1000,@"display_refresh_preference_hz":@60,@"physical_presentation_measurement":@"unavailable",@"presented_time_sample_limit":@120,@"presented_time_source":@"CAMetalDrawable.addPresentedHandler/presentedTime",@"owned_slot_limit":@3,@"display_gpu_inflight_limit":@1} mutableCopy];
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
@property NSButton *takeButton,*returnButton;
@property NSTextField *inputLabel;
@property NSTimer *inputTimer;
@property NSMutableSet<NSString *> *heldKeys;
@property NSMutableSet<NSString *> *heldButtons;
@property BOOL inputWasReady;
- (void)inputEvent:(NSEvent *)event;
- (void)updateInput;
- (void)suspendInput;
- (void)takeControl:(id)sender;
- (void)returnAgent:(id)sender;
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
    self.view=[[VNReadOnlyView alloc] initWithFrame:NSMakeRect(0,0,960,600) device:self.device];self.view.delegate=self;
    self.view.colorPixelFormat=MTLPixelFormatBGRA8Unorm;self.view.clearColor=MTLClearColorMake(0.03,0.03,0.03,1);self.view.framebufferOnly=YES;self.view.preferredFramesPerSecond=60;self.view.enableSetNeedsDisplay=NO;self.view.paused=NO;
    __weak VNDisplay *weak=self;self.view.inputEvent=^(NSEvent *event){[weak inputEvent:event];};
    NSView *container=[[NSView alloc] initWithFrame:NSMakeRect(0,0,960,640)];
    self.view.autoresizingMask=NSViewWidthSizable|NSViewHeightSizable;[container addSubview:self.view];
    self.takeButton=[NSButton buttonWithTitle:@"Take Control" target:self action:@selector(takeControl:)];self.takeButton.frame=NSMakeRect(10,605,125,28);self.takeButton.autoresizingMask=NSViewMinYMargin;self.takeButton.accessibilityLabel=@"Take Control";[container addSubview:self.takeButton];
    self.returnButton=[NSButton buttonWithTitle:@"Return to Agent" target:self action:@selector(returnAgent:)];self.returnButton.frame=NSMakeRect(145,605,145,28);self.returnButton.autoresizingMask=NSViewMinYMargin;self.returnButton.accessibilityLabel=@"Return to Agent";[container addSubview:self.returnButton];
    self.inputLabel=[NSTextField labelWithString:@"View Only"];self.inputLabel.frame=NSMakeRect(305,610,640,20);self.inputLabel.autoresizingMask=NSViewWidthSizable|NSViewMinYMargin;self.inputLabel.accessibilityLabel=@"Native input state";[container addSubview:self.inputLabel];
    self.window=[[NSWindow alloc] initWithContentRect:container.frame styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskClosable|NSWindowStyleMaskResizable|NSWindowStyleMaskMiniaturizable backing:NSBackingStoreBuffered defer:NO];
    self.layoutObserved=NO;self.windowEpoch++;self.lastGeneration=UINT64_MAX;self.lastSequence=UINT64_MAX;self.lastDrawableSize=CGSizeZero;
    self.window.title=@"Veynode VM Console — View Only";self.window.releasedWhenClosed=NO;self.window.delegate=self;self.window.contentView=container;self.window.acceptsMouseMovedEvents=YES;
    self.inputTimer=[NSTimer timerWithTimeInterval:0.05 repeats:YES block:^(NSTimer *timer){[weak updateInput];}];[[NSRunLoop mainRunLoop] addTimer:self.inputTimer forMode:NSRunLoopCommonModes];
    [publicationLock lock];enabled=YES;[publicationLock unlock];
    [self.window center];[self.window makeKeyAndOrderFront:nil];[NSApp activateIgnoringOtherApps:YES];[self updateVisibility];return nil;
}
- (void)close {
    [self suspendInput];[self.inputTimer invalidate];self.inputTimer=nil;
    NSAssert([NSThread isMainThread],@"AppKit thread");
    [publicationLock lock];enabled=NO;visible=NO;if(latest)[pool releasePixel:latest];latest=NULL;sourceGeneration=sourceSequence=0;sourceCapturedMono=0;[displayed invalidate];[publicationLock unlock];
    self.windowEpoch++;self.view.paused=YES;self.view.delegate=nil;self.window.delegate=nil;[self.window close];self.window=nil;self.view=nil;
    // A previously submitted GPU command retains its own frame lease until its
    // completion handler; closing a window never overwrites/recycles that slot.
}
- (BOOL)windowShouldClose:(NSWindow *)window {[self close];return NO;}
- (void)invalidateLayout {
    [self suspendInput];
    [publicationLock lock];[displayed invalidate];[publicationLock unlock];
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
    [publicationLock lock];BOOL changed=visible!=(enabled&&current);visible=enabled&&current;if(changed)[displayed invalidate];[publicationLock unlock];if(changed){[self suspendInput];self.lastGeneration=UINT64_MAX;self.lastSequence=UINT64_MAX;}
}
- (void)windowDidChangeOcclusionState:(NSNotification *)notification{[self updateVisibility];}
- (void)windowDidMiniaturize:(NSNotification *)notification{[self updateVisibility];}
- (void)windowDidDeminiaturize:(NSNotification *)notification{[self updateVisibility];}
- (void)suspendInput{[nativeControl suspend];[self.heldKeys removeAllObjects];[self.heldButtons removeAllObjects];self.inputWasReady=NO;}
- (void)windowDidResignKey:(NSNotification *)notification{[self suspendInput];}
- (void)applicationDidResignActive:(NSNotification *)notification{[self suspendInput];}
- (void)takeControl:(id)sender{
    if(!VNInputMayTakeControl(NSApp.active,self.window.keyWindow))return;
    // Keys physically held before acquisition are not owned synthetic presses.
    if(NSEvent.modifierFlags&(NSEventModifierFlagShift|NSEventModifierFlagControl|NSEventModifierFlagOption|NSEventModifierFlagCommand)){self.inputLabel.stringValue=@"Release modifier keys before Take Control";return;}
    [nativeControl takeControl];[self updateInput];
}
- (void)returnAgent:(id)sender{[self.heldKeys removeAllObjects];[self.heldButtons removeAllObjects];self.inputWasReady=NO;[nativeControl returnAgent];[self updateInput];}
- (void)updateInput{
    NSDictionary *s=nativeInputStatus();BOOL ready=[s[@"input_enabled"] boolValue];
    if(ready&&!VNInputMayTakeControl(NSApp.active,self.window.keyWindow)){[self suspendInput];s=nativeInputStatus();ready=NO;}
    if(ready&&!self.inputWasReady)[self.window makeFirstResponder:self.view];
    if(ready&&!VNInputMayDispatch(NSApp.active,self.window.keyWindow,self.view&&self.window.firstResponder==self.view)){[self suspendInput];s=nativeInputStatus();ready=NO;}
    if(!ready){[self.heldKeys removeAllObjects];[self.heldButtons removeAllObjects];}
    self.inputWasReady=ready;
    NSString *state=s[@"native_input_state"]?:@"view_only";
    NSDictionary *labels=@{@"view_only":@"View Only",@"connecting":@"Connecting — View Only",@"human_control":@"Human Control",@"taking_control":@"Taking Control",@"suspending":@"Releasing Inputs — Human Hold",@"suspended_human_hold":@"Suspended — Human Hold",@"returning_agent":@"Returning to Agent",@"cleanup_unconfirmed":@"Cleanup Unconfirmed — Input Disabled"};
    NSString *label=labels[state]?:@"Input Disabled";
    self.inputLabel.stringValue=label;
    self.takeButton.enabled=[s[@"native_input_available"] boolValue]&&!ready&&![@[@"taking_control",@"suspending",@"returning_agent"] containsObject:state];
    self.returnButton.enabled=[s[@"native_input_available"] boolValue]&&[s[@"native_human_hold"] boolValue]&&![@[@"taking_control",@"suspending",@"returning_agent"] containsObject:state];
    self.window.title=[@"Veynode VM Console — " stringByAppendingString:label];
}
- (void)inputEvent:(NSEvent *)e{
    if(!VNInputMayDispatch(NSApp.active,self.window.keyWindow,self.view&&self.window.firstResponder==self.view)){[self suspendInput];return;}
    if(![[nativeControl status][@"input_enabled"] boolValue])return;
    [self refreshLayout];[publicationLock lock];NSDictionary *frame=[displayed snapshot];[publicationLock unlock];
    if(!frame){[self suspendInput];return;}
    NSDictionary *token=VNDisplayedFrameToken(frame);NSDictionary *event=nil;
    if(e.type==NSEventTypeKeyDown||e.type==NSEventTypeKeyUp||e.type==NSEventTypeFlagsChanged){
        NSString *q=VNNativeQCode(e.keyCode);if(!q){if(e.type!=NSEventTypeKeyUp)[self suspendInput];return;}
        BOOL down=e.type==NSEventTypeKeyDown;
        if(e.type==NSEventTypeFlagsChanged&&!VNNativeModifierDown(e.keyCode,e.modifierFlags,&down))return;
        if(down&&[self.heldKeys containsObject:q])return;
        if(!down&&![self.heldKeys containsObject:q])return;
        event=@{@"kind":@"key",@"qcode":q,@"down":@(down)};
        if([nativeControl enqueueEvent:event frame:token]){if(down)[self.heldKeys addObject:q];else[self.heldKeys removeObject:q];}else[self suspendInput];return;
    }
    NSPoint point=[self.view convertPointToBacking:[self.view convertPoint:e.locationInWindow fromView:nil]];VNDisplayGeometry geometry;size_t x,y;
    if(!VNDisplayedFrameGeometry(frame,&geometry)||!VNDisplayMapBackingPoint(geometry,point.x,point.y,&x,&y)){if(self.heldButtons.count)[self suspendInput];return;}
    if(e.type==NSEventTypeMouseMoved||e.type==NSEventTypeLeftMouseDragged||e.type==NSEventTypeRightMouseDragged||e.type==NSEventTypeOtherMouseDragged)event=@{@"kind":@"move",@"x":@(x),@"y":@(y)};
    else if(e.type==NSEventTypeScrollWheel){double dx=e.scrollingDeltaX,dy=e.scrollingDeltaY;if(!isfinite(dx)||!isfinite(dy))return;double delta=fabs(dy)>=fabs(dx)?dy:dx;if(delta==0)return;NSString *direction=fabs(dy)>=fabs(dx)?(dy>0?@"up":@"down"):(dx>0?@"left":@"right");event=@{@"kind":@"scroll",@"direction":direction,@"ticks":@(MIN(20,MAX(1,(int)ceil(fabs(delta))))),@"x":@(x),@"y":@(y)};}
    else{if(e.buttonNumber>2)return;NSString *button=@[@"left",@"right",@"middle"][e.buttonNumber];BOOL down=e.type==NSEventTypeLeftMouseDown||e.type==NSEventTypeRightMouseDown||e.type==NSEventTypeOtherMouseDown;if(down==[self.heldButtons containsObject:button])return;event=@{@"kind":@"button",@"button":button,@"down":@(down),@"x":@(x),@"y":@(y)};if([nativeControl enqueueEvent:event frame:token]){if(down)[self.heldButtons addObject:button];else[self.heldButtons removeObject:button];}else[self suspendInput];return;}
    if(event&&![nativeControl enqueueEvent:event frame:token])[self suspendInput];
}
- (NSDictionary *)status {
    [self updateVisibility];[self refreshLayout];[self updateInput];
    NSMutableDictionary *value=[VeynodeDisplayCapabilities() mutableCopy];value[@"window_open"]=(self.window!=nil?@YES:@NO);value[@"window_visible"]=(self.window.visible?@YES:@NO);value[@"capture_current_limit_fps"]=@(VeynodeDisplayVisible()?60:30);value[@"capture_native_visible"]=@(VeynodeDisplayVisible());value[@"physical_presentations"]=[NSNull null];value[@"display_gpu_inflight"]=@(self.inflight);value[@"drawables_submitted"]=@(self.submitted);value[@"gpu_commands_completed"]=@(self.completed);value[@"gpu_commands_failed"]=@(self.failed);value[@"pool"]=[pool statistics];
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
    value[@"frame_available"]=(latest!=NULL?@YES:@NO);value[@"source_generation"]=@(sourceGeneration);value[@"source_sequence"]=@(sourceSequence);NSDictionary *shown=[displayed snapshot];value[@"displayed_frame_available"]=shown?@YES:@NO;value[@"displayed_frame"]=shown?:[NSNull null];value[@"displayed_frame_authorizes_input"]=@NO;value[@"displayed_frame_age_ms"]=shown?@(MAX(0,NSProcessInfo.processInfo.systemUptime-[shown[@"callback_received_monotonic_seconds"] doubleValue])*1000):[NSNull null];[publicationLock unlock];[value addEntriesFromDictionary:[self.presented statistics]];return value;
}
- (void)mtkView:(MTKView *)view drawableSizeWillChange:(CGSize)size{[self invalidateLayout];}
- (void)drawInMTKView:(MTKView *)view {
    [self updateVisibility];[self refreshLayout];if(self.inflight||!self.window||!VeynodeDisplayVisible())return;
    [publicationLock lock];CVPixelBufferRef frame=latest;uint64_t generation=sourceGeneration,sequence=sourceSequence;int64_t captured=sourceCapturedMono;uint64_t submissionRevision=displayed.revision;
    if(frame&&![pool retainPixel:frame])frame=NULL;[publicationLock unlock];
    if(generation==self.lastGeneration&&sequence==self.lastSequence&&CGSizeEqualToSize(view.drawableSize,self.lastDrawableSize)){if(frame)[pool releasePixel:frame];return;}
    uint64_t displayEpoch=self.windowEpoch;CGSize drawableSize=view.drawableSize;
    id<CAMetalDrawable> drawable=view.currentDrawable;MTLRenderPassDescriptor *pass=view.currentRenderPassDescriptor;
    if(!drawable||!pass){if(frame)[pool releasePixel:frame];return;}
    // Freeze actual target pixels after drawable acquisition, not a possibly
    // changing view size. Render viewport and observation use this same value.
    drawableSize=CGSizeMake(drawable.texture.width,drawable.texture.height);
    id<MTLCommandBuffer> command=[self.commands commandBuffer];id<MTLRenderCommandEncoder> render=[command renderCommandEncoderWithDescriptor:pass];
    if(!command||!render){if(frame)[pool releasePixel:frame];self.failed++;return;}
    VNDisplayGeometry geometry={0};
    if(frame){
        NSUInteger w=CVPixelBufferGetWidth(frame),h=CVPixelBufferGetHeight(frame);
        MTLTextureDescriptor *desc=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:w height:h mipmapped:NO];desc.storageMode=MTLStorageModeShared;desc.usage=MTLTextureUsageShaderRead;
        id<MTLTexture> texture=[self.device newTextureWithDescriptor:desc iosurface:CVPixelBufferGetIOSurface(frame) plane:0];
        if(!texture){[render endEncoding];[pool releasePixel:frame];self.failed++;return;}
        double scale=fmin(drawableSize.width/w,drawableSize.height/h);double dw=w*scale,dh=h*scale;
        NSRect vb=view.bounds,bb=[view convertRectToBacking:view.bounds];
        geometry=(VNDisplayGeometry){.sourceWidth=w,.sourceHeight=h,.drawableWidth=drawableSize.width,.drawableHeight=drawableSize.height,.viewBounds={vb.origin.x,vb.origin.y,vb.size.width,vb.size.height},.backingBounds={bb.origin.x,bb.origin.y,bb.size.width,bb.size.height},.viewport={(drawableSize.width-dw)/2,(drawableSize.height-dh)/2,dw,dh},.viewFlipped=view.flipped};
        [render setViewport:(MTLViewport){(drawableSize.width-dw)/2,(drawableSize.height-dh)/2,dw,dh,0,1}];
        [render setRenderPipelineState:self.pipeline];[render setFragmentTexture:texture atIndex:0];[render drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    }
    [render endEncoding];self.inflight=YES;self.submitted++;
    [command addCompletedHandler:^(id<MTLCommandBuffer> completed){
        if(frame)[pool releasePixel:frame];
        dispatch_async(dispatch_get_main_queue(),^{self.inflight=NO;if(completed.status==MTLCommandBufferStatusCompleted){self.completed++;[publicationLock lock];if(self.windowEpoch==displayEpoch&&displayed.revision==submissionRevision){self.lastGeneration=generation;self.lastSequence=sequence;self.lastDrawableSize=drawableSize;}[publicationLock unlock];}else{self.failed++;}});
    }];
    // Do not newly submit a frame whose generation retired while building its
    // command. An already submitted GPU command cannot be retroactively erased;
    // next tick clears it, and its slot remains pinned through completion.
    [publicationLock lock];
    BOOL valid=!frame||(enabled&&sourceGeneration==generation);
    if(valid){
        NSDictionary *candidate=frame?[displayed candidateGeometry:geometry generation:generation sequence:sequence windowEpoch:displayEpoch capturedMonotonicUS:captured]:nil;
        [drawable addPresentedHandler:^(id<MTLDrawable> shown){
            double time=shown.presentedTime,received=NSProcessInfo.processInfo.systemUptime;
            dispatch_async(dispatch_get_main_queue(),^{
                [self.presented recordTime:time generation:generation sequence:sequence windowEpoch:displayEpoch];
                [self updateVisibility];[self refreshLayout];
                [publicationLock lock];
                if(enabled&&visible&&self.windowEpoch==displayEpoch&&sourceGeneration==generation)[displayed commitCandidate:candidate presentedTime:time receivedMonotonic:received];
                [publicationLock unlock];
            });
        }];
        [command presentDrawable:drawable];[command commit];
    }
    [publicationLock unlock];
    if(!valid){self.inflight=NO;self.submitted--;if(frame)[pool releasePixel:frame];}

}
@end
static VNDisplay *display;
void VeynodeDisplayCommand(NSString *method,NSDictionary *frame,void (^completion)(NSDictionary *,NSString *)){
    NSTimeInterval issued=NSProcessInfo.processInfo.systemUptime;
    dispatch_async(dispatch_get_main_queue(),^{@autoreleasepool{
        if(NSProcessInfo.processInfo.systemUptime-issued>1.0){completion(nil,@"native_window_request_expired");return;}
        NSString *error=nil;
        if([method isEqual:@"validate_displayed_frame"]){[display updateVisibility];[display refreshLayout];[publicationLock lock];NSDictionary *r=[displayed validateToken:frame now:NSProcessInfo.processInfo.systemUptime];[publicationLock unlock];completion(r,r?nil:@"displayed_frame_unavailable_or_expired");return;}
        if([method isEqual:@"show_window"])error=[display show];
        else if([method isEqual:@"close_window"])[display close];
        if(error)completion(nil,error);else completion([display status],nil);
    }});
}
int VeynodeDisplayHeadlessSelfTest(void){@autoreleasepool{
    initializeState();NSCAssert(NSApp==nil,@"test must not initialize NSApplication or open a window");
    VNDisplay *headless=[VNDisplay new];
    for(int phase=0;phase<3;phase++){
        if(phase==1)nativeControl=[[VNNativeControl alloc] initWithSocket:@"/nonexistent/control.sock" vm:@"headless-test" worker:[@"a" stringByPaddingToLength:64 withString:@"a" startingAtIndex:0] display:[displayed instance]];
        if(phase==2)nativeControl=nil;
        for(int cycle=0;cycle<4;cycle++){
            NSDictionary *s=[headless status];NSData *wire=[NSJSONSerialization dataWithJSONObject:s options:0 error:nil];NSCAssert(wire,@"real status is serializable");
            NSDictionary *value=[NSJSONSerialization JSONObjectWithData:wire options:0 error:nil];
            for(NSString *key in @[@"window_open",@"window_visible",@"native_input_ready",@"native_input_available",@"native_human_hold",@"input_enabled",@"frame_available",@"displayed_frame_available"]){NSCAssert(CFGetTypeID((__bridge CFTypeRef)value[key])==CFBooleanGetTypeID()&&![value[key] boolValue],@"headless runtime bool must remain false: %@",key);}
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
    for(int i=0;i<600;i++){int64_t t=(int64_t)i*1000000/60;frames+=VNFramePacingAdmit(&pacing,t,YES);NSCAssert(!VNFramePacingAdmit(&pacing,t,YES),@"same timestamp cannot catch up");}
    NSCAssert(frames==600,@"rational 60 Hz admission has no integer drift");
    frames=0;for(int i=0;i<600;i++)frames+=VNFramePacingAdmit(&pacing,10000000+(int64_t)i*1000000/60,NO);
    NSCAssert(frames==300,@"background 30 Hz admits half a 60 Hz source");
    NSCAssert(VNFramePacingAdmit(&pacing,30000000,NO)&&!VNFramePacingAdmit(&pacing,30000000,NO),@"late event cannot create catch-up burst");
    NSCAssert(VNFramePacingAdmit(&pacing,30000001,YES),@"visible policy transition starts fresh cadence");
    CVPixelBufferRef a=VeynodeDisplayAcquire(128,80),b=VeynodeDisplayAcquire(128,80),c=VeynodeDisplayAcquire(128,80);NSCAssert(a&&b&&c&&a!=b&&b!=c&&a!=c,@"three owned slots");NSCAssert(!VeynodeDisplayAcquire(128,80),@"bounded pool");
    [publicationLock lock];enabled=YES;[publicationLock unlock];
    VeynodeDisplayPublish(a,1,1,1000);NSCAssert([pool retainPixel:a],@"GPU lease");VeynodeDisplayRelease(a);VeynodeDisplayInvalidate();
    NSCAssert(!VeynodeDisplayAcquire(128,80),@"invalidated in-flight cannot be reused");[pool releasePixel:a];
    CVPixelBufferRef replacement=VeynodeDisplayAcquire(256,160);NSCAssert(replacement&&CVPixelBufferGetWidth(replacement)==256,@"retired free slot can resize");
    VeynodeDisplayPublish(b,2,1,1000);VeynodeDisplayPublish(c,2,2,1000);VeynodeDisplayRelease(b);VeynodeDisplayRelease(c);
    [publicationLock lock];enabled=NO;[publicationLock unlock];VeynodeDisplayInvalidate();VeynodeDisplayRelease(replacement);
    NSCAssert([[pool statistics][@"leases"] unsignedIntegerValue]==0,@"close/invalidation releases all except actual GPU leases");
    CVPixelBufferRef d=VeynodeDisplayAcquire(128,80);VeynodeDisplayPublish(d,3,1,1000);VeynodeDisplayRelease(d);NSCAssert(!latest,@"closed display cannot republish");
    puts("PASS HEVC/H264 landscape/portrait/small/max geometry and invalid boundaries; bounded presentation metadata policy (synthetic inputs only), rational 60/30 Hz pacing, policy transition and no catch-up burst; actual CVPixelBuffer bounded slot leases, replacement, in-flight pin, generation invalidation and closed publication. No AppKit window, SPICE connection or GPU command.");return 0;
}}
int main(int argc,char **argv){@autoreleasepool{
    initializeState();
    if(argc==2&&!strcmp(argv[1],"--display-headless-self-test"))return VeynodeDisplayHeadlessSelfTest();
    if(argc==2&&!strcmp(argv[1],"--display-self-test"))return VeynodeDisplaySelfTest();
    if(argc==2&&!strcmp(argv[1],"--display-metal-self-test")){VNDisplay *check=[VNDisplay new];if([check prepareMetal])return 1;puts("PASS actual Metal device, command queue, fixed shader compilation, function lookup and render pipeline creation; no window or GPU command submission.");return 0;}
    if(argc<3||strcmp(argv[2],"--persistent"))return VeynodeConsoleMain(argc,argv);
    if(argc!=3&&argc!=9){fprintf(stderr,"Invalid native control configuration\n");return 2;}
    if(argc==9){if(strcmp(argv[3],"--control-socket")||strcmp(argv[5],"--vm-id")||strcmp(argv[7],"--worker-instance")){fprintf(stderr,"Invalid native control configuration\n");return 2;}nativeControl=[[VNNativeControl alloc] initWithSocket:@(argv[4]) vm:@(argv[6]) worker:@(argv[8]) display:[displayed instance]];[nativeControl start];}
    [NSApplication sharedApplication];[NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];display=[VNDisplay new];NSApp.delegate=display;
    __block int code=1;
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED,0),^{code=VeynodeConsoleMain(3,argv);dispatch_async(dispatch_get_main_queue(),^{[display close];[NSApp stop:nil];[NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0 context:nil subtype:0 data1:0 data2:0] atStart:NO];});});
    [NSApp run];return code;
}}
