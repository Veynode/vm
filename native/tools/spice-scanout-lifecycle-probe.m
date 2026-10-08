// Actual scanout generation/producer-disconnect diagnostic; no cached ID reuse.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <IOSurface/IOSurface.h>
#include <glib.h>
#include <spice-client.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/ioctl.h>
static GMainLoop *loop;
static id<MTLDevice> device;
static id<MTLCommandQueue> queue;
static id<MTLTexture> texture;
static NSString *mode,*prefix,*failure;
static int retainedFD=-1;
static NSUInteger generation,draws,generationDraws,invalidations,oldFrames,newFrames,totalFrames,framesAfterDisconnect,discardedDrawsAfterDisconnect,framesAtDisconnect;
static BOOL disconnected,changedSize,deathStopScheduled;
static gboolean finishDeath(gpointer data){framesAfterDisconnect=totalFrames-framesAtDisconnect;g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
static void observeDeath(void){if(!deathStopScheduled){deathStopScheduled=YES;framesAtDisconnect=totalFrames;g_timeout_add(500,finishDeath,NULL);}}
static NSMutableArray *records;
static void emit(NSDictionary *object){[records addObject:object];NSData *d=[NSJSONSerialization dataWithJSONObject:object options:NSJSONWritingSortedKeys error:nil];fwrite(d.bytes,1,d.length,stdout);fputc('\n',stdout);fflush(stdout);}
static void clearSurface(NSString *reason,BOOL hup){if(retainedFD>=0){close(retainedFD);retainedFD=-1;}texture=nil;emit(@{@"event":@"surface-released",@"reason":reason,@"generation":@(generation),@"hup":@(hup),@"frames":@(totalFrames)});}
static BOOL liveFD(void){if(retainedFD<0)return NO;struct pollfd p={.fd=retainedFD,.events=POLLIN};int result=poll(&p,1,0);if(result<0||(p.revents&(POLLHUP|POLLERR|POLLNVAL))){invalidations++;clearSurface(@"producer-generation-fd-invalid",(p.revents&POLLHUP)!=0);return NO;}return YES;}
static void scanout(SpiceDisplayChannel *channel,GParamSpec *spec,gpointer data){
 if(disconnected){emit(@{@"event":@"scanout-ignored-after-disconnect"});return;}
 const SpiceGlScanout *info=spice_display_channel_get_gl_scanout(channel);
 if(retainedFD>=0){BOOL oldLive=liveFD();if(oldLive){failure=@"Prior client FD still live at scanout replacement";clearSurface(@"replacement-without-hup",NO);g_main_loop_quit(loop);return;}}
 texture=nil;
 if(!info){emit(@{@"event":@"scanout-null"});return;}
 struct pollfd p={.fd=info->fd,.events=POLLIN};int available=0;ioctl(info->fd,FIONREAD,&available);
 if(poll(&p,1,0)!=1||!(p.revents&POLLIN)||(p.revents&POLLHUP)||available!=4){failure=@"No fresh live scanout ID";g_main_loop_quit(loop);return;}
 IOSurfaceID ident=0;if(read(info->fd,&ident,4)!=4){failure=@"Scanout ID read failed";g_main_loop_quit(loop);return;}
 IOSurfaceRef surface=IOSurfaceLookup(ident);
 if(!surface||IOSurfaceGetWidth(surface)!=info->width||IOSurfaceGetHeight(surface)!=info->height||info->width>1920||info->height>1080||IOSurfaceGetPixelFormat(surface)!='BGRA'){
  if(surface)CFRelease(surface);failure=@"Unsupported scanout dimensions or format";g_main_loop_quit(loop);return;}
 retainedFD=dup(info->fd);if(retainedFD<0){CFRelease(surface);failure=@"Cannot retain generation FD";g_main_loop_quit(loop);return;}fcntl(retainedFD,F_SETFD,FD_CLOEXEC);
 MTLTextureDescriptor *desc=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:info->width height:info->height mipmapped:NO];desc.usage=MTLTextureUsageShaderRead;
 texture=[device newTextureWithDescriptor:desc iosurface:surface plane:0];CFRelease(surface);generation++;generationDraws=0;
 if(info->width==1024&&info->height==768&&oldFrames>=2)changedSize=YES;
 emit(@{@"event":@"generation-bound",@"generation":@(generation),@"width":@(info->width),@"height":@(info->height),@"surface_id":@(ident),@"available_before_read":@(available)});
 if(!texture){failure=@"Metal surface binding failed";g_main_loop_quit(loop);}
}
static void draw(SpiceDisplayChannel *channel,guint32 x,guint32 y,guint32 w,guint32 h,gpointer data){
 draws++;generationDraws++;
 if(disconnected){discardedDrawsAfterDisconnect++;spice_display_channel_gl_draw_done(channel);return;}
 if(texture&&liveFD()&&(generationDraws==10||generationDraws==80)){@autoreleasepool{
  MTLTextureDescriptor *desc=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:texture.pixelFormat width:texture.width height:texture.height mipmapped:NO];desc.storageMode=MTLStorageModeShared;
  id<MTLTexture> owned=[device newTextureWithDescriptor:desc];id<MTLCommandBuffer> command=[queue commandBuffer];id<MTLBlitCommandEncoder> blit=[command blitCommandEncoder];
  [blit copyFromTexture:texture sourceSlice:0 sourceLevel:0 sourceOrigin:MTLOriginMake(0,0,0) sourceSize:MTLSizeMake(texture.width,texture.height,1) toTexture:owned destinationSlice:0 destinationLevel:0 destinationOrigin:MTLOriginMake(0,0,0)];[blit endEncoding];[command commit];[command waitUntilCompleted];
  if(command.status!=MTLCommandBufferStatusCompleted){failure=@"Metal frame copy failed";}else{
   NSMutableData *pixels=[NSMutableData dataWithLength:owned.width*owned.height*4];[owned getBytes:pixels.mutableBytes bytesPerRow:owned.width*4 fromRegion:MTLRegionMake2D(0,0,owned.width,owned.height) mipmapLevel:0];const uint8_t *bytes=pixels.bytes;NSUInteger nonblack=0;uint64_t hash=14695981039346656037ULL;
   for(NSUInteger i=0;i<pixels.length;i+=4){if(bytes[i]||bytes[i+1]||bytes[i+2])nonblack++;for(int j=0;j<3;j++){hash^=bytes[i+j];hash*=1099511628211ULL;}}
   if(nonblack<=1000)failure=@"Captured frame is black";else{totalFrames++;if(owned.width==1280&&owned.height==800)oldFrames++;if(owned.width==1024&&owned.height==768)newFrames++;}
   emit(@{@"event":@"frame",@"generation":@(generation),@"width":@(owned.width),@"height":@(owned.height),@"nonblack":@(nonblack),@"rgb_hash":[NSString stringWithFormat:@"%016llx",(unsigned long long)hash]});
  }
 }}
 spice_display_channel_gl_draw_done(channel);
 if(failure||([mode isEqual:@"once"]&&totalFrames>=2)||([mode isEqual:@"resize"]&&changedSize&&newFrames>=2&&invalidations>=1))g_main_loop_quit(loop);
}
static void channelEvent(SpiceChannel *channel,SpiceChannelEvent event,gpointer data){
 if(event==SPICE_CHANNEL_CLOSED||event>=SPICE_CHANNEL_ERROR_CONNECT){
  disconnected=YES;BOOL live=liveFD();clearSurface(@"channel-disconnected",!live);emit(@{@"event":@"channel-closed",@"channel_event":@(event),@"frames":@(totalFrames)});
  if([mode isEqual:@"death"])observeDeath();else{failure=@"Unexpected producer disconnect";g_main_loop_quit(loop);}
 }
}
static void channel(SpiceSession *session,SpiceChannel *ch,gpointer data){if(SPICE_IS_DISPLAY_CHANNEL(ch)){g_signal_connect(ch,"notify::gl-scanout",G_CALLBACK(scanout),NULL);g_signal_connect(ch,"gl-draw",G_CALLBACK(draw),NULL);g_signal_connect(ch,"channel-event",G_CALLBACK(channelEvent),NULL);spice_channel_connect(ch);}else if(SPICE_IS_MAIN_CHANNEL(ch)){g_signal_connect(ch,"channel-event",G_CALLBACK(channelEvent),NULL);}}
static gboolean check(gpointer data){if(retainedFD>=0&&!liveFD()&&[mode isEqual:@"death"]){disconnected=YES;emit(@{@"event":@"producer-generation-ended",@"frames":@(totalFrames)});observeDeath();}return G_SOURCE_CONTINUE;}
static gboolean timeout(gpointer data){failure=@"Lifecycle condition not reached within35seconds";g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
int main(int argc,char **argv){@autoreleasepool{
 if(argc!=4)return 2;mode=[NSString stringWithUTF8String:argv[2]];prefix=[NSString stringWithUTF8String:argv[3]];records=[NSMutableArray new];device=MTLCreateSystemDefaultDevice();queue=[device newCommandQueue];if(!device||!queue)return 3;
 loop=g_main_loop_new(NULL,FALSE);SpiceSession *session=spice_session_new();g_object_set(session,"unix-path",argv[1],NULL);g_signal_connect(session,"channel-new",G_CALLBACK(channel),NULL);
 if(!spice_session_connect(session))failure=@"SPICE connect failed";else{g_timeout_add_seconds(35,timeout,NULL);g_timeout_add(100,check,NULL);g_main_loop_run(loop);}
 BOOL passed=!failure&&([mode isEqual:@"resize"]?(oldFrames>=2&&newFrames>=2&&changedSize&&invalidations>=1):([mode isEqual:@"death"]?(totalFrames>=1&&disconnected&&texture==nil&&retainedFD<0&&framesAfterDisconnect==0):totalFrames>=2));
 // Detach callbacks before our own deliberate disconnect, so it cannot satisfy death acceptance.
 GList *channels=spice_session_get_channels(session);
 for(GList *item=channels;item;item=item->next)g_signal_handlers_disconnect_by_func(item->data,G_CALLBACK(channelEvent),NULL);
 g_list_free(channels);
 clearSurface(@"test-exit",NO);spice_session_disconnect(session);g_object_unref(session);g_main_loop_unref(loop);
 NSDictionary *report=@{@"success":@(passed),@"mode":mode,@"error":failure?:[NSNull null],@"generations":@(generation),@"frames":@(totalFrames),@"initial_size_frames":@(oldFrames),@"resized_frames":@(newFrames),@"prior_generation_invalidations":@(invalidations),@"disconnected":@(disconnected),@"frames_after_disconnect":@(framesAfterDisconnect),@"discarded_draws_after_disconnect":@(discardedDrawsAfterDisconnect),@"post_disconnect_observation_ms":@(deathStopScheduled?500:0),@"surface_released":@(texture==nil&&retainedFD<0),@"events":records};
 NSData *json=[NSJSONSerialization dataWithJSONObject:report options:NSJSONWritingPrettyPrinted|NSJSONWritingSortedKeys error:nil];[json writeToFile:[prefix stringByAppendingString:@".lifecycle.json"] atomically:YES];return passed?0:1;
}}
