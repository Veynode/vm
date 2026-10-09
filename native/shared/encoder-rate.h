// Private, flat JSON rate actuator. Call only on the encoder's owning thread.
#pragma once
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#include <sys/socket.h>
#include <fcntl.h>
#include <poll.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
static const uint64_t rateMinimum=250000,rateMaximum=40000000;
static NSString *rateInstance;
static uint64_t rateSequence;
static BOOL rateFenced;
static int64_t rateNow(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return (int64_t)t.tv_sec*1000000+t.tv_nsec/1000;}
static BOOL rateUInt(id n,uint64_t lo,uint64_t hi){if(![n isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)n)==CFBooleanGetTypeID())return NO;double v=[n doubleValue];return isfinite(v)&&floor(v)==v&&v>=lo&&v<=hi;}
static BOOL rateText(id s,NSString *pattern){return [s isKindOfClass:NSString.class]&&[s rangeOfString:pattern options:NSRegularExpressionSearch].location!=NSNotFound;}
static NSDictionary *rateParse(NSData *data){
 if(data.length>1023)return nil;NSString *s=[[NSString alloc]initWithData:data encoding:NSUTF8StringEncoding];if(!s)return nil;
 // Flat strings/unsigned decimal integers only: reject escaped keys, floats,
 // booleans, nested payloads and duplicate fields rather than normalizing them.
 NSString *pair=@"\\\"[a-z_]+\\\"\\s*:\\s*(?:\\\"[A-Za-z0-9_-]*\\\"|(?:0|[1-9][0-9]*))";
 NSString *grammar=[NSString stringWithFormat:@"^\\s*\\{\\s*%@(\\s*,\\s*%@)*\\s*\\}\\s*$",pair,pair];
 if(!rateText(s,grammar))return nil;
 NSDictionary *value=[NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
 NSRegularExpression *keys=[NSRegularExpression regularExpressionWithPattern:@"\\\"([a-z_]+)\\\"\\s*:" options:0 error:NULL];
 if(![value isKindOfClass:NSDictionary.class]||[keys numberOfMatchesInString:s options:0 range:NSMakeRange(0,s.length)]!=value.count)return nil;
 return value;
}
static void rateReset(void){rateInstance=[[[NSUUID UUID] UUIDString].lowercaseString stringByReplacingOccurrencesOfString:@"-" withString:@""];rateSequence=0;rateFenced=NO;}
static NSDictionary *rateError(NSString *code){return @{@"error":@{@"code":code}};}
static NSDictionary *rateApply(NSDictionary *q,VTCompressionSessionRef session,BOOL hardware,uint64_t epoch,uint64_t submitted){
 NSString *method=q[@"method"];BOOL set=[method isEqual:@"set_bitrate"];
 NSArray *fields=set?@[@"version",@"request_id",@"method",@"encoder_instance",@"epoch",@"sequence",@"target_bps"]:@[@"version",@"request_id",@"method"];
 if(![[NSSet setWithArray:q.allKeys] isEqual:[NSSet setWithArray:fields]]||!rateUInt(q[@"version"],1,1)||!rateText(q[@"request_id"],@"^[A-Za-z0-9_-]{1,64}$")||(!set&&![method isEqual:@"rate_status"]))return rateError(@"invalid_request");
 if(!session||!hardware||!rateInstance)return rateError(@"encoder_not_ready");
 if(rateFenced)return rateError(@"rate_control_fenced");
 if(set){
  if(!rateText(q[@"encoder_instance"],@"^[0-9a-f]{32}$")||!rateUInt(q[@"epoch"],1,9007199254740991ULL)||!rateUInt(q[@"sequence"],1,9007199254740991ULL)||!rateUInt(q[@"target_bps"],rateMinimum,rateMaximum))return rateError(@"invalid_request");
  if(![q[@"encoder_instance"] isEqual:rateInstance]||[q[@"epoch"] unsignedLongLongValue]!=epoch)return rateError(@"stale_encoder");
  if([q[@"sequence"] unsignedLongLongValue]!=rateSequence+1)return rateError(@"stale_sequence");
  if(VTSessionSetProperty(session,kVTCompressionPropertyKey_AverageBitRate,(__bridge CFNumberRef)q[@"target_bps"])!=noErr){rateFenced=YES;return rateError(@"rate_property_failed");}
 }
 CFTypeRef raw=NULL;OSStatus status=VTSessionCopyProperty(session,kVTCompressionPropertyKey_AverageBitRate,NULL,&raw);
 id value=CFBridgingRelease(raw);
 if(status!=noErr||!rateUInt(value,rateMinimum,rateMaximum)||(set&&![value isEqual:q[@"target_bps"]])){rateFenced=YES;return rateError(@"rate_readback_failed");}
 if(set)rateSequence++;
 return @{@"result":@{@"encoder_instance":rateInstance,@"epoch":@(epoch),@"sequence":@(rateSequence),@"target_bps":value,@"readback_bps":value,@"submitted_frames":@(submitted),@"hardware_verified":@YES}};
}
static __attribute__((unused)) BOOL rateConfigureFD(int fd){struct sockaddr_storage addr; socklen_t n=sizeof(addr);int type=0;socklen_t tn=sizeof(type);int flags=fcntl(fd,F_GETFL);return fd>2&&getsockname(fd,(struct sockaddr *)&addr,&n)==0&&addr.ss_family==AF_UNIX&&getpeername(fd,(struct sockaddr *)&addr,&n)==0&&getsockopt(fd,SOL_SOCKET,SO_TYPE,&type,&tn)==0&&type==SOCK_STREAM&&flags>=0&&fcntl(fd,F_SETFL,flags|O_NONBLOCK)==0&&fcntl(fd,F_SETFD,FD_CLOEXEC)==0;}
// One line at a time, no request queue. Partial requests have a 500ms total budget.
static __attribute__((unused)) BOOL rateServiceFD(int fd,NSMutableData *buffer,int64_t *began,VTCompressionSessionRef session,BOOL hardware,uint64_t epoch,uint64_t submitted){
 if(fd<0)return YES;if(*began&&rateNow()-*began>500000)return NO;
 for(NSUInteger i=0;i<1024;i++){uint8_t b;ssize_t n=recv(fd,&b,1,0);if(n==0)return NO;if(n<0){if(errno==EINTR)continue;return errno==EAGAIN||errno==EWOULDBLOCK;}if(!*began)*began=rateNow();
  if(b!='\n'){[buffer appendBytes:&b length:1];if(buffer.length>1023)return NO;continue;}
  NSDictionary *q=rateParse(buffer);[buffer setLength:0];*began=0;if(!q||!rateText(q[@"request_id"],@"^[A-Za-z0-9_-]{1,64}$"))return NO;
  uint8_t queued;if(recv(fd,&queued,1,MSG_PEEK)>0)return NO;
  NSMutableDictionary *answer=[rateApply(q,session,hardware,epoch,submitted) mutableCopy];answer[@"version"]=@1;answer[@"request_id"]=q[@"request_id"]?:@"invalid";
  NSMutableData *wire=[[NSJSONSerialization dataWithJSONObject:answer options:0 error:NULL] mutableCopy];[wire appendBytes:"\n" length:1];if(wire.length>1024)return NO;
  NSUInteger sent=0;int64_t deadline=rateNow()+500000;while(sent<wire.length){if(rateNow()>=deadline)return NO;ssize_t count=send(fd,(const uint8_t *)wire.bytes+sent,wire.length-sent,0);if(count>0){sent+=(NSUInteger)count;continue;}if(count<0&&errno==EINTR)continue;if(count<0&&(errno==EAGAIN||errno==EWOULDBLOCK)){struct pollfd p={fd,POLLOUT,0};poll(&p,1,10);continue;}return NO;}return YES;
 }return NO;
}
