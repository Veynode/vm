// Pure protocol/socket tests. No VT session, capture, window or Guest.
#include "../shared/encoder-rate.h"
#include <assert.h>
static NSDictionary *parse(NSString *s){return rateParse([s dataUsingEncoding:NSUTF8StringEncoding]);}
int main(void){@autoreleasepool{
 NSDictionary *valid=parse(@"{\"version\":1,\"request_id\":\"query\",\"method\":\"rate_status\"}");assert(valid);
 for(NSString *bad in @[@"{\"version\":1,\"version\":1,\"method\":\"rate_status\",\"request_id\":\"q\"}",@"{\"version\":true}",@"{\"version\":1.0}",@"{\"version\":null}",@"{\"nested\":{}}",@"{\"v\\u0065rsion\":1}"]){assert(!parse(bad));}
 assert([rateApply(valid,NULL,NO,1,0)[@"error"][@"code"] isEqual:@"encoder_not_ready"]);
 NSMutableDictionary *unknown=[valid mutableCopy];unknown[@"extra"]=@1;assert([rateApply(unknown,NULL,NO,1,0)[@"error"][@"code"] isEqual:@"invalid_request"]);
 rateReset();NSString *before=rateInstance;rateReset();assert(![before isEqual:rateInstance]&&rateSequence==0&&!rateFenced);
 int sockets[2];assert(socketpair(AF_UNIX,SOCK_STREAM,0,sockets)==0);assert(rateConfigureFD(sockets[1]));NSMutableData *buffer=[NSMutableData new];int64_t began=0;
 const char *query="{\"version\":1,\"request_id\":\"query\",\"method\":\"rate_status\"}\n";assert(write(sockets[0],query,strlen(query))==(ssize_t)strlen(query));assert(rateServiceFD(sockets[1],buffer,&began,NULL,NO,1,0));char reply[1024];ssize_t count=read(sockets[0],reply,sizeof(reply));assert(count>0);NSDictionary *answer=[NSJSONSerialization JSONObjectWithData:[NSData dataWithBytes:reply length:(NSUInteger)count] options:0 error:NULL];assert([answer[@"request_id"] isEqual:@"query"]&&[answer[@"error"][@"code"] isEqual:@"encoder_not_ready"]);
 assert(write(sockets[0],"{",1)==1);assert(rateServiceFD(sockets[1],buffer,&began,NULL,NO,1,0));began=rateNow()-500001;assert(!rateServiceFD(sockets[1],buffer,&began,NULL,NO,1,0));close(sockets[0]);close(sockets[1]);
 assert(socketpair(AF_UNIX,SOCK_STREAM,0,sockets)==0);assert(rateConfigureFD(sockets[1]));[buffer setLength:0];began=0;assert(write(sockets[0],query,strlen(query))>0);assert(write(sockets[0],query,strlen(query))>0);assert(!rateServiceFD(sockets[1],buffer,&began,NULL,NO,1,0));close(sockets[0]);close(sockets[1]);
 puts("encoder-rate protocol/socket tests passed; no hardware acceptance");return 0;
}}
