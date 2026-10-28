/* SPDX-License-Identifier: GPL-2.0-or-later */
// Independent diagnostic loader. Never edits the official QEMU/app images.
#include <dlfcn.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "qemu-ram-reclaim.h"
extern char **environ;
int main(int argc,char **argv){
 if(argc<3)return 2;
 const char *path=getenv("VEYNODE_SPICE_LIBRARY");if(!path){fprintf(stderr,"Explicit test server library required\n");return 2;}
 void *server=dlopen(path,RTLD_LOCAL|RTLD_NOW);if(!server){fprintf(stderr,"server dlopen: %s\n",dlerror());return 3;}
 void *qemu=dlopen(argv[1],RTLD_LOCAL|RTLD_NOW);if(!qemu){fprintf(stderr,"qemu dlopen: %s\n",dlerror());return 3;}
 void *expected=dlsym(server,"spice_server_new"),*bound=dlsym(qemu,"spice_server_new");
 unsigned count=0;for(uint32_t i=0;i<_dyld_image_count();i++){const char *name=_dyld_get_image_name(i);if(strstr(name,"spice-server")){count++;fprintf(stderr,"VEYNODE_LOADED_SPICE %s\n",name);}}
 if(!expected||expected!=bound||count!=1){fprintf(stderr,"Explicit server dependency selection failed; refusing VM startup\n");return 4;}
 Dl_info info={0};dladdr(bound,&info);fprintf(stderr,"VEYNODE_VERIFIED_SPICE %s\n",info.dli_fname);
 int (*init)(int,char**,char**)=dlsym(qemu,"qemu_init");void(*loop)(void)=dlsym(qemu,"qemu_main_loop");void(*cleanup)(void)=dlsym(qemu,"qemu_cleanup");
 if(!init||!loop||!cleanup)return 5;qemu_ram_reclaim_install(qemu);init(argc-1,argv+1,environ);loop();cleanup();return 0;
}
