// Read-only raw evdev observation and actual Xorg pointer coordinates.
// XGrabPointer directs the disposable fixture's events to this diagnostic;
// no Guest-side motion injection or calibration changes are performed.
#include <X11/Xlib.h>
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>
int main(void){
 Display *d=XOpenDisplay(NULL);if(!d)return 2;
 Window root=DefaultRootWindow(d);
 int fd=open("/dev/input/event1",O_RDONLY|O_NONBLOCK);if(fd<0)return 3;
 char name[128]={0};if(ioctl(fd,EVIOCGNAME(sizeof(name)),name)<0)return 4;
 printf("COORD_DEVICE %s screen=%dx%d\n",name,DisplayWidth(d,0),DisplayHeight(d,0));
 for(int axis=ABS_X;axis<=ABS_Y;axis++){
  struct input_absinfo info;if(ioctl(fd,EVIOCGABS(axis),&info)<0)return 5;
  printf("COORD_AXIS axis=%d min=%d max=%d resolution=%d fuzz=%d flat=%d value=%d\n",axis,info.minimum,info.maximum,info.resolution,info.fuzz,info.flat,info.value);
 }
 if(XGrabPointer(d,root,False,PointerMotionMask,GrabModeAsync,GrabModeAsync,None,None,CurrentTime)!=GrabSuccess)return 6;
 XSync(d,False);Window qr,qc;int qx,qy,lx,ly;unsigned mask;XQueryPointer(d,root,&qr,&qc,&qx,&qy,&lx,&ly,&mask);printf("COORD_INITIAL x=%d y=%d\n",qx,qy);printf("INPUT_ORACLE_READY coordinate-grab\n");fflush(stdout);
 int x=0,y=0,motions=0,raw_reports=0;
 for(int i=0;i<3000;i++){
  struct input_event events[32];ssize_t size=read(fd,events,sizeof(events));
  if(size>0)for(unsigned j=0;j<(unsigned)size/sizeof(events[0]);j++){
   struct input_event e=events[j];
   if(e.type==EV_ABS&&e.code==ABS_X)x=e.value;
   if(e.type==EV_ABS&&e.code==ABS_Y)y=e.value;
   if(e.type==EV_SYN&&e.code==SYN_REPORT){printf("COORD_RAW {\"x\":%d,\"y\":%d}\n",x,y);raw_reports++;}
  }
  while(XPending(d)){
   XEvent e;XNextEvent(d,&e);
   if(e.type==MotionNotify){printf("COORD_MOTION {\"x\":%d,\"y\":%d,\"synthetic\":%d}\n",e.xmotion.x_root,e.xmotion.y_root,e.xmotion.send_event);motions++;}
  }
  fflush(stdout);usleep(10000);
 }
 printf("INPUT_RESULT {\"motions\":%d,\"raw_reports\":%d}\n",motions,raw_reports);fflush(stdout);
 XUngrabPointer(d,CurrentTime);close(fd);sleep(120);return 0;
}
