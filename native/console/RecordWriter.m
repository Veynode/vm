#import "RecordWriter.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <unistd.h>
#include <time.h>

static double monotonic(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
// A JSON record (kind 1) may carry a full-resolution screenshot: up to 8 MiB of PNG is about 10.7 MiB of
// base64, the 12 MiB `console_wire::MAX_JSON_BODY`. At most one such record is queued, outside the 8 MiB
// budget of every other record, whose 4 MiB bound is unchanged.
static const NSUInteger recordLimit=4*1024*1024,jsonLimit=12*1024*1024,queueLimit=8*1024*1024,countLimit=16;
@interface VNWriteReceipt ()
- (void)complete:(BOOL)success began:(double)began admitted:(double)admitted bytes:(NSUInteger)bytes;
@end
@implementation VNWriteReceipt { NSLock *_lock; NSDictionary *_status; }
- (instancetype)init {if((self=[super init])){_lock=[NSLock new];_status=@{@"finished":@NO};}return self;}
- (NSDictionary *)status {[_lock lock];NSDictionary *s=_status;[_lock unlock];return s;}
- (void)complete:(BOOL)success began:(double)began admitted:(double)admitted bytes:(NSUInteger)bytes {
 [_lock lock];_status=@{@"finished":@YES,@"success":@(success),@"queue_us":@((began-admitted)*1e6),@"write_us":@((monotonic()-began)*1e6),@"completed_monotonic_us":@(monotonic()*1e6),@"bytes_written":@(bytes)};[_lock unlock];
}
@end
@implementation VNRecordWriter {
 int _fd; NSCondition *_condition; dispatch_queue_t _queue; BOOL _failed;
 NSUInteger _pending,_bytes;BOOL _largePending;
}
- (instancetype)initWithFD:(int)fd {
 if((self=[super init])){_fd=fd;_condition=[NSCondition new];_queue=dispatch_queue_create("veynode.console-output",DISPATCH_QUEUE_SERIAL);
  int flags=fcntl(fd,F_GETFL);if(flags<0||!(flags&O_NONBLOCK))return nil;
 }return self;
}
- (BOOL)failed {[_condition lock];BOOL value=_failed;[_condition unlock];return value;}
- (NSUInteger)pending {[_condition lock];NSUInteger value=_pending;[_condition unlock];return value;}
- (void)invalidate {[_condition lock];_failed=YES;[_condition broadcast];[_condition unlock];}
- (VNWriteReceipt *)enqueueKind:(uint8_t)kind epoch:(uint64_t)epoch body:(NSData *)body {
 double admitted=monotonic();NSUInteger length=body.length;
 [_condition lock];
 BOOL large=kind==1&&length>recordLimit;
 if(_failed||!length||length>(kind==1?jsonLimit:recordLimit)||_pending>=countLimit||(large?_largePending:(_bytes>=queueLimit||length+13>queueLimit-_bytes))){_failed=YES;[_condition broadcast];[_condition unlock];return nil;}
 // Compact independent wire bytes; mutable caller storage is never retained.
 uint8_t header[13];uint32_t n=CFSwapInt32HostToBig((uint32_t)length);uint64_t e=CFSwapInt64HostToBig(epoch);
 memcpy(header,&n,4);header[4]=kind;memcpy(header+5,&e,8);
 NSMutableData *wire=nil;VNWriteReceipt *receipt=nil;
 @try {wire=[NSMutableData dataWithBytes:header length:13];[wire appendBytes:body.bytes length:length];receipt=[VNWriteReceipt new];}
 @catch(NSException *exception){(void)exception;_failed=YES;[_condition broadcast];[_condition unlock];return nil;}
 _pending++;if(large)_largePending=YES;else _bytes+=length+13;
 dispatch_async(_queue,^{@autoreleasepool{
  double began=monotonic(),deadline=admitted+(large?2.:.5);NSUInteger offset=0;BOOL success=YES;
  while(offset<wire.length){
   if(self.failed||monotonic()>=deadline){success=NO;break;}
   ssize_t count=write(self->_fd,(const uint8_t *)wire.bytes+offset,wire.length-offset);
   if(count>0){offset+=(NSUInteger)count;continue;}
   if(count<0&&errno==EINTR)continue;
   if(count<0&&(errno==EAGAIN||errno==EWOULDBLOCK)){
    // Short waits allow cancellation without extending the admission deadline.
    struct pollfd p={.fd=self->_fd,.events=POLLOUT};int ms=(int)MAX(1,MIN(20,(deadline-monotonic())*1000));
    int r=poll(&p,1,ms);if(r==0||(r<0&&errno==EINTR))continue;
    if(r>0&&!(p.revents&(POLLHUP|POLLERR|POLLNVAL)))continue;
   }
   success=NO;break;
  }
  if(!success)[self invalidate];
  [receipt complete:success began:began admitted:admitted bytes:offset];
  [self->_condition lock];self->_pending--;if(large)self->_largePending=NO;else self->_bytes-=wire.length;[self->_condition broadcast];[self->_condition unlock];
 }});
 [_condition unlock];
 return receipt;
}
- (BOOL)drainFor:(double)seconds {
 double until=monotonic()+seconds;[_condition lock];
 while(_pending&&monotonic()<until)[_condition waitUntilDate:[NSDate dateWithTimeIntervalSinceNow:MIN(.02,MAX(0,until-monotonic()))]];
 BOOL okay=!_pending&&!_failed;[_condition unlock];return okay;
}
@end
