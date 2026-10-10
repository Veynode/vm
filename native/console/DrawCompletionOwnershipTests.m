// Headless regression test for draw completion ownership (fix in 74dfa73).
//
// Defect: drawInMTKView registered the completed handler that releases the frame
// lease before the retired-generation check. An invalid draw released its lease on
// the !valid path; Metal then ran the handler when the never-committed command buffer
// was deallocated, released the same lease again, and VNFramePool raised
// "unbalanced owned-frame lease".
//
// Seam: drawInMTKView releases publicationLock before it reads view.currentDrawable
// and checks validity under the lock afterwards. The fake view publishes a new
// generation from currentDrawable, so the draw is invalid deterministically.
//
// Production state: Display.m sets its file-static enabled flag only in show(), which
// opens a window. This file therefore includes the production Display.m (its main()
// is renamed and never runs) and sets enabled as VeynodeDisplaySelfTest does. visible
// then follows from updateVisibility() with the stand-in window below. No NSWindow and
// no NSApplication are created: self.window reports a visible, unminimized, unoccluded
// window, and the view never joins a window.
// GPU work is offscreen only: the valid branch presents a CAMetalLayer drawable.

#import <AppKit/AppKit.h>
#import <MetalKit/MetalKit.h>
#import <QuartzCore/CAMetalLayer.h>
#include <stdio.h>
#include <stdlib.h>

#define main veynode_display_production_main
#include "Display.m"
#undef main

// Display.m's renamed main() still references VeynodeConsoleMain, which spice-console.m
// defines. That file needs GLib and SPICE and is not linked here. The stub is never called.
int VeynodeConsoleMain(int argc, char **argv){(void)argc;(void)argv;return 2;}

#define DC_WIDTH 64
#define DC_HEIGHT 32
#define DC_PAIRS 100

static void dcFail(int line,NSString *message){
    fprintf(stderr,"FAIL DrawCompletionOwnershipTests.m:%d: %s\n",line,message.UTF8String);
    exit(1);
}
#define DC_REQUIRE(condition,...) do{if(!(condition))dcFail(__LINE__,[NSString stringWithFormat:__VA_ARGS__]);}while(0)

static uint64_t dcGeneration,dcSequence;
static NSUInteger dcFakePresentations,dcFakePresentedHandlers;

static NSUInteger dcPoolLeases(void){return [[pool statistics][@"leases"] unsignedIntegerValue];}
static NSUInteger dcBranch(NSString *name){return [[drawTiming statistics][@"branches"][name] unsignedIntegerValue];}
static void dcSpinUntilIdle(VNDisplay *dcDisplay){
    NSDate *deadline=[NSDate dateWithTimeIntervalSinceNow:5];
    while(dcDisplay.inflight&&[deadline timeIntervalSinceNow]>0)[[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.002]];
}

// Stand-in for the NSWindow that the draw path queries. No window is created.
@interface VNDrawCompletionWindow : NSObject
@end
@implementation VNDrawCompletionWindow
- (BOOL)isVisible{return YES;}
- (BOOL)isMiniaturized{return NO;}
- (NSWindowOcclusionState)occlusionState{return NSWindowOcclusionStateVisible;}
@end

// Fake CAMetalDrawable for the invalid branch, which never presents it. Every
// presentation entry point is counted so the test can prove none was reached.
@interface VNDrawCompletionDrawable : NSObject <CAMetalDrawable>
- (instancetype)initWithTexture:(id<MTLTexture>)texture;
@end
@implementation VNDrawCompletionDrawable {
    id<MTLTexture> _texture;
}
- (instancetype)initWithTexture:(id<MTLTexture>)texture{if((self=[super init]))_texture=texture;return self;}
- (id<MTLTexture>)texture{return _texture;}
- (CAMetalLayer *)layer{return nil;}
- (void)present{dcFakePresentations++;}
- (void)presentAtTime:(CFTimeInterval)presentationTime{(void)presentationTime;dcFakePresentations++;}
- (void)presentAfterMinimumDuration:(CFTimeInterval)duration{(void)duration;dcFakePresentations++;}
- (void)addPresentedHandler:(MTLDrawablePresentedHandler)block{(void)block;dcFakePresentedHandlers++;}
- (CFTimeInterval)presentedTime{return 0;}
- (NSUInteger)drawableID{return 0;}
@end

// Fake MTKView. drawInMTKView reads only these members of its view argument, and the
// view is passed as MTKView *. A real MTKView is an NSView, and creating one initializes
// NSApplication as a side effect, which the headless self-tests forbid (they assert
// NSApp == nil). Its currentDrawable is the seam: drawInMTKView reads it after releasing
// publicationLock and before the valid check, so a publish there retires the drawn
// frame's generation in the middle of the draw.
@interface VNDrawCompletionView : NSObject
@property(strong) id<CAMetalDrawable> drawable;
@property(strong) MTLRenderPassDescriptor *renderPass;
@property CVPixelBufferRef retiringPixel;
@property uint64_t retiringGeneration,retiringSequence;
@end
@implementation VNDrawCompletionView
- (id<CAMetalDrawable>)currentDrawable{
    if(self.retiringPixel){
        VeynodeDisplayPublish(self.retiringPixel,self.retiringGeneration,self.retiringSequence,(int64_t)self.retiringSequence*1000);
        self.retiringPixel=NULL;
    }
    return self.drawable;
}
- (MTLRenderPassDescriptor *)currentRenderPassDescriptor{return self.renderPass;}
- (CGSize)drawableSize{return CGSizeMake(DC_WIDTH,DC_HEIGHT);}
- (NSRect)bounds{return NSMakeRect(0,0,DC_WIDTH,DC_HEIGHT);}
- (NSRect)convertRectToBacking:(NSRect)rect{return rect;}
- (BOOL)isFlipped{return NO;}
@end

