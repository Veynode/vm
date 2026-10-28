// Compiled only by test-screenshot.sh. Uses the actual owner command/tick code,
// real frame-pool leases, CPU ImageIO and framed pipe; no SPICE or Metal device.
#include <sys/socket.h>
#define SHOT_CHECK(x) do{if(!(x)){fprintf(stderr,"FAIL pipeline line %d: %s\n",__LINE__,#x);return 1;}}while(0)
static int screenshotPipelineSelfTest(void){
 for(int mode=0;mode<6;mode++){
  int sink[2],live[2];SHOT_CHECK(pipe(sink)==0&&socketpair(AF_UNIX,SOCK_STREAM,0,live)==0);
  for(int i=0;i<2;i++)fcntl(sink[i],F_SETFL,fcntl(sink[i],F_GETFL)|O_NONBLOCK);
  outputWriter=[[VNRecordWriter alloc]initWithFD:sink[1]];generationFD=live[0];loop=g_main_loop_new(NULL,FALSE);generation=cachedGeneration=40;sequence=12;sourceWidth=sourceHeight=64;capturedWall=99;capturedMono=88;mainOpen=displayOpen=YES;closing=NO;fatal=nil;shutdownID=nil;shutdownReplied=NO;lastScreenshotTiming=nil;
  owned=VeynodeDisplayAcquire(64,64);SHOT_CHECK(owned);CVPixelBufferLockBaseAddress(owned,0);memset(CVPixelBufferGetBaseAddress(owned),43,CVPixelBufferGetDataSize(owned));CVPixelBufferUnlockBaseAddress(owned,0);
  dispatch_semaphore_t entered=dispatch_semaphore_create(0),resume=dispatch_semaphore_create(0);
  [VNAsyncScreenshot setTestBarrier:^{dispatch_semaphore_signal(entered);dispatch_semaphore_wait(resume,DISPATCH_TIME_FOREVER);}];
  command([@"{\"id\":\"shot\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]);SHOT_CHECK(screenshotJob);SHOT_CHECK(dispatch_semaphore_wait(entered,dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC))==0);
  // A second request is rejected while the first actual worker holds its lease.
  command([@"{\"id\":\"busy\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]);
  if(mode==0){sequence++;} // new frame sequence alone must not invalidate snapshot
  if(mode==1){generation++;}
  if(mode==2){close(live[1]);live[1]=-1;}
  if(mode==3){command([@"{\"id\":\"stop\",\"method\":\"shutdown\"}" dataUsingEncoding:NSUTF8StringEncoding]);screenshotTick(NULL);SHOT_CHECK(!shutdownReplied&&screenshotJob);}
  if(mode==4){closing=YES;[screenshotJob invalidate];} // actual control EOF teardown fence
  if(mode==5){screenshotDeadline=nativeClock()-.01;} // force a real completed-after-deadline job before owner tick
  dispatch_semaphore_signal(resume);SHOT_CHECK([screenshotJob waitFor:2]);[VNAsyncScreenshot setTestBarrier:nil];
  double until=nativeClock()+2;do{screenshotTick(NULL);usleep(1000);}while(screenshotJob&&nativeClock()<until);SHOT_CHECK(!screenshotJob);SHOT_CHECK([outputWriter drainFor:.6]);screenshotTick(NULL);SHOT_CHECK([outputWriter drainFor:.6]);
  NSMutableData *wire=[NSMutableData new];uint8_t buf[4096];ssize_t n;while((n=read(sink[0],buf,sizeof(buf)))>0)[wire appendBytes:buf length:(NSUInteger)n];
  NSUInteger offset=0;NSMutableDictionary *responses=[NSMutableDictionary new];while(offset<wire.length){SHOT_CHECK(wire.length-offset>=13);uint32_t length;memcpy(&length,(const char *)wire.bytes+offset,4);length=CFSwapInt32BigToHost(length);offset+=13;SHOT_CHECK(length&&length<=wire.length-offset);NSDictionary *r=[NSJSONSerialization JSONObjectWithData:[wire subdataWithRange:NSMakeRange(offset,length)] options:0 error:nil];SHOT_CHECK(r[@"id"]);responses[r[@"id"]]=r;offset+=length;}
  SHOT_CHECK([responses[@"busy"][@"error"][@"code"] isEqual:@"screenshot_busy"]);
  if(mode==0){SHOT_CHECK([responses[@"shot"][@"result"][@"source_sequence"] isEqual:@12]);SHOT_CHECK([lastScreenshotTiming[@"success"] boolValue]);}
  if(mode==1||mode==2){SHOT_CHECK(!responses[@"shot"][@"result"]);SHOT_CHECK([responses[@"shot"][@"error"][@"diagnostic"][@"stage"] isEqual:@"cache_invalid_after_encode"]);}
  if(mode==3){SHOT_CHECK(!responses[@"shot"]&&[responses[@"stop"][@"result"][@"state"] isEqual:@"stopped"]);SHOT_CHECK(closing&&outputWriter.pending==0);}
  if(mode==4){SHOT_CHECK(!responses[@"shot"]&&!responses[@"stop"]);}
  if(mode==5){SHOT_CHECK(!responses[@"shot"]&&[fatal isEqual:@"screenshot_worker_deadline"]);}
  clearCache();close(live[0]);if(live[1]>=0)close(live[1]);generationFD=-1;close(sink[0]);close(sink[1]);g_main_loop_unref(loop);loop=NULL;outputWriter=nil;
 }
 puts("PASS actual owner screenshot commands: single flight, new sequence, generation/connection invalidation, shutdown settlement, EOF fence");return 0;
}
// Run by test-screenshot.sh: deferred capture without a viewer. Uses the actual draw,
// command and tick code with a real IOSurface as the scanout and real Metal copies;
// no SPICE connection (the draw ACK is counted), NSWindow or encoder.
static NSDictionary *shotResponses(int fd){
 NSMutableData *wire=[NSMutableData new];uint8_t buf[4096];ssize_t n;while((n=read(fd,buf,sizeof(buf)))>0)[wire appendBytes:buf length:(NSUInteger)n];
 NSUInteger offset=0;NSMutableDictionary *responses=[NSMutableDictionary new];
 while(offset+13<=wire.length){uint32_t length;memcpy(&length,(const char *)wire.bytes+offset,4);length=CFSwapInt32BigToHost(length);offset+=13;if(length>wire.length-offset)break;NSDictionary *r=[NSJSONSerialization JSONObjectWithData:[wire subdataWithRange:NSMakeRange(offset,length)] options:0 error:nil];if(r[@"id"])responses[r[@"id"]]=r;offset+=length;}
 return responses;
}
static int lazyCaptureSelfTest(void){
 int sink[2],live[2];SHOT_CHECK(pipe(sink)==0&&socketpair(AF_UNIX,SOCK_STREAM,0,live)==0);
 for(int i=0;i<2;i++)fcntl(sink[i],F_SETFL,fcntl(sink[i],F_GETFL)|O_NONBLOCK);
 lock=[NSLock new];outputWriter=[[VNRecordWriter alloc]initWithFD:sink[1]];loop=g_main_loop_new(NULL,FALSE);
 NSDictionary *props=@{(__bridge NSString *)kIOSurfaceWidth:@64,(__bridge NSString *)kIOSurfaceHeight:@64,(__bridge NSString *)kIOSurfaceBytesPerElement:@4,(__bridge NSString *)kIOSurfacePixelFormat:@(kCVPixelFormatType_32BGRA)};
 surface=IOSurfaceCreate((__bridge CFDictionaryRef)props);SHOT_CHECK(surface);
 IOSurfaceLock(surface,0,NULL);memset(IOSurfaceGetBaseAddress(surface),0x2b,IOSurfaceGetAllocSize(surface));IOSurfaceUnlock(surface,0,NULL);
 generationFD=live[0];generation=9;sequence=100;sourceWidth=sourceHeight=64;mainOpen=displayOpen=YES;awaitingSource=NO;
 void (^fill)(uint8_t)=^(uint8_t v){IOSurfaceLock(surface,0,NULL);memset(IOSurfaceGetBaseAddress(surface),v,IOSurfaceGetAllocSize(surface));IOSurfaceUnlock(surface,0,NULL);};
 BOOL (^settle)(void)=^BOOL{double until=nativeClock()+3;do{screenshotTick(NULL);usleep(1000);}while(screenshotJob&&nativeClock()<until);return !screenshotJob;};
 command([@"{\"id\":\"before\",\"method\":\"frame_status\"}" dataUsingEncoding:NSUTF8StringEncoding]);
 for(int i=0;i<3;i++)draw(NULL,0,0,64,64,NULL);
 // No viewer: every draw ACKed, none copied, each visible in the sequence, no timer armed.
 SHOT_CHECK(testDrawACKs==3&&received==3&&deferredDraws==3&&sourceCopies==0&&!owned);
 SHOT_CHECK(sequence==103&&lazySequence&&sourceDirty&&frameReady()&&!cacheValid());
 SHOT_CHECK(!flushSource&&!screenshotSource&&!captureTimer&&flushDeadline()<0);
 command([@"{\"id\":\"status\",\"method\":\"frame_status\"}" dataUsingEncoding:NSUTF8StringEncoding]);
 // (a) A screen quiet for drawQuietUS: the screenshot copies the drawn scanout at once, under the draws' sequence.
 lastDrawMono=g_get_monotonic_time()-2*drawQuietUS;
 command([@"{\"id\":\"shot\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]);
 SHOT_CHECK(screenshotJob&&screenshotSource&&!screenshotWaiting&&!captureTimer);
 SHOT_CHECK(sourceCopies==1&&onDemandCopies==1&&backgroundSourceCopies==1&&visibleSourceCopies==0&&sequence==103&&!lazySequence&&!sourceDirty&&cacheValid());
 SHOT_CHECK(settle());screenshotTick(NULL);SHOT_CHECK(!screenshotSource);
 // Nothing drawn since: a second screenshot reuses that copy.
 command([@"{\"id\":\"again\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]);SHOT_CHECK(sourceCopies==1&&screenshotJob);SHOT_CHECK(settle());
 // (b) A Guest that is drawing: no copy now; the next draw copies before its ACK and admits the screenshot.
 fill(0x3c);draw(NULL,0,0,64,64,NULL);SHOT_CHECK(sequence==104&&sourceDirty&&sourceCopies==1);
 command([@"{\"id\":\"busy-guest\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]);
 SHOT_CHECK(screenshotWaiting&&!screenshotJob&&captureTimer&&sourceCopies==1);
 command([@"{\"id\":\"second\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]); // single flight while waiting
 fill(0x4d);draw(NULL,0,0,64,64,NULL);
 SHOT_CHECK(sourceCopies==2&&testCopiesAtACK==2&&onDemandCopies==2&&deferredDraws==4&&sequence==105&&!sourceDirty);
 SHOT_CHECK(!screenshotWaiting&&screenshotJob&&!captureTimer);SHOT_CHECK(settle());
 // (c) Waiting, and no further draw: the quiet timer copies after drawQuietUS.
 fill(0x5e);draw(NULL,0,0,64,64,NULL);gint64 asked=g_get_monotonic_time();
 command([@"{\"id\":\"quiet\",\"method\":\"screenshot\"}" dataUsingEncoding:NSUTF8StringEncoding]);SHOT_CHECK(screenshotWaiting&&captureTimer&&sourceCopies==2);
 double give=nativeClock()+1;while(screenshotWaiting&&nativeClock()<give)g_main_context_iteration(NULL,TRUE);
 gint64 waited=g_get_monotonic_time()-asked;
 SHOT_CHECK(!screenshotWaiting&&!captureTimer&&screenshotJob&&sourceCopies==3&&onDemandCopies==3&&sequence==106&&testCopiesAtACK==2);
 SHOT_CHECK(waited>=drawQuietUS-2000&&waited<drawQuietUS+40000);SHOT_CHECK(settle());
 SHOT_CHECK([outputWriter drainFor:.6]);NSDictionary *responses=shotResponses(sink[0]);
 SHOT_CHECK([responses[@"before"][@"result"][@"frame_available"] isEqual:@NO]);
 SHOT_CHECK([responses[@"status"][@"result"][@"frame_available"] isEqual:@YES]&&[responses[@"status"][@"result"][@"source_sequence"] isEqual:@103]&&[responses[@"status"][@"result"][@"width"] isEqual:@64]);
 SHOT_CHECK([responses[@"second"][@"error"][@"code"] isEqual:@"screenshot_busy"]);
 NSDictionary *expect=@{@"shot":@[@103,@0x2b],@"again":@[@103,@0x2b],@"busy-guest":@[@105,@0x4d],@"quiet":@[@106,@0x5e]};
 for(NSString *ident in expect){NSDictionary *result=responses[ident][@"result"];SHOT_CHECK([result[@"source_sequence"] isEqual:expect[ident][0]]&&[result[@"source_generation"] isEqual:@9]);
  NSData *png=[[NSData alloc] initWithBase64EncodedString:result[@"data_base64"] options:0];CGImageSourceRef src=CGImageSourceCreateWithData((__bridge CFDataRef)png,NULL);SHOT_CHECK(src);CGImageRef image=CGImageSourceCreateImageAtIndex(src,0,NULL);CFRelease(src);SHOT_CHECK(image&&CGImageGetWidth(image)==64);
  uint8_t px[4]={0};CGColorSpaceRef color=CGColorSpaceCreateDeviceRGB();CGContextRef ctx=CGBitmapContextCreate(px,1,1,8,4,color,kCGImageAlphaNoneSkipFirst|kCGBitmapByteOrder32Little);CGContextDrawImage(ctx,CGRectMake(0,0,1,1),image);CGContextRelease(ctx);CGColorSpaceRelease(color);CGImageRelease(image);
  uint8_t v=[expect[ident][1] unsignedCharValue];SHOT_CHECK(px[0]==v&&px[1]==v&&px[2]==v);}
 // The flush is armed only while frames are wanted.
 sourceDirty=YES;active=NO;SHOT_CHECK(flushDeadline()<0);
 active=YES;streamFPS=60;captureFPS=60;encodeDirty=NO;capturePacing=(VNFramePacing){0};VNFramePacingAdmit(&capturePacing,5000000,60);
 SHOT_CHECK(flushDeadline()==VNFramePacingDue(&capturePacing,60)&&flushDeadline()==5000000+16666-3000);
 scheduleFlush(5000000);SHOT_CHECK(flushSource&&flushDue==5000000+14000);g_source_remove(flushSource);flushSource=0;
 sourceDirty=NO;active=NO;
 // Rows compared: only damaged ones (and their mirror); nothing drawn means unchanged.
 CVPixelBufferRef a=VeynodeDisplayAcquire(64,64);SHOT_CHECK(a);size_t row=CVPixelBufferGetBytesPerRow(a);
 void (^paint)(size_t,uint8_t)=^(size_t r,uint8_t v){CVPixelBufferLockBaseAddress(a,0);memset((uint8_t *)CVPixelBufferGetBaseAddress(a)+r*row,v,row);CVPixelBufferUnlockBaseAddress(a,0);};
 for(size_t r=0;r<64;r++)paint(r,7);
 free(previousFrame);previousFrame=NULL;previousSize=0;pendingAll=copiedAll=YES;memset(pendingRows,0,sizeof pendingRows);memset(copiedRows,0,sizeof copiedRows);
 SHOT_CHECK(!sourceUnchanged(a));SHOT_CHECK(sourceUnchanged(a));
 noteDamage(0,10,64,10);damageCopied();SHOT_CHECK(sourceUnchanged(a)); // re-flushed identical rows
 paint(15,8);noteDamage(0,15,64,1);damageCopied();SHOT_CHECK(!sourceUnchanged(a));SHOT_CHECK(sourceUnchanged(a));
 paint(48,9);noteDamage(0,15,64,1);damageCopied();SHOT_CHECK(!sourceUnchanged(a)); // the damage's vertical mirror
 noteDamage(0,0,0,0);damageCopied();SHOT_CHECK(sourceUnchanged(a)); // a zero rectangle compares everything
 paint(63,1);noteDamage(0,0,0,0);damageCopied();SHOT_CHECK(!sourceUnchanged(a));
 pendingAll=YES;damageCopied();paint(0,2);SHOT_CHECK(!sourceUnchanged(a)); // a new generation: every row
 VeynodeDisplayRelease(a);
 // A retired generation ends the deferred state.
 draw(NULL,0,0,64,64,NULL);SHOT_CHECK(sequence==107&&lazySequence&&testDrawACKs==7);
 close(live[1]);draw(NULL,0,0,64,64,NULL);SHOT_CHECK(testDrawACKs==8&&generationFD<0&&!sourceDirty&&!lazySequence&&!frameReady()&&!cacheValid());
 clearCache();close(live[0]);close(sink[0]);close(sink[1]);g_main_loop_unref(loop);loop=NULL;outputWriter=nil;
 puts("PASS deferred capture: draws without a viewer ACK uncopied with no timer armed; a screenshot copies a quiet scanout at once, waits for a drawing Guest's next draw and copies before its ACK, or falls back after the quiet period (real Metal copies, pixels checked); flush armed for the next slot only while frames are wanted, damage-row comparison incl. mirror and full fallbacks");return 0;
}
