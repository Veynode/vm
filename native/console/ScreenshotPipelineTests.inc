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
