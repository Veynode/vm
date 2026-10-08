#include "spice-scanout-generation.h"
#include <IOSurface/IOSurface.h>
#include <CoreFoundation/CoreFoundation.h>
#include <sys/stat.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
#include <stdbool.h>
#define MAX_SCANOUT_CLIENTS 16
struct VeynodeScanoutGeneration {
    int source_fd;
    IOSurfaceID id;
    IOSurfaceRef surface;
    bool valid;
    struct { const void *client; int writer; } clients[MAX_SCANOUT_CLIENTS];
};
static bool producer_live(int fd) {
    struct pollfd p={.fd=fd,.events=POLLIN};
    return poll(&p,1,0)>=0 && !(p.revents&(POLLHUP|POLLERR|POLLNVAL));
}
VeynodeScanoutGeneration *veynode_scanout_generation_new(int fd,uint32_t width,uint32_t height) {
    struct stat st;int available=0;struct pollfd p={.fd=fd,.events=POLLIN};
    if(fstat(fd,&st)<0 || !S_ISFIFO(st.st_mode) || ioctl(fd,FIONREAD,&available)<0 || available!=4 || poll(&p,1,0)!=1 || !(p.revents&POLLIN) || (p.revents&(POLLHUP|POLLERR|POLLNVAL))){errno=EPROTO;return NULL;}
    IOSurfaceID id=0;
    if(read(fd,&id,sizeof(id))!=sizeof(id)){errno=EPROTO;return NULL;}
    IOSurfaceRef surface=IOSurfaceLookup(id);
    if(!surface || IOSurfaceGetWidth(surface)!=width || IOSurfaceGetHeight(surface)!=height || !producer_live(fd)){if(surface)CFRelease(surface);errno=ESTALE;return NULL;}
    VeynodeScanoutGeneration *g=calloc(1,sizeof(*g));
    if(!g){CFRelease(surface);return NULL;}
    g->source_fd=fd;g->id=id;g->surface=surface;g->valid=true;
    for(int i=0;i<MAX_SCANOUT_CLIENTS;i++)g->clients[i].writer=-1;
    return g;
}
void veynode_scanout_generation_release_client(VeynodeScanoutGeneration *g,const void *client) {
    if(!g)return;
    for(int i=0;i<MAX_SCANOUT_CLIENTS;i++)if(g->clients[i].client==client && g->clients[i].writer>=0){close(g->clients[i].writer);g->clients[i].writer=-1;g->clients[i].client=NULL;}
}
void veynode_scanout_generation_invalidate(VeynodeScanoutGeneration *g) {
    if(!g)return;
    g->valid=false;
    for(int i=0;i<MAX_SCANOUT_CLIENTS;i++){if(g->clients[i].writer>=0)close(g->clients[i].writer);g->clients[i].writer=-1;g->clients[i].client=NULL;}
    if(g->surface){CFRelease(g->surface);g->surface=NULL;}
}
int veynode_scanout_generation_publish(VeynodeScanoutGeneration *g,const void *client) {
    if(!g||!client){errno=EINVAL;return -1;}
    /* Replacement first invalidates this client's old pipe, including failure. */
    veynode_scanout_generation_release_client(g,client);
    if(!g->valid || !producer_live(g->source_fd)){veynode_scanout_generation_invalidate(g);errno=ESTALE;return -1;}
    int slot=-1;for(int i=0;i<MAX_SCANOUT_CLIENTS;i++)if(g->clients[i].writer<0){slot=i;break;}
    if(slot<0){errno=EMFILE;return -1;}
    int fds[2];if(pipe(fds)<0)return -1;
    if(fcntl(fds[0],F_SETFD,FD_CLOEXEC)<0 || fcntl(fds[1],F_SETFD,FD_CLOEXEC)<0 || write(fds[1],&g->id,sizeof(g->id))!=sizeof(g->id)){
        int error=errno;close(fds[0]);close(fds[1]);errno=error;return -1;
    }
    /* Check again after allocation/write before publishing a generation token. */
    if(!producer_live(g->source_fd)){close(fds[0]);close(fds[1]);veynode_scanout_generation_invalidate(g);errno=ESTALE;return -1;}
    g->clients[slot].client=client;g->clients[slot].writer=fds[1];return fds[0];
}
void veynode_scanout_generation_free(VeynodeScanoutGeneration *g){if(g){veynode_scanout_generation_invalidate(g);free(g);}}