// Acquires a real pool slot, publishes it as latest and drops the acquisition lease,
// so the publication alone holds the frame.
static CVPixelBufferRef dcPublishFrame(void){
    CVPixelBufferRef pixel=VeynodeDisplayAcquire(DC_WIDTH,DC_HEIGHT);
    DC_REQUIRE(pixel,@"owned pool returned no slot");
    uint64_t generation=++dcGeneration,sequence=++dcSequence;
    VeynodeDisplayPublish(pixel,generation,sequence,(int64_t)sequence*1000);
    VeynodeDisplayRelease(pixel);
    return pixel;
}
static id<MTLTexture> dcTargetTexture(id<MTLDevice> device){
    MTLTextureDescriptor *descriptor=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:DC_WIDTH height:DC_HEIGHT mipmapped:NO];
    descriptor.usage=MTLTextureUsageRenderTarget;descriptor.storageMode=MTLStorageModePrivate;
    id<MTLTexture> texture=[device newTextureWithDescriptor:descriptor];
    DC_REQUIRE(texture,@"Metal returned no render target");
    return texture;
}
static MTLRenderPassDescriptor *dcRenderPass(id<MTLTexture> target){
    MTLRenderPassDescriptor *pass=[MTLRenderPassDescriptor renderPassDescriptor];
    pass.colorAttachments[0].texture=target;pass.colorAttachments[0].loadAction=MTLLoadActionClear;pass.colorAttachments[0].storeAction=MTLStoreActionStore;
    return pass;
}

// Invalid branch: the draw retires its own generation through the seam, so it must
// release the frame it retained exactly once, count one retired_generation and never
// commit, present or register a presented handler.
static void dcForcedInvalidDraw(VNDisplay *dcDisplay,VNDrawCompletionView *view){
    CVPixelBufferRef drawn=dcPublishFrame();
    CVPixelBufferRef retiring=VeynodeDisplayAcquire(DC_WIDTH,DC_HEIGHT);
    DC_REQUIRE(retiring&&retiring!=drawn,@"no second slot for the retiring generation");
    DC_REQUIRE(dcPoolLeases()==2,@"before the draw: leases=%lu, expected 2",(unsigned long)dcPoolLeases());
    id<MTLTexture> target=dcTargetTexture(dcDisplay.device);
    view.drawable=[[VNDrawCompletionDrawable alloc] initWithTexture:target];
    view.renderPass=dcRenderPass(target);
    view.retiringPixel=retiring;
    view.retiringGeneration=++dcGeneration;
    view.retiringSequence=++dcSequence;
    NSUInteger submitted=dcDisplay.submitted,retired=dcBranch(@"retired_generation"),committed=dcBranch(@"submitted_frame");
    NSUInteger presented=dcFakePresentations,handlers=dcFakePresentedHandlers;
    @try{@autoreleasepool{[dcDisplay drawInMTKView:(MTKView *)view];}}
    @catch(NSException *e){dcFail(__LINE__,[NSString stringWithFormat:@"forced invalid draw raised %@: %@",e.name,e.reason]);}
    DC_REQUIRE(view.retiringPixel==NULL,@"seam did not run: currentDrawable was not reached");
    DC_REQUIRE(!dcDisplay.inflight&&dcDisplay.submitted==submitted,@"invalid draw must leave inflight clear and submitted unchanged");
    DC_REQUIRE(dcBranch(@"retired_generation")==retired+1,@"invalid draw must count exactly one retired_generation branch");
    DC_REQUIRE(dcBranch(@"submitted_frame")==committed,@"invalid draw must not count as a submission");
    DC_REQUIRE(dcFakePresentations==presented&&dcFakePresentedHandlers==handlers,@"invalid draw must not present or register a presented handler");
    BOOL stillLeased=[pool retainPixel:drawn];
    if(stillLeased)[pool releasePixel:drawn];
    DC_REQUIRE(!stillLeased,@"drawn frame kept a lease after an invalid draw");
    VeynodeDisplayRelease(retiring);
    DC_REQUIRE(dcPoolLeases()==1,@"only the retiring publication may hold a lease: leases=%lu",(unsigned long)dcPoolLeases());
    VeynodeDisplayInvalidate();
    DC_REQUIRE(dcPoolLeases()==0,@"leases=%lu after invalidation, expected 0",(unsigned long)dcPoolLeases());
}

