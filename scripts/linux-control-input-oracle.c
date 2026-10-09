// Independent actual Linux evdev and Xorg oracle; never injects Guest input.
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <linux/input.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
#include <time.h>
int main(int argc,char**argv){
 long seconds=180;if(argc>2)return 2;if(argc==2){char*end=NULL;seconds=strtol(argv[1],&end,10);if(!end||*end||seconds<30||seconds>3600)return 2;}
 struct timespec began,current;if(clock_gettime(CLOCK_MONOTONIC,&began))return 2;
 int fds[16],nf=0;for(int i=0;i<16;i++){char path[80],name[256]={0};snprintf(path,sizeof(path),"/dev/input/event%d",i);int fd=open(path,O_RDONLY|O_NONBLOCK);if(fd<0)continue;
 if(ioctl(fd,EVIOCGNAME(sizeof(name)),name)>=0&&(strstr(name,"Virtio Keyboard")||strstr(name,"Virtio Tablet"))){fds[nf++]=fd;printf("INPUT_DEVICE %s %s\n",path,name);}else close(fd);}
 Display*d=XOpenDisplay(NULL);if(!d||nf!=2)return 2;
 Window root=DefaultRootWindow(d);XSetWindowAttributes a={.override_redirect=True,.background_pixel=0x112233};
 Window w=XCreateWindow(d,root,100,100,400,300,0,CopyFromParent,InputOutput,CopyFromParent,CWOverrideRedirect|CWBackPixel,&a);
 XSelectInput(d,w,ButtonPressMask|ButtonReleaseMask|KeyPressMask|KeyReleaseMask|PointerMotionMask);XMapRaised(d,w);XSetInputFocus(d,w,RevertToParent,CurrentTime);XSync(d,False);GC gc=XCreateGC(d,w,0,NULL);
 puts("CONTROL_INPUT_READY");fflush(stdout);
 for(;;){
  if(clock_gettime(CLOCK_MONOTONIC,&current)||current.tv_sec-began.tv_sec>=seconds)break;
  for(int j=0;j<nf;j++){struct input_event e;while(read(fds[j],&e,sizeof(e))==sizeof(e)){if(e.type==EV_KEY||e.type==EV_ABS||e.type==EV_REL)printf("RAW_INPUT {\"type\":%d,\"code\":%d,\"value\":%d}\n",e.type,e.code,e.value);}}
  while(XPending(d)){XEvent e;XNextEvent(d,&e);
   if(e.type==ButtonPress||e.type==ButtonRelease){printf("X_INPUT {\"type\":%d,\"button\":%u,\"x\":%d,\"y\":%d,\"synthetic\":%d}\n",e.type,e.xbutton.button,e.xbutton.x_root,e.xbutton.y_root,e.xbutton.send_event);if(e.type==ButtonPress&&e.xbutton.button==1){XSetForeground(d,gc,0x00ff00);XFillRectangle(d,w,gc,0,0,100,100);}}
   if(e.type==KeyPress||e.type==KeyRelease){printf("X_INPUT {\"type\":%d,\"keycode\":%u,\"keysym\":%lu,\"synthetic\":%d}\n",e.type,e.xkey.keycode,XLookupKeysym(&e.xkey,0),e.xkey.send_event);}
   if(e.type==MotionNotify)printf("X_INPUT {\"type\":6,\"x\":%d,\"y\":%d,\"state\":%u,\"synthetic\":%d}\n",e.xmotion.x_root,e.xmotion.y_root,e.xmotion.state,e.xmotion.send_event);
   XSync(d,False);
  }
  fflush(stdout);usleep(10000);
 }
 puts("CONTROL_INPUT_ORACLE_ENDED");fflush(stdout);return 0;
}
