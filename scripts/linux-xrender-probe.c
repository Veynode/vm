/* Actual X11/XRender/GLX pixel diagnostic. No performance or application claim.
 * Expected colors are computed from input values, not a reference screenshot.
 * XRender operator/alpha semantics: X Rendering Extension protocol 0.11.
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/Xrender.h>
#include <GL/gl.h>
#include <GL/glx.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#define W 64
#define H 64
static Display *d;
static int screen, failures, cases, exclude_diagonal;
static Window win;
static GC gc;
static XRenderPictFormat *rgb, *a8;
static unsigned char expected[W*H*3];
static const char *outdir;
static int xerror(Display *display, XErrorEvent *e) {
    char text[256]; XGetErrorText(display,e->error_code,text,sizeof(text));
    fprintf(stderr,"X_ERROR request=%u minor=%u code=%u text=%s\n",e->request_code,e->minor_code,e->error_code,text);
    exit(2);
}
static void expect_fill(unsigned color) {
    for(int i=0;i<W*H;i++){expected[i*3]=color>>16;expected[i*3+1]=color>>8;expected[i*3+2]=color;}
}
static void expect_rect(int x,int y,int w,int h,unsigned color) {
    for(int j=y;j<y+h;j++)for(int i=x;i<x+w;i++) {
        unsigned char *p=expected+(j*W+i)*3;p[0]=color>>16;p[1]=color>>8;p[2]=color;
    }
}
static void fill(Drawable dst,unsigned color) {XSetForeground(d,gc,color);XFillRectangle(d,dst,gc,0,0,W,H);}
static unsigned channel(unsigned long pixel,unsigned long mask) {
    if(!mask)return 0;unsigned shift=0;while(!(mask&1)){shift++;mask>>=1;}
    return ((pixel>>shift)&mask)*255/mask;
}
static int check(const char *name,Drawable target,int tolerance) {
    XSync(d,False);XImage *image=XGetImage(d,target,0,0,W,H,AllPlanes,ZPixmap);
    if(!image){fprintf(stderr,"XGetImage failed\n");exit(2);}
    char path[1024];snprintf(path,sizeof(path),"%s/%s.ppm",outdir,name);FILE *f=fopen(path,"wb");if(!f)exit(2);
    fprintf(f,"P6\n%d %d\n255\n",W,H);
    unsigned mismatch=0,max_error=0,first_x=0,first_y=0,first_actual=0,first_expected=0;
    for(int y=0;y<H;y++)for(int x=0;x<W;x++) {
        unsigned long px=XGetPixel(image,x,y);unsigned char actual[3]={channel(px,rgb->direct.redMask<<rgb->direct.red),channel(px,rgb->direct.greenMask<<rgb->direct.green),channel(px,rgb->direct.blueMask<<rgb->direct.blue)};
        fwrite(actual,1,3,f);int bad=0;unsigned char *want=expected+(y*W+x)*3;
        for(int c=0;c<3;c++){unsigned diff=abs((int)actual[c]-want[c]);if(diff>max_error)max_error=diff;if(diff>(unsigned)tolerance)bad=1;}
        if(exclude_diagonal && x==y)bad=0;
        if(bad){if(!mismatch){first_x=x;first_y=y;first_actual=(actual[0]<<16)|(actual[1]<<8)|actual[2];first_expected=(want[0]<<16)|(want[1]<<8)|want[2];}mismatch++;}
    }
    fclose(f);XDestroyImage(image);
    snprintf(path,sizeof(path),"%s/%s.expected.ppm",outdir,name);f=fopen(path,"wb");if(!f)exit(2);
    fprintf(f,"P6\n%d %d\n255\n",W,H);fwrite(expected,1,sizeof(expected),f);fclose(f);
    cases++;failures+=!!mismatch;
    printf("XRENDER_CASE {\"name\":\"%s\",\"passed\":%s,\"mismatched_pixels\":%u,\"total_pixels\":4096,\"excluded_diagonal_pixels\":%d,\"max_channel_error\":%u,\"tolerance\":%d,\"first_x\":%u,\"first_y\":%u,\"first_actual_rgb\":\"%06x\",\"first_expected_rgb\":\"%06x\"}\n",name,mismatch?"false":"true",mismatch,exclude_diagonal?64:0,max_error,tolerance,first_x,first_y,first_actual,first_expected);fflush(stdout);
    return !mismatch;
}
static void mask_expected(int x0,int y0,int width,int height) {
    for(int y=0;y<height;y++)for(int x=0;x<width;x++) {
        const int alpha[4]={0,64,128,255};int a=alpha[x*4/width];
        expect_rect(x0+x,y0+y,1,1,(a<<16)|(255-a));
    }
}
int main(int argc,char **argv) {
    if(argc!=2){fprintf(stderr,"usage: linux-xrender-probe OUTPUT_DIRECTORY\n");return 2;}
    outdir=argv[1];d=XOpenDisplay(NULL);if(!d)return 2;XSetErrorHandler(xerror);screen=DefaultScreen(d);
    int major=0,minor=0;if(!XRenderQueryVersion(d,&major,&minor))return 2;
    printf("XRENDER_VERSION %d.%d depth=%d\n",major,minor,DefaultDepth(d,screen));
    rgb=XRenderFindVisualFormat(d,DefaultVisual(d,screen));a8=XRenderFindStandardFormat(d,PictStandardA8);if(!rgb||!a8)return 2;
    win=XCreateSimpleWindow(d,RootWindow(d,screen),20,20,W,H,0,0,0);XMapWindow(d,win);XSync(d,False);
    gc=XCreateGC(d,win,0,NULL);
    Pixmap pix=XCreatePixmap(d,win,W,H,DefaultDepth(d,screen));
    Picture dst=XRenderCreatePicture(d,pix,rgb,0,NULL);
    XRenderColor red={65535,0,0,65535},blue={0,0,65535,65535};Picture solid=XRenderCreateSolidFill(d,&red);
    fill(pix,0x0000ff);XSetForeground(d,gc,0xff0000);XFillRectangle(d,pix,gc,8,12,32,20);expect_fill(0x0000ff);expect_rect(8,12,32,20,0xff0000);check("core_fill_pixmap",pix,0);
    fill(win,0x000000);XCopyArea(d,pix,win,gc,0,0,W,H,0,0);check("core_copy_to_window",win,0);
    fill(pix,0x0000ff);XRenderFillRectangle(d,PictOpSrc,dst,&red,8,12,32,20);check("render_fill_src",pix,0);
    fill(pix,0x0000ff);XRenderComposite(d,PictOpSrc,solid,None,dst,0,0,0,0,8,12,32,20);check("render_solid_src",pix,0);
    Pixmap maskpix=XCreatePixmap(d,win,W,H,8);Picture mask=XRenderCreatePicture(d,maskpix,a8,0,NULL);GC mgc=XCreateGC(d,maskpix,0,NULL);
    XImage *mi=XCreateImage(d,DefaultVisual(d,screen),8,ZPixmap,0,NULL,W,H,32,0);if(!mi)return 2;mi->data=calloc(H,mi->bytes_per_line);if(!mi->data)return 2;
    const int alphas[4]={0,64,128,255};for(int y=0;y<H;y++)for(int x=0;x<W;x++)XPutPixel(mi,x,y,alphas[x/16]);
    XPutImage(d,maskpix,mgc,mi,0,0,0,0,W,H);XDestroyImage(mi);
    fill(pix,0x0000ff);XRenderComposite(d,PictOpOver,solid,mask,dst,0,0,0,0,0,0,W,H);expect_fill(0x0000ff);mask_expected(0,0,W,H);check("a8_putimage_over",pix,1);
    for(int i=0;i<4;i++){XRenderColor alpha={0,0,0,(unsigned short)(alphas[i]*257)};XRenderFillRectangle(d,PictOpSrc,mask,&alpha,i*16,0,16,H);}
    fill(pix,0x0000ff);XRenderComposite(d,PictOpOver,solid,mask,dst,0,0,0,0,0,0,W,H);check("a8_renderfill_over",pix,1);
    // Upload a deterministic 8x8 glyph: four vertical alpha stripes, no font dependency.
    GlyphSet glyphset=XRenderCreateGlyphSet(d,a8);Glyph glyph=1;XGlyphInfo gi={8,8,0,0,10,0};char glyphdata[64];for(int y=0;y<8;y++)for(int x=0;x<8;x++)glyphdata[y*8+x]=alphas[x/2];
    XRenderAddGlyphs(d,glyphset,&glyph,&gi,1,glyphdata,sizeof(glyphdata));
    fill(pix,0x0000ff);char text=1;XRenderCompositeString8(d,PictOpOver,solid,dst,a8,glyphset,0,0,10,12,&text,1);expect_fill(0x0000ff);mask_expected(10,12,8,8);check("a8_custom_glyph",pix,1);
    unsigned char bitmap[W*H/8];memset(bitmap,0,sizeof(bitmap));for(int y=0;y<H;y++)memset(bitmap+y*W/8,255,4);
    Pixmap bitmap_pix=XCreateBitmapFromData(d,win,(const char *)bitmap,W,H);
    Picture bitmap_mask=XRenderCreatePicture(d,bitmap_pix,XRenderFindStandardFormat(d,PictStandardA1),0,NULL);
    fill(pix,0x0000ff);XRenderComposite(d,PictOpOver,solid,bitmap_mask,dst,0,0,0,0,0,0,W,H);expect_fill(0x0000ff);expect_rect(0,0,32,H,0xff0000);check("a1_bitmap_mask",pix,0);
    XSetForeground(d,gc,0xff0000);XSetBackground(d,gc,0x0000ff);XCopyPlane(d,bitmap_pix,pix,gc,0,0,W,H,0,0,1);check("core_copyplane_bitmap",pix,0);
    // Linear red→blue gradient: pixel centers are (x+0.5)/64; tolerate integer rounding.
    XLinearGradient line={{0,0},{XDoubleToFixed(W),0}};XFixed stops[2]={0,XDoubleToFixed(1)};XRenderColor colors[2]={red,blue};Picture gradient=XRenderCreateLinearGradient(d,&line,stops,colors,2);
    fill(pix,0);XRenderComposite(d,PictOpSrc,gradient,None,dst,0,0,0,0,0,0,W,H);
    for(int y=0;y<H;y++)for(int x=0;x<W;x++){int b=(int)((x+0.5)*255/W+0.5);expect_rect(x,y,1,1,((255-b)<<16)|b);}check("linear_gradient",pix,2);
    // Core-font glyphs compared with the server's independent depth-one bitmap path.
    XFontStruct *font=XLoadQueryFont(d,"fixed");if(!font)return 2;XSetFont(d,gc,font->fid);
    Pixmap mono=XCreatePixmap(d,win,W,H,1);GC mono_gc=XCreateGC(d,mono,0,NULL);XSetForeground(d,mono_gc,0);XFillRectangle(d,mono,mono_gc,0,0,W,H);XSetForeground(d,mono_gc,1);XSetFont(d,mono_gc,font->fid);XDrawString(d,mono,mono_gc,8,24,"Hi",2);XSync(d,False);
    XImage *bits=XGetImage(d,mono,0,0,W,H,1,ZPixmap);expect_fill(0x0000ff);for(int y=0;y<H;y++)for(int x=0;x<W;x++)if(XGetPixel(bits,x,y))expect_rect(x,y,1,1,0xffffff);XDestroyImage(bits);unsigned char font_expected[sizeof(expected)];memcpy(font_expected,expected,sizeof(expected));
    fill(pix,0x0000ff);XSetForeground(d,gc,0xffffff);XDrawString(d,pix,gc,8,24,"Hi",2);check("core_text_vs_depth1",pix,0);
    fill(pix,0x0000ff);XSetForeground(d,gc,0xffffff);XSetBackground(d,gc,0x0000ff);XCopyPlane(d,mono,pix,gc,0,0,W,H,0,0,1);check("core_copyplane_font",pix,0);
    fill(pix,0x0000ff);XSetForeground(d,gc,0xffffff);XSetBackground(d,gc,0);XDrawImageString(d,pix,gc,8,24,"Hi",2);
    expect_fill(0x0000ff);expect_rect(8,24-font->ascent,XTextWidth(font,"Hi",2),font->ascent+font->descent,0);
    for(int y=0;y<H;y++)for(int x=0;x<W;x++)if(font_expected[(y*W+x)*3]==255)expect_rect(x,y,1,1,0xffffff);
    check("core_image_text_vs_depth1",pix,0);
    // GLX context is separately checked for virgl; triangle and scissor cases are independent of XRender.
    int attrs[]={GLX_RGBA,GLX_RED_SIZE,8,GLX_GREEN_SIZE,8,GLX_BLUE_SIZE,8,None};XVisualInfo *vi=glXChooseVisual(d,screen,attrs);if(!vi)return 2;
    XSetWindowAttributes wa={0};wa.colormap=XCreateColormap(d,RootWindow(d,screen),vi->visual,AllocNone);
    Window gw=XCreateWindow(d,RootWindow(d,screen),100,20,W,H,0,vi->depth,InputOutput,vi->visual,CWColormap,&wa);XMapWindow(d,gw);XSync(d,False);
    GLXContext ctx=glXCreateContext(d,vi,NULL,True);if(!ctx||!glXMakeCurrent(d,gw,ctx))return 2;
    const char *renderer=(const char *)glGetString(GL_RENDERER);printf("XRENDER_GL_RENDERER %s\n",renderer?renderer:"null");if(!renderer||!strstr(renderer,"virgl")||strstr(renderer,"llvmpipe"))return 2;
    glViewport(0,0,W,H);glClearColor(0,0,1,1);glClear(GL_COLOR_BUFFER_BIT);glColor3f(1,0,0);glBegin(GL_TRIANGLES);glVertex2f(-1,-1);glVertex2f(1,-1);glVertex2f(-1,1);glEnd();glFinish();
    // In X top-down coordinates the triangle covers x<y; diagonal centers lie
    // exactly on the edge, whose inclusion depends on rasterization fill rules.
    expect_fill(0x0000ff);for(int y=0;y<H;y++)for(int x=0;x<y;x++)expect_rect(x,y,1,1,0xff0000);
    exclude_diagonal=1;check("glx_triangle_readback",gw,0);exclude_diagonal=0;
    glEnable(GL_SCISSOR_TEST);glScissor(0,0,32,H);glClearColor(1,0,0,1);glClear(GL_COLOR_BUFFER_BIT);glScissor(32,0,32,H);glClearColor(0,0,1,1);glClear(GL_COLOR_BUFFER_BIT);glDisable(GL_SCISSOR_TEST);glFinish();
    unsigned char glpixel[4];glReadPixels(16,16,1,1,GL_RGBA,GL_UNSIGNED_BYTE,glpixel);if(glpixel[0]!=255||glpixel[1]!=0||glpixel[2]!=0)return 2;
    expect_fill(0x0000ff);expect_rect(0,0,32,H,0xff0000);check("glx_scissor_readback",gw,0);
    printf("XRENDER_SUMMARY {\"cases\":%d,\"failed\":%d,\"passed\":%s}\n",cases,failures,failures?"false":"true");fflush(stdout);
    XCloseDisplay(d);return failures?1:0;
}
