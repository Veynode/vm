/* Minimal real-guest EGL/GLES pixel test. ABI subset avoids a full SDK in RAM.
 * Constants/types follow Khronos EGL 1.5 and OpenGL ES 3.0 public headers.
 * This is a correctness smoke test, not a frame-time benchmark. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef void *EGLDisplay; typedef void *EGLConfig; typedef void *EGLContext; typedef void *EGLSurface;
extern EGLDisplay eglGetDisplay(void *);
extern unsigned int eglInitialize(EGLDisplay,int *,int *);
extern unsigned int eglChooseConfig(EGLDisplay,const int *,EGLConfig *,int,int *);
extern unsigned int eglBindAPI(unsigned int);
extern EGLContext eglCreateContext(EGLDisplay,EGLConfig,EGLContext,const int *);
extern EGLSurface eglCreatePbufferSurface(EGLDisplay,EGLConfig,const int *);
extern unsigned int eglMakeCurrent(EGLDisplay,EGLSurface,EGLSurface,EGLContext);
extern unsigned int eglGetError(void);
extern const unsigned char *glGetString(unsigned int);
extern unsigned int glCreateShader(unsigned int);
extern void glShaderSource(unsigned int,int,const char *const *,const int *);
extern void glCompileShader(unsigned int);
extern void glGetShaderiv(unsigned int,unsigned int,int *);
extern void glGetShaderInfoLog(unsigned int,int,int *,char *);
extern unsigned int glCreateProgram(void);
extern void glAttachShader(unsigned int,unsigned int);
extern void glLinkProgram(unsigned int);
extern void glGetProgramiv(unsigned int,unsigned int,int *);
extern void glUseProgram(unsigned int);
extern void glViewport(int,int,int,int);
extern void glClearColor(float,float,float,float);
extern void glClear(unsigned int);
extern void glDrawArrays(unsigned int,int,int);
extern void glFinish(void);
extern void glReadPixels(int,int,int,int,unsigned int,unsigned int,void *);
extern unsigned int glGetError(void);
static _Noreturn void fail(const char *message) { fprintf(stderr,"GPU_TEST_FAIL %s egl=0x%x gl=0x%x\n",message,eglGetError(),glGetError()); exit(1); }
static unsigned int shader(unsigned int type,const char *source) {
    unsigned int s=glCreateShader(type); int ok=0; char log[1024];
    glShaderSource(s,1,&source,NULL); glCompileShader(s); glGetShaderiv(s,0x8B81,&ok);
    if(!ok){glGetShaderInfoLog(s,sizeof(log),NULL,log);fprintf(stderr,"%s\n",log);fail("shader compile");}
    return s;
}
int main(void) {
    EGLDisplay d=eglGetDisplay(NULL);int major,minor,n;EGLConfig cfg;
    const int attrs[]={0x3033,1,0x3040,0x40,0x3024,8,0x3023,8,0x3022,8,0x3021,8,0x3038};
    const int context[]={0x3098,3,0x3038},surface[]={0x3057,64,0x3056,64,0x3038};
    if(!eglInitialize(d,&major,&minor)||!eglChooseConfig(d,attrs,&cfg,1,&n)||n!=1||!eglBindAPI(0x30A0))fail("EGL configuration");
    EGLContext c=eglCreateContext(d,cfg,NULL,context);EGLSurface s=eglCreatePbufferSurface(d,cfg,surface);
    if(!c||!s||!eglMakeCurrent(d,s,s,c))fail("EGL context");
    const char *renderer=(const char *)glGetString(0x1F01);const char *version=(const char *)glGetString(0x1F02);
    printf("GPU_RENDERER %s\nGPU_VERSION %s\n",renderer,version);
    if(!renderer||!strstr(renderer,"virgl")||strstr(renderer,"llvmpipe")||strstr(renderer,"softpipe"))fail("not expected virgl renderer");
    const char *vs="#version 300 es\nconst vec2 p[3]=vec2[3](vec2(-0.8,-0.8),vec2(0.8,-0.8),vec2(0,0.8));void main(){gl_Position=vec4(p[gl_VertexID],0,1);}";
    const char *fs="#version 300 es\nprecision highp float;out vec4 color;void main(){color=vec4(1,0,0,1);}";
    unsigned int p=glCreateProgram();glAttachShader(p,shader(0x8B31,vs));glAttachShader(p,shader(0x8B30,fs));glLinkProgram(p);
    int ok=0;glGetProgramiv(p,0x8B82,&ok);if(!ok)fail("program link");glUseProgram(p);glViewport(0,0,64,64);
    for(int i=0;i<120;i++){glClearColor(0,0,1,1);glClear(0x4000);glDrawArrays(4,0,3);}
    glFinish();unsigned char center[4],corner[4];
    glReadPixels(32,32,1,1,0x1908,0x1401,center);glReadPixels(0,0,1,1,0x1908,0x1401,corner);
    if(glGetError()!=0)fail("render error");
    printf("GPU_PIXELS center=%u,%u,%u,%u corner=%u,%u,%u,%u\n",center[0],center[1],center[2],center[3],corner[0],corner[1],corner[2],corner[3]);
    if(center[0]!=255||center[1]||center[2]||center[3]!=255||corner[0]||corner[1]||corner[2]!=255||corner[3]!=255)fail("pixel mismatch");
    puts("GPU_TEST_PASS frames=120 size=64x64 sample=shader_triangle");return 0;
}
