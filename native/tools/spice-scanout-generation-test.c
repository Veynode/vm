/* SPDX-License-Identifier: GPL-2.0-or-later */
#include "spice-scanout-generation.h"
#include <IOSurface/IOSurface.h>
#include <CoreFoundation/CoreFoundation.h>
#include <assert.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>
#include <errno.h>
#include <stdint.h>
static int countfd(void){int count=0;for(int i=0;i<1024;i++)if(fcntl(i,F_GETFD)>=0)count++;return count;}
static int events(int fd){struct pollfd p={.fd=fd,.events=POLLIN};assert(poll(&p,1,0)>=0);return p.revents;}
static IOSurfaceRef surface(void){CFMutableDictionaryRef d=CFDictionaryCreateMutable(NULL,0,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);int width=64,height=64,bpe=4;CFNumberRef w=CFNumberCreate(NULL,kCFNumberIntType,&width),h=CFNumberCreate(NULL,kCFNumberIntType,&height),b=CFNumberCreate(NULL,kCFNumberIntType,&bpe);CFDictionarySetValue(d,kIOSurfaceWidth,w);CFDictionarySetValue(d,kIOSurfaceHeight,h);CFDictionarySetValue(d,kIOSurfaceBytesPerElement,b);IOSurfaceRef s=IOSurfaceCreate(d);CFRelease(w);CFRelease(h);CFRelease(b);CFRelease(d);assert(s);return s;}
int main(void){IOSurfaceRef s=surface();IOSurfaceID id=IOSurfaceGetID(s);int baseline=countfd();
 for(int cycle=0;cycle<1000;cycle++){
  int source[2];assert(pipe(source)==0);assert(write(source[1],&id,4)==4);VeynodeScanoutGeneration *g=veynode_scanout_generation_new(source[0],64,64);assert(g);
  int readers[16];for(int i=0;i<16;i++){readers[i]=veynode_scanout_generation_publish(g,(void *)(uintptr_t)(i+1));assert(readers[i]>=0);IOSurfaceID received=0;assert(read(readers[i],&received,4)==4&&received==id);}
  assert(veynode_scanout_generation_publish(g,(void *)99)<0&&errno==EMFILE);
  int replaced=veynode_scanout_generation_publish(g,(void *)1);assert(replaced>=0&&(events(readers[0])&POLLHUP));close(readers[0]);readers[0]=replaced;
  veynode_scanout_generation_release_client(g,(void *)2);assert(events(readers[1])&POLLHUP);
  close(source[1]);assert(events(source[0])&POLLHUP);
  /* The production watch calls invalidate on HUP. publish also fails closed
     before its callback runs, including invalidation of existing clients. */
  assert(veynode_scanout_generation_publish(g,(void *)99)<0&&errno==ESTALE);
  for(int i=0;i<16;i++){assert(events(readers[i])&POLLHUP);close(readers[i]);}
  veynode_scanout_generation_free(g);close(source[0]);assert(countfd()==baseline);
 }
 CFRelease(s);puts("{\"actual_iosurface\":true,\"cycles\":1000,\"clients_per_generation\":16,\"overflow_rejected\":true,\"replacement_invalidates_old\":true,\"producer_hup_invalidates_all\":true,\"fd_count_stable\":true}");return 0;}