// Valid branch: a real CAMetalLayer drawable is presented offscreen, the command
// completes on the GPU and the main-queue completion clears inflight. The completion
// must release the draw's lease, leaving only the latest publication.
static void dcValidDraw(VNDisplay *dcDisplay,VNDrawCompletionView *view){
    dcPublishFrame();
    CAMetalLayer *layer=[CAMetalLayer layer];
    layer.device=dcDisplay.device;layer.pixelFormat=MTLPixelFormatBGRA8Unorm;layer.framebufferOnly=YES;layer.drawableSize=CGSizeMake(DC_WIDTH,DC_HEIGHT);
    id<CAMetalDrawable> drawable=[layer nextDrawable];
    DC_REQUIRE(drawable,@"CAMetalLayer returned no drawable without a window");
    view.drawable=drawable;view.renderPass=dcRenderPass(drawable.texture);view.retiringPixel=NULL;
    DC_REQUIRE(!dcDisplay.inflight,@"valid draw started while a command was in flight");
    NSUInteger submitted=dcDisplay.submitted,completed=dcDisplay.completed,failed=dcDisplay.failed,committed=dcBranch(@"submitted_frame"),retired=dcBranch(@"retired_generation");
    @try{@autoreleasepool{[dcDisplay drawInMTKView:(MTKView *)view];}}
    @catch(NSException *e){dcFail(__LINE__,[NSString stringWithFormat:@"valid draw raised %@: %@",e.name,e.reason]);}
    DC_REQUIRE(dcDisplay.inflight,@"valid draw was not submitted");
    dcSpinUntilIdle(dcDisplay);
    DC_REQUIRE(!dcDisplay.inflight,@"valid command did not complete on the main queue");
    DC_REQUIRE(dcDisplay.completed==completed+1&&dcDisplay.failed==failed,@"valid command: completed=%lu failed=%lu",(unsigned long)dcDisplay.completed,(unsigned long)dcDisplay.failed);
    DC_REQUIRE(dcDisplay.submitted==submitted+1&&dcBranch(@"submitted_frame")==committed+1&&dcBranch(@"retired_generation")==retired,@"valid draw must count one submission and no retirement");
    DC_REQUIRE(dcPoolLeases()==1,@"completion must release the draw lease: leases=%lu",(unsigned long)dcPoolLeases());
    VeynodeDisplayInvalidate();
    DC_REQUIRE(dcPoolLeases()==0,@"leases=%lu after invalidation, expected 0",(unsigned long)dcPoolLeases());
}

int main(void){@autoreleasepool{
    initializeState();
    DC_REQUIRE(NSApp==nil,@"test must not initialize NSApplication");
    frameAcquisition=[VNFrameAcquisition new];
    drawTiming=[[VNDrawTiming alloc] initWithEnabled:YES];
    VNDisplay *dcDisplay=[VNDisplay new];
    NSString *failure=[dcDisplay prepareMetal];
    DC_REQUIRE(!failure,@"prepareMetal failed: %@",failure);
    dcDisplay.window=(NSWindow *)[VNDrawCompletionWindow new];
    [publicationLock lock];enabled=YES;[publicationLock unlock];
    [dcDisplay updateVisibility];
    DC_REQUIRE(VeynodeDisplayVisible(),@"stand-in window did not make the display visible");
    VNDrawCompletionView *view=[VNDrawCompletionView new];
    DC_REQUIRE(NSApp==nil&&dcDisplay.view==nil,@"no application or display view may exist");
    DC_REQUIRE(dcPoolLeases()==0,@"pool must start empty");
    dcForcedInvalidDraw(dcDisplay,view);
    for(int pair=0;pair<DC_PAIRS;pair++){dcValidDraw(dcDisplay,view);dcForcedInvalidDraw(dcDisplay,view);}
    DC_REQUIRE(dcPoolLeases()==0,@"pool leases=%lu after the loop, expected 0",(unsigned long)dcPoolLeases());
    DC_REQUIRE(!dcDisplay.inflight&&dcDisplay.submitted==DC_PAIRS&&dcDisplay.completed==DC_PAIRS&&dcDisplay.failed==0,@"loop counters: submitted=%lu completed=%lu failed=%lu",(unsigned long)dcDisplay.submitted,(unsigned long)dcDisplay.completed,(unsigned long)dcDisplay.failed);
    DC_REQUIRE(dcBranch(@"retired_generation")==DC_PAIRS+1&&dcBranch(@"submitted_frame")==DC_PAIRS,@"loop branches: retired_generation=%lu submitted_frame=%lu",(unsigned long)dcBranch(@"retired_generation"),(unsigned long)dcBranch(@"submitted_frame"));
    DC_REQUIRE(NSApp==nil,@"the draw path must not initialize NSApplication");
    [publicationLock lock];enabled=NO;[publicationLock unlock];
    VeynodeDisplayInvalidate();
    printf("PASS draw completion ownership: %d draws (%d valid, %d forced-invalid); each invalid draw released its frame once, pool leases 0, no NSException; offscreen GPU only, no NSWindow or NSApplication.\n",2*DC_PAIRS+1,DC_PAIRS,DC_PAIRS+1);
    return 0;
}}
