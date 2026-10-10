#import "NativeControl.h"
#import "InputFrameDiagnostic.h"
#import "FrameAcquisition.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <math.h>

static double mono(void){return NSProcessInfo.processInfo.systemUptime;}
static BOOL integer(id n){return VNFrameUnsignedInteger(n,NO);}
static BOOL protocolOne(id n){return integer(n)&&[n unsignedLongLongValue]==1;}
static BOOL boolean(id n){return n&&CFGetTypeID((__bridge CFTypeRef)n)==CFBooleanGetTypeID();}
static NSDictionary *safeDiagnostic(id value){
    if(![value isKindOfClass:NSDictionary.class]||[value count]!=3)return nil;
    id version=value[@"version"],elapsed=value[@"elapsed_ms"];
    if(!integer(version)||[version unsignedLongLongValue]!=1||![elapsed isKindOfClass:NSNumber.class]||CFGetTypeID((__bridge CFTypeRef)elapsed)==CFBooleanGetTypeID())return nil;
    double ms=[elapsed doubleValue];if(!isfinite(ms)||ms<0||ms>60000||ms!=floor(ms))return nil;
    NSArray *stages=@[@"admission",@"begin_display",@"authority",@"frame_decode",@"frame_instance",@"event_validation",@"display_validation_io",@"display_validation_rejected",@"source_status_io",@"source_changed",@"validation_expired",@"qmp_dispatch",@"display_invalid_token",@"display_no_snapshot",@"display_instance_mismatch",@"display_epoch_mismatch",@"display_history_miss",@"display_historical_expired",@"display_record_mismatch",@"acquisition_invalid_request",@"acquisition_unavailable",@"acquisition_expired",@"acquisition_invalidated"];
    if(![stages containsObject:value[@"stage"]])return nil;
    return [value copy];
}
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
    NSMutableArray<NSDictionary *> *_suspensions;
    BOOL _suspensionRecorded;
    uint64_t _suspensionCount;
    NSDictionary *_move;
    BOOL _acquisitionNegotiated;
    BOOL _started,_attached,_ready,_humanHold,_releaseConfirmed,_fatal,_suspendPending,_takeInFlight;
    uint64_t _epoch,_generation,_sequence;
}
- (instancetype)initWithSocket:(NSString *)socket vm:(NSString *)vm worker:(NSString *)worker display:(NSString *)display{
    if((self=[super init])){_condition=[NSCondition new];_queue=[NSMutableArray new];_suspensions=[NSMutableArray new];_socket=[socket copy];_vm=[vm copy];_worker=[worker copy];_display=[display copy];_state=@"connecting";_releaseConfirmed=YES;
        if(!hex(worker,64)||!hex(display,32)||!vm.length||vm.length>128||![socket hasPrefix:@"/"]){_fatal=YES;_state=@"unavailable";}}
    return self;
}
- (NSDictionary *)acquisitionContext{[_condition lock];NSDictionary *c=(_ready&&!_fatal&&_acquisitionNegotiated)?@{@"epoch":@(_epoch),@"generation":@(_generation)}:nil;[_condition unlock];return c;}
- (void)start{[_condition lock];if(!_started&&!_fatal){_started=YES;[NSThread detachNewThreadSelector:@selector(run) toTarget:self withObject:nil];}[_condition unlock];}
- (NSDictionary *)status{[_condition lock];NSDictionary *s=@{@"native_frame_acquisition_protocol":@1,@"native_frame_acquisition_negotiated":_acquisitionNegotiated?@YES:@NO,@"native_frame_acquisition_enabled":(_ready&&!_fatal&&_acquisitionNegotiated)?@YES:@NO,@"native_input_supported":@YES,@"native_input_available":(_attached&&!_fatal?@YES:@NO),@"native_input_protocol":@1,@"input_enabled":(_ready&&!_fatal?@YES:@NO),@"display_only":(!_ready||_fatal?@YES:@NO),@"native_input_state":_state,@"native_input_ready":(_ready&&!_fatal?@YES:@NO),@"native_human_hold":(_humanHold?@YES:@NO),@"native_release_confirmed":(_releaseConfirmed?@YES:@NO),@"native_input_epoch":_epoch?@(_epoch):[NSNull null],@"native_input_queued_transitions":@(_queue.count),@"native_input_pending_move":(_move!=nil?@YES:@NO),@"native_input_transition_limit":@32,@"native_input_suspension_count":@(_suspensionCount),@"native_input_suspensions":[_suspensions copy]};[_condition unlock];return s;}
- (void)takeControl{[_condition lock];if(_attached&&!_fatal&&!_ready&&[@[@"view_only",@"suspended_human_hold"] containsObject:_state]){_suspensionRecorded=NO;_generation++;[self.frameAcquisition deactivate];[_queue removeAllObjects];_move=nil;_state=@"taking_control";[_queue addObject:@{@"method":@"take_control",@"local_generation":@(_generation)}];[_condition signal];}[_condition unlock];}
- (void)returnAgent{[_condition lock];if(_attached&&!_fatal&&_epoch&&_humanHold&&[@[@"human_control",@"suspended_human_hold"] containsObject:_state]){_generation++;[self.frameAcquisition deactivate];_ready=NO;_move=nil;[_queue removeAllObjects];_state=@"returning_agent";[_queue addObject:@{@"method":@"return_agent",@"epoch":@(_epoch),@"local_generation":@(_generation)}];[_condition signal];}[_condition unlock];}
- (void)suspendLocked{_generation++;[self.frameAcquisition deactivate];_ready=NO;_move=nil;[_queue removeAllObjects];if(_epoch){_suspendPending=YES;_releaseConfirmed=NO;_state=@"suspending";}else if([_state isEqual:@"taking_control"]){_state=_takeInFlight?@"suspended":@"view_only";}[_condition signal];}
- (void)recordSuspensionLocked:(NSString *)reason queueAge:(double)age remote:(NSDictionary *)remote{
    if(_suspensionRecorded)return;
    _suspensionRecorded=YES;
    NSArray *allowed=@[@"explicit",@"source_invalidated",@"window_closed",@"layout_changed",@"visibility_changed",@"window_focus_lost",@"application_focus_lost",@"dispatch_focus_lost",@"displayed_frame_missing",@"unsupported_key",@"pointer_outside",@"enqueue_rejected",@"transition_overflow",@"queue_expired",@"generation_changed",@"not_ready",@"remote_stale_frame",@"remote_authority_lost",@"remote_unavailable"];
    NSString *safe=[allowed containsObject:reason]?reason:@"explicit";
    NSMutableDictionary *record=[@{@"ordinal":@(++_suspensionCount),@"reason":safe,@"queued_transitions":@(_queue.count),@"monotonic_seconds":@(mono())} mutableCopy];
    if(isfinite(age)&&age>=0)record[@"queue_age_ms"]=@(MIN(age*1000,86400000));
    // The peer diagnostic is separately allowlisted below. No input payload is retained.
    NSDictionary *safeRemote=safeDiagnostic(remote);if(safeRemote)record[@"remote_diagnostic"]=safeRemote;
    if(_suspensions.count==8)[_suspensions removeObjectAtIndex:0];
    [_suspensions addObject:[record copy]];
}
- (void)suspend{[self suspendWithReason:@"explicit"];}
- (void)suspendWithReason:(NSString *)reason{[_condition lock];if(!_fatal&&(_ready||[_state isEqual:@"taking_control"])){
    [self recordSuspensionLocked:reason queueAge:-1 remote:nil];[self suspendLocked];}[_condition unlock];}
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame{return [self enqueueEvent:event frame:frame acquisition:nil];}
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame acquisition:(NSDictionary *)acquisition{return [self enqueueEvent:event frame:frame acquisition:acquisition receipt:nil context:nil];}
- (BOOL)enqueueEvent:(NSDictionary *)event frame:(NSDictionary *)frame acquisition:(NSDictionary *)acquisition receipt:(NSString *)receipt context:(NSDictionary *)context{
    [_condition lock];BOOL accepted=_ready&&!_fatal&&frame;
    if(self.frameAcquisitionRequested)accepted=accepted&&_acquisitionNegotiated&&VNFrameReceiptIdentifier(receipt)&&[context[@"epoch"] unsignedLongLongValue]==_epoch&&[context[@"generation"] unsignedLongLongValue]==_generation;
    else if(receipt)accepted=NO;
    if(accepted){NSMutableDictionary *entry=[@{@"method":@"event",@"epoch":@(_epoch),@"frame":frame,@"event":event,@"local_generation":@(_generation),@"queued_at":@(mono())} mutableCopy];
        if(receipt)entry[@"acquisition_receipt"]=receipt;
        NSNumber *identifier=[self.frameDiagnostic acquireToken:frame observation:acquisition epoch:_epoch];if(identifier)entry[@"diagnostic_id"]=identifier;
        if([event[@"kind"] isEqual:@"move"]){[self.frameAcquisition cancel:_move[@"acquisition_receipt"]];_move=entry;}
        else if(_queue.count>=32){[self recordSuspensionLocked:@"transition_overflow" queueAge:-1 remote:nil];[self suspendLocked];accepted=NO;}
        else{[self.frameAcquisition cancel:_move[@"acquisition_receipt"]];_move=nil;[_queue addObject:entry];}[_condition signal];}
    if(!accepted)[self.frameAcquisition cancel:receipt];[_condition unlock];return accepted;
}
- (void)failLocked{[self.frameAcquisition deactivate];_fatal=YES;_ready=NO;_attached=NO;_state=@"cleanup_unconfirmed";_releaseConfirmed=NO;_move=nil;[_queue removeAllObjects];}
- (void)run{@autoreleasepool{
    int fd=connectPrivate(_socket);
    NSMutableDictionary *arguments=[@{@"protocol_version":@1,@"id":_vm,@"worker_instance":_worker,@"display_instance":_display} mutableCopy];if(self.frameAcquisitionRequested)arguments[@"frame_acquisition_protocol"]=@1;
    NSDictionary *attach=fd>=0?exchange(fd,@{@"method":@"native_attach",@"arguments":arguments}):nil;
    NSDictionary *result=attach[@"result"];
    [_condition lock];BOOL okay=[result isKindOfClass:NSDictionary.class]&&[result[@"native_control_protocol"] isEqual:@1]&&hex(result[@"native_session"],64)&&boolean(result[@"ready"])&&![result[@"ready"] boolValue];
    okay=okay&&(self.frameAcquisitionRequested?protocolOne(result[@"frame_acquisition_protocol"]):result[@"frame_acquisition_protocol"]==nil);
    if(!okay){[self failLocked];[_condition unlock];if(fd>=0)close(fd);return;}_acquisitionNegotiated=self.frameAcquisitionRequested;_attached=YES;_state=@"view_only";[_condition unlock];
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
        double queueAge=[method isEqual:@"event"]?mono()-[entry[@"queued_at"] doubleValue]:-1;
        if([method isEqual:@"event"]&&(!_ready||generation!=_generation||queueAge>0.200)){
            [self recordSuspensionLocked:(!_ready?@"not_ready":generation!=_generation?@"generation_changed":@"queue_expired") queueAge:queueAge remote:nil];
            [self suspendLocked];[_condition unlock];continue;}
        NSMutableDictionary *request=[entry mutableCopy];[request removeObjectForKey:@"local_generation"];[request removeObjectForKey:@"queued_at"];if(_sequence==UINT64_MAX){[self failLocked];[_condition unlock];break;}uint64_t seq=_sequence+1;request[@"sequence"]=@(seq);
        if([method isEqual:@"event"]&&_acquisitionNegotiated&&![self.frameAcquisition claim:entry[@"acquisition_receipt"] token:entry[@"frame"] epoch:_epoch sequence:seq generation:generation now:mono()]){[self recordSuspensionLocked:@"queue_expired" queueAge:queueAge remote:nil];[self suspendLocked];[_condition unlock];continue;}
        _sequence=seq;
        if([method isEqual:@"event"])[self.frameDiagnostic dispatched:entry[@"diagnostic_id"] sequence:seq epoch:_epoch now:mono() queueAge:queueAge];
        [_condition unlock];
        NSDictionary *reply=exchange(fd,request);last=mono();[_condition lock];if([method isEqual:@"event"])[self.frameAcquisition cancel:entry[@"acquisition_receipt"]];
        if(!reply||reply.count!=2||((reply[@"result"]!=nil)==(reply[@"error"]!=nil))||!integer(reply[@"sequence"])||[reply[@"sequence"] unsignedLongLongValue]!=seq){[self failLocked];[_condition unlock];break;}
        NSDictionary *error=reply[@"error"],*r=reply[@"result"];
        if(error){NSString *code=[error isKindOfClass:NSDictionary.class]?error[@"code"]:nil;if(![error isKindOfClass:NSDictionary.class]||[@[@"invalid_request",@"cleanup_unconfirmed"] containsObject:code]||![@[@"authority_lost",@"stale_frame",@"unavailable"] containsObject:code]){[self failLocked];[_condition unlock];break;}if([method isEqual:@"event"]||[method isEqual:@"heartbeat"]){
            [self recordSuspensionLocked:[@"remote_" stringByAppendingString:code] queueAge:queueAge remote:error[@"diagnostic"]];
            [self suspendLocked];[_condition unlock];continue;}[self failLocked];[_condition unlock];break;}
        if(![r isKindOfClass:NSDictionary.class]||!boolean(r[@"ready"])){[self failLocked];[_condition unlock];break;}
        if([method isEqual:@"take_control"]){_takeInFlight=NO;if(![r[@"ready"] boolValue]||!integer(r[@"epoch"])||(_acquisitionNegotiated?!protocolOne(r[@"frame_acquisition_protocol"]):r[@"frame_acquisition_protocol"]!=nil)){[self failLocked];}else{_epoch=[r[@"epoch"] unsignedLongLongValue];_humanHold=YES;_releaseConfirmed=NO;if(generation==_generation){_ready=YES;_state=@"human_control";if(_acquisitionNegotiated)[self.frameAcquisition activateEpoch:_epoch generation:_generation];}else{_suspendPending=YES;_state=@"suspending";}}}
        else if([method isEqual:@"suspend"]||[method isEqual:@"return_agent"]){if([r[@"ready"] boolValue]||!boolean(r[@"released"])||![r[@"released"] boolValue]){[self failLocked];}else{_ready=NO;_releaseConfirmed=YES;if([method isEqual:@"suspend"]){if(!integer(r[@"epoch"]))[self failLocked];else _epoch=[r[@"epoch"] unsignedLongLongValue];}_state=_humanHold?@"suspended_human_hold":@"view_only";if([method isEqual:@"return_agent"]){_humanHold=NO;_epoch=0;_suspendPending=NO;_state=@"view_only";}}}
        else if([method isEqual:@"event"]){if(![r[@"ready"] boolValue]||!boolean(r[@"dispatched"])||![r[@"dispatched"] boolValue]||!boolean(r[@"guest_delivery_confirmed"])||[r[@"guest_delivery_confirmed"] boolValue]){[self failLocked];}}
        else if([method isEqual:@"heartbeat"]){BOOL active=request[@"epoch"]!=nil;if([r[@"ready"] boolValue]!=active||(active&&(!integer(r[@"epoch"])||[r[@"epoch"] unsignedLongLongValue]!=[request[@"epoch"] unsignedLongLongValue])))[self failLocked];}
        if(_fatal)_state=@"cleanup_unconfirmed";BOOL done=_fatal;[_condition unlock];if(done)break;
    }}
    shutdown(fd,SHUT_RDWR);close(fd);
}}
@end
NSString *VNNativeQCode(unsigned short code){
    // ANSI physical positions from HIToolbox Events.h. These are QEMU key names,
    // not text characters; the Guest still applies its keyboard layout.
    static NSDictionary *keys;static dispatch_once_t once;dispatch_once(&once,^{keys=@{@0:@"a",@1:@"s",@2:@"d",@3:@"f",@4:@"h",@5:@"g",@6:@"z",@7:@"x",@8:@"c",@9:@"v",@11:@"b",@12:@"q",@13:@"w",@14:@"e",@15:@"r",@16:@"y",@17:@"t",@18:@"1",@19:@"2",@20:@"3",@21:@"4",@22:@"6",@23:@"5",@24:@"equal",@25:@"9",@26:@"7",@27:@"minus",@28:@"8",@29:@"0",@30:@"bracket_right",@31:@"o",@32:@"u",@33:@"bracket_left",@34:@"i",@35:@"p",@36:@"ret",@37:@"l",@38:@"j",@39:@"apostrophe",@40:@"k",@41:@"semicolon",@42:@"backslash",@43:@"comma",@44:@"slash",@45:@"n",@46:@"m",@47:@"dot",@48:@"tab",@49:@"spc",@50:@"grave_accent",@51:@"backspace",@53:@"esc",@54:@"meta_r",@55:@"meta_l",@56:@"shift",@58:@"alt",@59:@"ctrl",@60:@"shift_r",@61:@"alt_r",@62:@"ctrl_r",@96:@"f5",@97:@"f6",@98:@"f7",@99:@"f3",@100:@"f8",@101:@"f9",@103:@"f11",@109:@"f10",@111:@"f12",@114:@"insert",@115:@"home",@116:@"pgup",@117:@"delete",@118:@"f4",@119:@"end",@120:@"f2",@121:@"pgdn",@122:@"f1",@123:@"left",@124:@"right",@125:@"down",@126:@"up"};});return keys[@(code)];
}

BOOL VNNativeModifierDown(unsigned short code,NSUInteger flags,BOOL *down){
    NSUInteger mask=0;switch(code){case 54:mask=0x10;break;case 55:mask=0x8;break;case 56:mask=0x2;break;case 60:mask=0x4;break;case 58:mask=0x20;break;case 61:mask=0x40;break;case 59:mask=0x1;break;case 62:mask=0x2000;break;default:return NO;}if(!down)return NO;*down=(flags&mask)!=0;return YES;
}
