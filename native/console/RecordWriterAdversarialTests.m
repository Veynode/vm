// Actual nonblocking pipes and the production writer; no Metal, VM or encoder.
#import <Foundation/Foundation.h>
#import "RecordWriter.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <time.h>
#include <unistd.h>

#define CHECK(condition, message) do { if (!(condition)) { fprintf(stderr,"FAIL %s:%d: %s\n",__func__,__LINE__,message); exit(1); } } while (0)
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
static void openPipe(int fds[2]){
 CHECK(pipe(fds)==0,"pipe");
 for(int i=0;i<2;i++){int flags=fcntl(fds[i],F_GETFL);CHECK(flags>=0&&fcntl(fds[i],F_SETFL,flags|O_NONBLOCK)==0,"nonblock");}
}
static NSData *body(NSUInteger count,uint8_t value){NSMutableData *d=[NSMutableData dataWithLength:count];memset(d.mutableBytes,value,count);return d;}
static NSUInteger fillPipe(int fd){
 uint8_t bytes[4096];memset(bytes,0xee,sizeof(bytes));NSUInteger n=0;size_t chunk=sizeof(bytes);
 for(;;){ssize_t c=write(fd,bytes,chunk);if(c>0){n+=(NSUInteger)c;CHECK(n<=1024*1024,"pipe storage unexpectedly exceeds test bound");continue;}if(c<0&&errno==EINTR)continue;CHECK(c<0&&(errno==EAGAIN||errno==EWOULDBLOCK),"fill reaches backpressure");if(chunk>1){chunk=1;continue;}return n;}
}
static NSData *readAvailable(int fd){
 NSMutableData *data=[NSMutableData new];uint8_t bytes[8192];
 for(;;){ssize_t n=read(fd,bytes,sizeof(bytes));if(n>0){CHECK(data.length+(NSUInteger)n<=8*1024*1024,"bounded read");[data appendBytes:bytes length:(NSUInteger)n];continue;}if(n<0&&errno==EINTR)continue;CHECK(n==0||(n<0&&(errno==EAGAIN||errno==EWOULDBLOCK)),"read status");return data;}
}
static NSArray<NSDictionary *> *parse(NSData *data){
 NSMutableArray *records=[NSMutableArray new];const uint8_t *p=data.bytes;NSUInteger at=0;
 while(at<data.length){CHECK(data.length-at>=13,"complete record header");uint32_t length;uint64_t epoch;memcpy(&length,p+at,4);length=CFSwapInt32BigToHost(length);uint8_t kind=p[at+4];memcpy(&epoch,p+at+5,8);epoch=CFSwapInt64BigToHost(epoch);at+=13;CHECK(length>0&&length<=4*1024*1024&&length<=data.length-at,"bounded complete body");[records addObject:@{@"kind":@(kind),@"epoch":@(epoch),@"body":[data subdataWithRange:NSMakeRange(at,length)]}];at+=length;
 }return records;
}
static void confirmReceipt(VNWriteReceipt *receipt,BOOL success){
 NSDictionary *s=receipt.status;CHECK([s[@"finished"] boolValue],"receipt finished");CHECK([s[@"success"] boolValue]==success,"receipt success");CHECK([s[@"queue_us"] doubleValue]>=0&&[s[@"write_us"] doubleValue]>=0,"timing nonnegative");
}
static void mixedFramingAndFIFO(void){
 int fds[2];openPipe(fds);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];CHECK(w,"writer");NSMutableArray<VNWriteReceipt *> *receipts=[NSMutableArray new];
 for(NSUInteger i=0;i<10;i++){VNWriteReceipt *r=[w enqueueKind:(i%2?2:1) epoch:100+i body:body(128,(uint8_t)i)];CHECK(r,"admitted");[receipts addObject:r];}
 CHECK([w drainFor:2]&&w.pending==0&&!w.failed,"actual drained");NSArray *records=parse(readAvailable(fds[0]));CHECK(records.count==10,"all records");
 for(NSUInteger i=0;i<10;i++){CHECK([records[i][@"epoch"] unsignedLongLongValue]==100+i,"FIFO epoch");CHECK([records[i][@"kind"] unsignedIntValue]==(i%2?2:1),"mixed kind");CHECK([records[i][@"body"] isEqual:body(128,(uint8_t)i)],"exact payload");confirmReceipt(receipts[i],YES);CHECK([receipts[i] status][@"bytes_written"]&&[[receipts[i] status][@"bytes_written"] unsignedIntegerValue]==141,"actual header plus body bytes");}
 close(fds[0]);close(fds[1]);puts("PASS mixed framing, FIFO, actual receipt bytes");
}
static void concurrentProducers(void){
 int fds[2];openPipe(fds);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];dispatch_group_t group=dispatch_group_create();NSLock *lock=[NSLock new];NSMutableArray<VNWriteReceipt *> *receipts=[NSMutableArray new];
 for(NSUInteger i=0;i<8;i++)dispatch_group_async(group,dispatch_get_global_queue(QOS_CLASS_DEFAULT,0),^{@autoreleasepool{VNWriteReceipt *r=[w enqueueKind:(i%2?2:1) epoch:200+i body:body(256,(uint8_t)i)];CHECK(r,"concurrent admission");[lock lock];[receipts addObject:r];[lock unlock];}});
 CHECK(dispatch_group_wait(group,dispatch_time(DISPATCH_TIME_NOW,2*NSEC_PER_SEC))==0,"bounded producers");CHECK([w drainFor:2],"drain concurrent");NSArray *records=parse(readAvailable(fds[0]));CHECK(records.count==8,"eight complete records");NSMutableSet *seen=[NSMutableSet new];
 for(NSDictionary *r in records){NSUInteger i=[r[@"epoch"] unsignedIntegerValue]-200;CHECK(i<8&&![seen containsObject:@(i)],"unique producer");[seen addObject:@(i)];CHECK([r[@"body"] isEqual:body(256,(uint8_t)i)],"no record interleaving");}
 for(VNWriteReceipt *r in receipts)confirmReceipt(r,YES);close(fds[0]);close(fds[1]);puts("PASS concurrent mixed producers, no interleaving");
}
static void mutableBodyIsCopied(void){
 int fds[2];openPipe(fds);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];NSMutableData *input=[body(512,0x55) mutableCopy];VNWriteReceipt *r=[w enqueueKind:1 epoch:7 body:input];CHECK(r,"admitted mutable input");memset(input.mutableBytes,0x99,input.length);CHECK([w drainFor:2],"drained");NSArray *records=parse(readAvailable(fds[0]));CHECK(records.count==1&&[records[0][@"body"] isEqual:body(512,0x55)],"caller mutation cannot alter wire");confirmReceipt(r,YES);close(fds[0]);close(fds[1]);puts("PASS independent wire storage");
}
static void partialTimeoutIsPermanent(void){
 int fds[2];openPipe(fds);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];double began=now();VNWriteReceipt *r=[w enqueueKind:1 epoch:9 body:body(1024*1024,0x42)];CHECK(r,"large admitted");CHECK(![w drainFor:2],"backpressure cannot report drain success");CHECK(now()-began>=.45&&now()-began<2,"bounded admission deadline");confirmReceipt(r,NO);NSUInteger written=[r.status[@"bytes_written"] unsignedIntegerValue];CHECK(written>0&&written<1024*1024+13,"real partial framed write");CHECK(w.failed&&w.pending==0,"permanent fail and release");CHECK(![w enqueueKind:1 epoch:10 body:body(1,1)],"no next frame after partial");NSData *raw=readAvailable(fds[0]);CHECK(raw.length==written,"only actual submitted bytes");usleep(25000);CHECK(readAvailable(fds[0]).length==0,"never resumes partial after consumer returns");close(fds[0]);close(fds[1]);puts("PASS actual partial pipe timeout, permanent fence");
}
static void countBudgetIncludesInFlight(void){
 int fds[2];openPipe(fds);fillPipe(fds[1]);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];NSMutableArray<VNWriteReceipt *> *receipts=[NSMutableArray new];
 for(int i=0;i<16;i++){VNWriteReceipt *r=[w enqueueKind:1 epoch:(uint64_t)i body:body(1,1)];CHECK(r,"first sixteen admitted");[receipts addObject:r];}
 CHECK(w.pending==16,"inflight included in count");CHECK(![w enqueueKind:1 epoch:17 body:body(1,1)]&&w.failed,"seventeenth fails closed");CHECK(![w drainFor:2]&&w.pending==0,"all queued reservations retired");for(VNWriteReceipt *r in receipts){confirmReceipt(r,NO);CHECK([r.status[@"bytes_written"] unsignedIntegerValue]==0,"full pipe writes zero");}close(fds[0]);close(fds[1]);puts("PASS count bound includes current writer");
}
static void byteBudgetIncludesHeadersAndInFlight(void){
 int fds[2];openPipe(fds);fillPipe(fds[1]);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];NSData *large=body(4*1024*1024,1);VNWriteReceipt *r=[w enqueueKind:2 epoch:1 body:large];CHECK(r,"max body admitted");CHECK(![w enqueueKind:1 epoch:2 body:large],"two max records exceed 8MiB with headers");CHECK(![w drainFor:2]&&w.failed&&w.pending==0,"byte failure retired");confirmReceipt(r,NO);close(fds[0]);close(fds[1]);puts("PASS byte bound before count bound");
}
static void explicitInvalidationAndBrokenPipe(void){
 int fds[2];openPipe(fds);fillPipe(fds[1]);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];VNWriteReceipt *r=[w enqueueKind:1 epoch:1 body:body(16,1)];CHECK(r,"admitted");CHECK(![w drainFor:.02]&&w.pending==1&&!w.failed,"drain timeout is not fabricated completion");[w invalidate];CHECK(![w drainFor:2]&&w.pending==0,"cancel genuinely retired");confirmReceipt(r,NO);close(fds[0]);close(fds[1]);
 openPipe(fds);w=[[VNRecordWriter alloc]initWithFD:fds[1]];close(fds[0]);r=[w enqueueKind:1 epoch:2 body:body(16,2)];CHECK(r,"broken pipe queued");CHECK(![w drainFor:2]&&w.failed&&w.pending==0,"EPIPE fatal");confirmReceipt(r,NO);CHECK([r.status[@"bytes_written"] unsignedIntegerValue]==0,"EPIPE not submitted");close(fds[1]);puts("PASS cancellation, drain timeout, actual EPIPE");
}
static void invalidAdmission(void){
 int fds[2];CHECK(pipe(fds)==0,"pipe");CHECK(![[VNRecordWriter alloc]initWithFD:fds[1]],"blocking FD rejected");close(fds[0]);close(fds[1]);openPipe(fds);VNRecordWriter *w=[[VNRecordWriter alloc]initWithFD:fds[1]];CHECK(![w enqueueKind:1 epoch:1 body:[NSData data]]&&w.failed,"empty rejected");CHECK(w.pending==0,"invalid reserves nothing");close(fds[0]);close(fds[1]);puts("PASS invalid admission");
}
int main(void){@autoreleasepool{signal(SIGPIPE,SIG_IGN);mixedFramingAndFIFO();concurrentProducers();mutableBodyIsCopied();partialTimeoutIsPermanent();countBudgetIncludesInFlight();byteBudgetIncludesHeadersAndInFlight();explicitInvalidationAndBrokenPipe();invalidAdmission();puts("PASS all eight real-pipe writer groups; no GPU workload");}return 0;}
