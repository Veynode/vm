#import "NativeControl.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <math.h>

static double mono(void){return NSProcessInfo.processInfo.systemUptime;}
static BOOL integer(id n){return [n isKindOfClass:NSNumber.class]&&CFGetTypeID((__bridge CFTypeRef)n)!=CFBooleanGetTypeID()&&[n doubleValue]>=1&&[n doubleValue]<=9007199254740991.0&&[n doubleValue]==floor([n doubleValue]);}
static BOOL boolean(id n){return n&&CFGetTypeID((__bridge CFTypeRef)n)==CFBooleanGetTypeID();}
static BOOL hex(NSString *s,NSUInteger length){return [s isKindOfClass:NSString.class]&&s.length==length&&[s rangeOfCharacterFromSet:[[NSCharacterSet characterSetWithCharactersInString:@"0123456789abcdef"] invertedSet]].location==NSNotFound;}
static BOOL waitFD(int fd,short events,double until){
    while(mono()<until){struct pollfd p={fd,events,0};int n=poll(&p,1,(int)MIN(100,MAX(1,(until-mono())*1000)));if(n>0)return(p.revents&events)!=0;if(n<0&&errno!=EINTR)return NO;}return NO;
}
static int connectPrivate(NSString *path){
    if(![path hasPrefix:@"/"]||strlen(path.fileSystemRepresentation)>=sizeof(((struct sockaddr_un *)0)->sun_path))return -1;
    NSString *parent=path.stringByDeletingLastPathComponent;struct stat d,s,after;
    int directory=open(parent.fileSystemRepresentation,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC);
    if(directory<0)return -1;
    BOOL okay=fstat(directory,&d)==0&&S_ISDIR(d.st_mode)&&d.st_uid==getuid()&&(d.st_mode&077)==0&&fstatat(directory,path.lastPathComponent.fileSystemRepresentation,&s,AT_SYMLINK_NOFOLLOW)==0&&S_ISSOCK(s.st_mode)&&s.st_uid==getuid()&&(s.st_mode&077)==0;
    int fd=okay?socket(AF_UNIX,SOCK_STREAM,0):-1;
    if(fd>=0){int one=1;okay=fcntl(fd,F_SETFD,FD_CLOEXEC)==0&&fcntl(fd,F_SETFL,O_NONBLOCK)==0&&setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&one,sizeof(one))==0;struct sockaddr_un a={0};a.sun_family=AF_UNIX;strlcpy(a.sun_path,path.fileSystemRepresentation,sizeof(a.sun_path));
        if(okay&&connect(fd,(void *)&a,sizeof(a))<0){okay=errno==EINPROGRESS&&waitFD(fd,POLLOUT,mono()+3);int error=0;socklen_t size=sizeof(error);okay=okay&&getsockopt(fd,SOL_SOCKET,SO_ERROR,&error,&size)==0&&error==0;}
        uid_t uid=0;gid_t gid=0;okay=okay&&getpeereid(fd,&uid,&gid)==0&&uid==getuid()&&fstatat(directory,path.lastPathComponent.fileSystemRepresentation,&after,AT_SYMLINK_NOFOLLOW)==0&&after.st_dev==s.st_dev&&after.st_ino==s.st_ino;struct stat named;okay=okay&&lstat(parent.fileSystemRepresentation,&named)==0&&S_ISDIR(named.st_mode)&&named.st_dev==d.st_dev&&named.st_ino==d.st_ino&&lstat(path.fileSystemRepresentation,&named)==0&&named.st_dev==s.st_dev&&named.st_ino==s.st_ino;
    }
    close(directory);if(!okay&&fd>=0){close(fd);fd=-1;}return fd;
}
static NSDictionary *exchange(int fd,NSDictionary *request){
    NSData *json=[NSJSONSerialization dataWithJSONObject:request options:NSJSONWritingWithoutEscapingSlashes error:nil];
    if(!json||json.length>=8192)return nil;
    NSMutableData *wire=[json mutableCopy];[wire appendBytes:"\n" length:1];double until=mono()+8;size_t offset=0;
    while(offset<wire.length){if(!waitFD(fd,POLLOUT,until))return nil;ssize_t n=write(fd,(const char *)wire.bytes+offset,wire.length-offset);if(n>0)offset+=n;else if(n==0||(errno!=EINTR&&errno!=EAGAIN))return nil;}
    NSMutableData *reply=[NSMutableData new];
    // Exact single line, no buffered read-ahead. Replies and outstanding work
    // remain bounded even if a local peer is malformed or stops reading.
    while(reply.length<8192){if(!waitFD(fd,POLLIN,until))return nil;char c;ssize_t n=read(fd,&c,1);if(n<0&&(errno==EINTR||errno==EAGAIN))continue;if(n!=1)return nil;if(c=='\n'){id v=[NSJSONSerialization JSONObjectWithData:reply options:0 error:nil];return [v isKindOfClass:NSDictionary.class]?v:nil;}[reply appendBytes:&c length:1];}return nil;
}
@implementation VNNativeControl{
    NSCondition *_condition;
    NSString *_socket,*_vm,*_worker,*_display,*_state;
    NSMutableArray<NSDictionary *> *_queue;
    NSDictionary *_move;
    BOOL _started,_attached,_ready,_humanHold,_releaseConfirmed,_fatal,_suspendPending,_takeInFlight;
    uint64_t _epoch,_generation,_sequence;
}
- (instancetype)initWithSocket:(NSString *)socket vm:(NSString *)vm worker:(NSString *)worker display:(NSString *)display{
    if((self=[super init])){_condition=[NSCondition new];_queue=[NSMutableArray new];_socket=[socket copy];_vm=[vm copy];_worker=[worker copy];_display=[display copy];_state=@"connecting";_releaseConfirmed=YES;
        if(!hex(worker,64)||!hex(display,32)||!vm.length||vm.length>128||![socket hasPrefix:@"/"]){_fatal=YES;_state=@"unavailable";}}
    return self;
}
- (void)start{[_condition lock];if(!_started&&!_fatal){_started=YES;[NSThread detachNewThreadSelector:@selector(run) toTarget:self withObject:nil];}[_condition unlock];}
- (NSDictionary *)status{[_condition lock];NSDictionary *s=@{@"native_input_supported":@YES,@"native_input_available":(_attached&&!_fatal?@YES:@NO),@"native_input_protocol":@1,@"input_enabled":(_ready&&!_fatal?@YES:@NO),@"display_only":(!_ready||_fatal?@YES:@NO),@"native_input_state":_state,@"native_input_ready":(_ready&&!_fatal?@YES:@NO),@"native_human_hold":(_humanHold?@YES:@NO),@"native_release_confirmed":(_releaseConfirmed?@YES:@NO),@"native_input_epoch":_epoch?@(_epoch):[NSNull null],@"native_input_queued_transitions":@(_queue.count),@"native_input_pending_move":(_move!=nil?@YES:@NO),@"native_input_transition_limit":@32};[_condition unlock];return s;}
- (void)takeControl{[_condition lock];if(_attached&&!_fatal&&!_ready&&[@[@"view_only",@"suspended_human_hold"] containsObject:_state]){_generation++;[_queue removeAllObjects];_move=nil;_state=@"taking_control";[_queue addObject:@{@"method":@"take_control",@"local_generation":@(_generation)}];[_condition signal];}[_condition unlock];}
- (void)returnAgent{[_condition lock];if(_attached&&!_fatal&&_epoch&&_humanHold&&[@[@"human_control",@"suspended_human_hold"] containsObject:_state]){_generation++;_ready=NO;_move=nil;[_queue removeAllObjects];_state=@"returning_agent";[_queue addObject:@{@"method":@"return_agent",@"epoch":@(_epoch),@"local_generation":@(_generation)}];[_condition signal];}[_condition unlock];}
- (void)suspendLocked{_generation++;_ready=NO;_move=nil;[_queue removeAllObjects];if(_epoch){_suspendPending=YES;_releaseConfirmed=NO;_state=@"suspending";}else if([_state isEqual:@"taking_control"]){_state=_takeInFlight?@"suspended":@"view_only";}[_condition signal];}
- (void)suspend{[_condition lock];if(!_fatal&&(_ready||[_state isEqual:@"taking_control"]))[self suspendLocked];[_condition unlock];}
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame{
    [_condition lock];BOOL accepted=_ready&&!_fatal&&frame;
    if(accepted){NSDictionary *entry=@{@"method":@"event",@"epoch":@(_epoch),@"frame":frame,@"event":event,@"local_generation":@(_generation),@"queued_at":@(mono())};
        if([event[@"kind"] isEqual:@"move"])_move=entry;
        else if(_queue.count>=32){[self suspendLocked];accepted=NO;}
        else{_move=nil;[_queue addObject:entry];}[_condition signal];}
    [_condition unlock];return accepted;
}
- (void)failLocked{_fatal=YES;_ready=NO;_attached=NO;_state=@"cleanup_unconfirmed";_releaseConfirmed=NO;_move=nil;[_queue removeAllObjects];}
- (void)run{@autoreleasepool{
    int fd=connectPrivate(_socket);
    NSDictionary *attach=fd>=0?exchange(fd,@{@"method":@"native_attach",@"arguments":@{@"protocol_version":@1,@"id":_vm,@"worker_instance":_worker,@"display_instance":_display}}):nil;
    NSDictionary *result=attach[@"result"];
    [_condition lock];BOOL okay=[result isKindOfClass:NSDictionary.class]&&[result[@"native_control_protocol"] isEqual:@1]&&hex(result[@"native_session"],64)&&boolean(result[@"ready"])&&![result[@"ready"] boolValue];
    if(!okay){[self failLocked];[_condition unlock];if(fd>=0)close(fd);return;}_attached=YES;_state=@"view_only";[_condition unlock];
    double last=mono();
    for(;;){@autoreleasepool{
        [_condition lock];while(!_fatal&&!_suspendPending&&!_queue.count&&!_move&&mono()-last<0.5)[_condition waitUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
        if(_fatal){[_condition unlock];break;}
        NSDictionary *entry;
        if(_suspendPending){_suspendPending=NO;entry=@{@"method":@"suspend",@"epoch":@(_epoch),@"local_generation":@(_generation)};}
        else if(_queue.count){entry=_queue[0];[_queue removeObjectAtIndex:0];}
        else if(_move){entry=_move;_move=nil;}
        else{NSMutableDictionary *h=[@{@"method":@"heartbeat",@"local_generation":@(_generation)} mutableCopy];if(_ready)h[@"epoch"]=@(_epoch);entry=h;}
        NSString *method=entry[@"method"];if([method isEqual:@"take_control"])_takeInFlight=YES;uint64_t generation=[entry[@"local_generation"] unsignedLongLongValue];
        if([method isEqual:@"event"]&&(!_ready||generation!=_generation||mono()-[entry[@"queued_at"] doubleValue]>0.200)){[self suspendLocked];[_condition unlock];continue;}
        NSMutableDictionary *request=[entry mutableCopy];[request removeObjectForKey:@"local_generation"];[request removeObjectForKey:@"queued_at"];request[@"sequence"]=@(++_sequence);uint64_t seq=_sequence;[_condition unlock];
        NSDictionary *reply=exchange(fd,request);last=mono();[_condition lock];
        if(!reply||reply.count!=2||((reply[@"result"]!=nil)==(reply[@"error"]!=nil))||!integer(reply[@"sequence"])||[reply[@"sequence"] unsignedLongLongValue]!=seq){[self failLocked];[_condition unlock];break;}
        NSDictionary *error=reply[@"error"],*r=reply[@"result"];
        if(error){NSString *code=[error isKindOfClass:NSDictionary.class]?error[@"code"]:nil;if(![error isKindOfClass:NSDictionary.class]||[@[@"invalid_request",@"cleanup_unconfirmed"] containsObject:code]||![@[@"authority_lost",@"stale_frame",@"unavailable"] containsObject:code]){[self failLocked];[_condition unlock];break;}if([method isEqual:@"event"]||[method isEqual:@"heartbeat"]){[self suspendLocked];[_condition unlock];continue;}[self failLocked];[_condition unlock];break;}
        if(![r isKindOfClass:NSDictionary.class]||!boolean(r[@"ready"])){[self failLocked];[_condition unlock];break;}
        if([method isEqual:@"take_control"]){_takeInFlight=NO;if(![r[@"ready"] boolValue]||!integer(r[@"epoch"])){[self failLocked];}else{_epoch=[r[@"epoch"] unsignedLongLongValue];_humanHold=YES;_releaseConfirmed=NO;if(generation==_generation){_ready=YES;_state=@"human_control";}else{_suspendPending=YES;_state=@"suspending";}}}
        else if([method isEqual:@"suspend"]||[method isEqual:@"return_agent"]){if([r[@"ready"] boolValue]||!boolean(r[@"released"])||![r[@"released"] boolValue]){[self failLocked];}else{_ready=NO;_releaseConfirmed=YES;if([method isEqual:@"suspend"]){if(!integer(r[@"epoch"]))[self failLocked];else _epoch=[r[@"epoch"] unsignedLongLongValue];}_state=_humanHold?@"suspended_human_hold":@"view_only";if([method isEqual:@"return_agent"]){_humanHold=NO;_epoch=0;_suspendPending=NO;_state=@"view_only";}}}
        else if([method isEqual:@"event"]){if(![r[@"ready"] boolValue]||!boolean(r[@"dispatched"])||![r[@"dispatched"] boolValue]||!boolean(r[@"guest_delivery_confirmed"])||[r[@"guest_delivery_confirmed"] boolValue]){[self failLocked];}}
        else if([method isEqual:@"heartbeat"]){BOOL active=request[@"epoch"]!=nil;if([r[@"ready"] boolValue]!=active||(active&&(!integer(r[@"epoch"])||[r[@"epoch"] unsignedLongLongValue]!=[request[@"epoch"] unsignedLongLongValue])))[self failLocked];}
        if(_fatal)_state=@"cleanup_unconfirmed";BOOL done=_fatal;[_condition unlock];if(done)break;
    }}
    shutdown(fd,SHUT_RDWR);close(fd);
}}
@end
NSString *VNNativeQCode(unsigned short code){
    static NSDictionary *keys;static dispatch_once_t once;dispatch_once(&once,^{keys=@{@0:@"a",@1:@"s",@2:@"d",@3:@"f",@4:@"h",@5:@"g",@6:@"z",@7:@"x",@8:@"c",@9:@"v",@11:@"b",@12:@"q",@13:@"w",@14:@"e",@15:@"r",@16:@"y",@17:@"t",@18:@"1",@19:@"2",@20:@"3",@21:@"4",@22:@"6",@23:@"5",@25:@"9",@26:@"7",@28:@"8",@29:@"0",@31:@"o",@32:@"u",@34:@"i",@35:@"p",@36:@"ret",@37:@"l",@38:@"j",@40:@"k",@45:@"n",@46:@"m",@48:@"tab",@49:@"spc",@51:@"backspace",@53:@"esc",@54:@"meta_r",@55:@"meta_l",@56:@"shift",@58:@"alt",@59:@"ctrl",@60:@"shift_r",@61:@"alt_r",@62:@"ctrl_r",@96:@"f5",@97:@"f6",@98:@"f7",@99:@"f3",@100:@"f8",@101:@"f9",@103:@"f11",@109:@"f10",@111:@"f12",@114:@"insert",@115:@"home",@116:@"pgup",@117:@"delete",@118:@"f4",@119:@"end",@120:@"f2",@121:@"pgdn",@122:@"f1",@123:@"left",@124:@"right",@125:@"down",@126:@"up"};});return keys[@(code)];
}

BOOL VNNativeModifierDown(unsigned short code,NSUInteger flags,BOOL *down){
    NSUInteger mask=0;switch(code){case 54:mask=0x10;break;case 55:mask=0x8;break;case 56:mask=0x2;break;case 60:mask=0x4;break;case 58:mask=0x20;break;case 61:mask=0x40;break;case 59:mask=0x1;break;case 62:mask=0x2000;break;default:return NO;}if(!down)return NO;*down=(flags&mask)!=0;return YES;
}
