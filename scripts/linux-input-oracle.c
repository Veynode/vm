// Actual X11 console event oracle; no synthetic Guest-side input is generated.
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/keysym.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(void){
 Display *d=XOpenDisplay(NULL);if(!d)return 2;
 Window root=DefaultRootWindow(d);
 XSetWindowAttributes a={.override_redirect=True,.background_pixel=0x112233};
 Window w=XCreateWindow(d,root,100,100,400,300,0,CopyFromParent,InputOutput,CopyFromParent,CWOverrideRedirect|CWBackPixel,&a);
 XSelectInput(d,w,ButtonPressMask|ButtonReleaseMask|KeyPressMask|KeyReleaseMask|PointerMotionMask);
 XMapRaised(d,w);XSetInputFocus(d,w,RevertToParent,CurrentTime);XSync(d,False);
 printf("INPUT_ORACLE_READY window=%lu origin=100,100 size=400,300\n",w);fflush(stdout);
 GC gc=XCreateGC(d,w,0,NULL);int clicks=0,keys=0,button_releases=0,key_releases=0;
 for(int i=0;i<250;i++){
  while(XPending(d)){
   XEvent e;XNextEvent(d,&e);
   if(e.type==ButtonPress||e.type==ButtonRelease){
    printf("INPUT_EVENT button type=%d button=%u local=%d,%d root=%d,%d synthetic=%d\n",e.type,e.xbutton.button,e.xbutton.x,e.xbutton.y,e.xbutton.x_root,e.xbutton.y_root,e.xbutton.send_event);
    if(e.type==ButtonRelease&&e.xbutton.button==1&&!e.xbutton.send_event)button_releases++;
    if(e.type==ButtonPress&&e.xbutton.button==1&&abs(e.xbutton.x_root-200)<=1&&abs(e.xbutton.y_root-200)<=1&&!e.xbutton.send_event){clicks++;XSetForeground(d,gc,0x00ff00);XFillRectangle(d,w,gc,0,0,100,100);}
   }
   if(e.type==KeyPress||e.type==KeyRelease){
    KeySym sym=XLookupKeysym(&e.xkey,0);
    printf("INPUT_EVENT key type=%d keycode=%u keysym=%lu synthetic=%d\n",e.type,e.xkey.keycode,sym,e.xkey.send_event);
    if(e.type==KeyRelease&&sym==XK_a&&!e.xkey.send_event)key_releases++;
    if(e.type==KeyPress&&sym==XK_a&&!e.xkey.send_event){keys++;XSetForeground(d,gc,0xff0000);XFillRectangle(d,w,gc,200,0,100,100);}
   }
   XSync(d,False);fflush(stdout);
  }
  usleep(100000);
 }
 XImage *p=XGetImage(d,w,0,0,400,300,AllPlanes,ZPixmap);if(!p)return 3;
 unsigned long click=XGetPixel(p,20,20),key=XGetPixel(p,220,20);
 printf("INPUT_RESULT {\"clicks\":%d,\"keys\":%d,\"button_releases\":%d,\"key_releases\":%d,\"click_pixel\":%lu,\"key_pixel\":%lu,\"passed\":%s}\n",clicks,keys,button_releases,key_releases,click,key,clicks==1&&keys==1&&button_releases==1&&key_releases==1&&click==0x00ff00&&key==0xff0000?"true":"false");fflush(stdout);
 // Keep the actual changed window alive for host capture until fixture shutdown.
 sleep(120);return 0;
}
