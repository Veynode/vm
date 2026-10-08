/* Independent pixel diagnostic for operations used by Xorg 21.1.23 glamor fonts.
 * Reference: glamor_font.c (R8UI atlas) and glamor_text.c (instanced core text).
 * This is not an Xorg patch, a performance benchmark, or a driver acceptance test.
 */
#define GL_GLEXT_PROTOTYPES
#ifdef USE_GLES
#include <GLES3/gl3.h>
#include <EGL/egl.h>
#else
#include <GL/gl.h>
#include <GL/glext.h>
#include <GL/glx.h>
#include <X11/Xlib.h>
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#define W 64
#define H 64
static const char *directory;
static unsigned char expected[W*H*3];
static unsigned char atlas[16];
static unsigned cases, failures;
static GLuint expected_fbo, expected_texture;
static void error(const char *step) {
    GLenum e=glGetError();
    if(e!=GL_NO_ERROR){fprintf(stderr,"GLFONT_API_ERROR step=%s code=0x%x\n",step,e);exit(2);}
}
static void expected_color(int x,int y,unsigned color) {
    unsigned char *p=expected+(y*W+x)*3;p[0]=color>>16;p[1]=color>>8;p[2]=color;
}
static void background(void) {
    glClearColor(0,0,1,1);glClear(GL_COLOR_BUFFER_BIT);
    for(int y=0;y<H;y++)for(int x=0;x<W;x++)expected_color(x,y,0x0000ff);
}
static void ppm(const char *name,const char *suffix,const unsigned char *pixels) {
    char path[1024];snprintf(path,sizeof(path),"%s/%s%s.ppm",directory,name,suffix);
    FILE *f=fopen(path,"wb");if(!f)exit(2);fprintf(f,"P6\n%d %d\n255\n",W,H);
    /* GL readback/oracles are lower-left origin; exported PNG is top-first. */
    for(int y=H-1;y>=0;y--)fwrite(pixels+y*W*3,1,W*3,f);fclose(f);
}
static void check(const char *name) {
    unsigned char rgba[W*H*4],rgb[W*H*3];glFinish();glReadPixels(0,0,W,H,GL_RGBA,GL_UNSIGNED_BYTE,rgba);error(name);
    unsigned mismatches=0,max_error=0,first_x=0,first_y=0,got=0,want=0;
    for(int y=0;y<H;y++)for(int x=0;x<W;x++){
        size_t i=y*W+x;int bad=0;
        for(int c=0;c<3;c++){
            rgb[i*3+c]=rgba[i*4+c];unsigned diff=abs((int)rgb[i*3+c]-expected[i*3+c]);
            if(diff>max_error)max_error=diff;if(diff)bad=1;
        }
        if(bad){if(!mismatches){first_x=x;first_y=y;got=(rgb[i*3]<<16)|(rgb[i*3+1]<<8)|rgb[i*3+2];want=(expected[i*3]<<16)|(expected[i*3+1]<<8)|expected[i*3+2];}mismatches++;}
    }
    ppm(name,"",rgb);ppm(name,".expected",expected);cases++;failures+=!!mismatches;
    printf("XRENDER_CASE {\"name\":\"%s\",\"passed\":%s,\"mismatched_pixels\":%u,\"total_pixels\":4096,\"max_channel_error\":%u,\"first_x\":%u,\"first_y_gl_bottom\":%u,\"first_actual_rgb\":\"%06x\",\"first_expected_rgb\":\"%06x\"}\n",name,mismatches?"false":"true",mismatches,max_error,first_x,first_y,got,want);fflush(stdout);
}
static GLuint shader(GLenum type,const char *body) {
    GLuint s=glCreateShader(type);glShaderSource(s,1,&body,NULL);glCompileShader(s);
    GLint ok=0;glGetShaderiv(s,GL_COMPILE_STATUS,&ok);
    if(!ok){char log[4096];glGetShaderInfoLog(s,sizeof(log),NULL,log);fprintf(stderr,"GLFONT_SHADER_FAIL type=%u %s\n",type,log);exit(2);}return s;
}
static GLuint program(const char *vs,const char *fs) {
    GLuint p=glCreateProgram();glAttachShader(p,shader(GL_VERTEX_SHADER,vs));glAttachShader(p,shader(GL_FRAGMENT_SHADER,fs));
    glBindAttribLocation(p,2,"position");glBindAttribLocation(p,0,"primitive");glBindAttribLocation(p,1,"source");glLinkProgram(p);
    GLint ok=0;glGetProgramiv(p,GL_LINK_STATUS,&ok);if(!ok){char log[4096];glGetProgramInfoLog(p,sizeof(log),NULL,log);fprintf(stderr,"GLFONT_LINK_FAIL %s\n",log);exit(2);}return p;
}
#ifdef USE_GLES
#define VS_HEADER "#version 300 es\nprecision highp float;precision highp int;\n#define attribute in\n#define varying out\n"
#define FS_HEADER "#version 300 es\nprecision highp float;precision highp int;precision highp usampler2D;\n#define varying in\n#define texelFetch2D texelFetch\nout vec4 output_color;\n#define gl_FragColor output_color\n"
#else
#define VS_HEADER "#version 120\n#extension GL_EXT_gpu_shader4 : require\n"
#define FS_HEADER "#version 120\n#extension GL_EXT_gpu_shader4 : require\n#define uint unsigned int\n"
#endif
static const char *prefix_vs=
    VS_HEADER
    "attribute vec2 position;varying vec2 glyph_pos;void main(){gl_Position=vec4(position,0.0,1.0);glyph_pos=vec2(0.0);}\n";
static const char *instanced_vs=
    VS_HEADER
    "attribute vec4 primitive;attribute vec2 source;varying vec2 glyph_pos;\n"
    "void main(){vec2 corner=vec2(gl_VertexID & 1,(gl_VertexID & 2)>>1);\n"
    "vec2 delta=primitive.zw*corner;gl_Position=vec4((primitive.xy+delta)/32.0-1.0,0.0,1.0);glyph_pos=source+delta;}\n";
static const char *font_fs=
    FS_HEADER
    "uniform usampler2D atlas;uniform int mode;varying vec2 glyph_pos;\n"
    "void main(){if(mode==0){gl_FragColor=vec4(1.0,0.0,0.0,1.0);return;}\n"
    "ivec2 xy=ivec2(gl_FragCoord.xy);\n"
    "if(mode==1){uint v=texelFetch2D(atlas,ivec2(xy.x/32,xy.y/8),0).r;gl_FragColor=vec4(vec3(float(v)/255.0),1.0);return;}\n"
    "ivec2 pos=(mode==4)?ivec2(glyph_pos):ivec2(xy.x/4,xy.y/8);\n"
    "uint value=texelFetch2D(atlas,ivec2(pos.x>>3,pos.y),0).r;\n"
    "uint shift=uint(pos.x & 7);uint result=0u;\n"
    "if(mode==5)result=value;\n"
    "if(mode==6)result=value>>2u;\n"
    "if(mode==7)result=128u>>shift;\n"
    "if(mode==8)result=uint(128>>int(shift));\n"
    "if(mode==9)result=1u<<shift;\n"
    "if(mode==10)result=(value&1u)*255u;\n"
    "if(mode==11)result=((value>>shift)&1u)*255u;\n"
    "if(mode==12)result=uint((int(value)>>int(shift))&1)*255u;\n"
    "if(mode==13)result=((value&(1u<<shift))!=0u)?255u:0u;\n"
    "if(mode==14)result=uint(mod(floor(float(value)/exp2(float(shift))),2.0))*255u;\n"
    "if(mode==15)result=uint(pos.x);\n"
    "if(mode>=5){gl_FragColor=vec4(vec3(float(result)/255.0),1.0);return;}\n"
    "uint bit=(value >> (pos.x & 7)) & 1u;\n"
    "if(mode>=3 && bit==0u)discard;\n"
    "gl_FragColor=(bit!=0u)?vec4(1.0,0.0,0.0,1.0):vec4(0.0,0.0,1.0,1.0);}\n";
static void use(GLuint p,int mode) {
    GLint mode_location=glGetUniformLocation(p,"mode"),sampler_location=glGetUniformLocation(p,"atlas");
    glUseProgram(p);glUniform1i(sampler_location,0);glUniform1i(mode_location,mode);
    GLint actual_program=0,actual_mode=-999,sampler=-999,unit=0,texture=0,fbo=0,viewport[4];
    glGetIntegerv(GL_CURRENT_PROGRAM,&actual_program);if(mode_location>=0)glGetUniformiv(p,mode_location,&actual_mode);
    if(sampler_location>=0)glGetUniformiv(p,sampler_location,&sampler);
    glGetIntegerv(GL_ACTIVE_TEXTURE,&unit);glGetIntegerv(GL_TEXTURE_BINDING_2D,&texture);glGetIntegerv(GL_FRAMEBUFFER_BINDING,&fbo);glGetIntegerv(GL_VIEWPORT,viewport);
    error("program/uniform/framebuffer state");
    printf("GLFONT_STATE program=%u actual_program=%d requested_mode=%d mode_location=%d actual_mode=%d sampler_location=%d sampler=%d texture_unit=%d texture=%d fbo=%d viewport=%d,%d,%d,%d\n",p,actual_program,mode,mode_location,actual_mode,sampler_location,sampler,unit,texture,fbo,viewport[0],viewport[1],viewport[2],viewport[3]);
    if((GLuint)actual_program!=p||(mode_location>=0&&actual_mode!=mode)||(sampler_location>=0&&sampler!=0)||unit!=GL_TEXTURE0||(GLuint)texture!=expected_texture||(GLuint)fbo!=expected_fbo||viewport[0]||viewport[1]||viewport[2]!=W||viewport[3]!=H||glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE){fprintf(stderr,"GLFONT_STATE_MISMATCH\n");exit(2);}
}
static void quad(void) {
    const GLfloat vertices[]={-1,-1,1,-1,-1,1,1,1};GLuint b;
    glGenBuffers(1,&b);glBindBuffer(GL_ARRAY_BUFFER,b);glBufferData(GL_ARRAY_BUFFER,sizeof(vertices),vertices,GL_STREAM_DRAW);
    glEnableVertexAttribArray(2);glVertexAttribPointer(2,2,GL_FLOAT,GL_FALSE,0,(const void *)0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);
    glDisableVertexAttribArray(2);glBindBuffer(GL_ARRAY_BUFFER,0);glDeleteBuffers(1,&b);
}
static void glyph_oracle(int x,int y,int atlas_bit_x) {
    for(int j=0;j<8;j++)for(int i=0;i<8;i++)if((atlas[j*2+(atlas_bit_x+i)/8]>>((atlas_bit_x+i)&7))&1)expected_color(x+i,y+j,0xff0000);
}
static void attributes(GLuint buffer,GLenum type,const void *data,size_t size,int stride,int source_offset) {
    glBindBuffer(GL_ARRAY_BUFFER,buffer);glBufferData(GL_ARRAY_BUFFER,size,data,GL_STREAM_DRAW);
    glEnableVertexAttribArray(0);glEnableVertexAttribArray(1);
    glVertexAttribPointer(0,4,type,GL_FALSE,stride,(const void *)0);glVertexAttribPointer(1,2,type,GL_FALSE,stride,(const void *)(uintptr_t)source_offset);
    glVertexAttribDivisor(0,1);glVertexAttribDivisor(1,1);
}
#ifdef POINTS_ONLY
static int points_probe(void) {
    glDisable(GL_BLEND);glDisable(GL_DEPTH_TEST);glDisable(GL_CULL_FACE);glDisable(GL_DITHER);glViewport(0,0,W,H);
    GLuint fbo,color,buffer;glGenFramebuffers(1,&fbo);glBindFramebuffer(GL_FRAMEBUFFER,fbo);
    glGenTextures(1,&color);glBindTexture(GL_TEXTURE_2D,color);glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA8,W,H,0,GL_RGBA,GL_UNSIGNED_BYTE,NULL);
    glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,color,0);
    expected_fbo=fbo;expected_texture=color;
    const char *vs_default="#version 120\nattribute vec2 primitive;void main(){gl_Position=vec4((primitive+vec2(0.5))/32.0-1.0,0.0,1.0);}";
    const char *vs_explicit="#version 120\nattribute vec2 primitive;void main(){gl_Position=vec4((primitive+vec2(0.5))/32.0-1.0,0.0,1.0);gl_PointSize=1.0;}";
    const char *fs="#version 120\nvoid main(){gl_FragColor=vec4(1.0,0.0,0.0,1.0);}";
    GLuint p_default=program(vs_default,fs),p_explicit=program(vs_explicit,fs);
    const GLshort points[]={8,8,9,8,10,8,8,9,10,9,8,10,9,10,10,10,28,36,30,38,32,40,48,16};
    GLfloat float_points[sizeof(points)/sizeof(*points)];for(unsigned i=0;i<sizeof(points)/sizeof(*points);i++)float_points[i]=points[i];
    glGenBuffers(1,&buffer);glBindBuffer(GL_ARRAY_BUFFER,buffer);glEnableVertexAttribArray(0);
    const char *names[]={"points_default_state_short","points_fixed_one_short","points_fixed_shader_one_short","points_program_shader_one_short","points_program_shader_one_float"};
    for(int mode=0;mode<5;mode++){
        if(mode==1){glDisable(GL_VERTEX_PROGRAM_POINT_SIZE);glPointSize(1.0f);}
        if(mode==3)glEnable(GL_VERTEX_PROGRAM_POINT_SIZE);
        GLfloat size=0,range[2];glGetFloatv(GL_POINT_SIZE,&size);glGetFloatv(GL_ALIASED_POINT_SIZE_RANGE,range);
        GLboolean enabled=glIsEnabled(GL_VERTEX_PROGRAM_POINT_SIZE);
        printf("GLFONT_POINT_STATE case=%s fixed_size=%.1f program_point_size=%d range=%.1f,%.1f\n",names[mode],size,enabled,range[0],range[1]);
        if(size!=1.0f||enabled!=(mode>=3)){fprintf(stderr,"GLFONT_UNEXPECTED_POINT_STATE\n");return 2;}
        background();use(mode>=2?p_explicit:p_default,0);
        GLenum type=mode==4?GL_FLOAT:GL_SHORT;
        glBufferData(GL_ARRAY_BUFFER,mode==4?sizeof(float_points):sizeof(points),mode==4?(const void *)float_points:(const void *)points,GL_STREAM_DRAW);
        glVertexAttribPointer(0,2,type,GL_FALSE,0,(const void *)0);
        GLint actual_type=0,actual_size=0,actual_buffer=0;glGetVertexAttribiv(0,GL_VERTEX_ATTRIB_ARRAY_TYPE,&actual_type);glGetVertexAttribiv(0,GL_VERTEX_ATTRIB_ARRAY_SIZE,&actual_size);glGetVertexAttribiv(0,GL_VERTEX_ATTRIB_ARRAY_BUFFER_BINDING,&actual_buffer);
        if(actual_type!=(GLint)type||actual_size!=2||actual_buffer!=(GLint)buffer)return 2;
        glDrawArrays(GL_POINTS,0,sizeof(points)/sizeof(*points)/2);
        for(unsigned i=0;i<sizeof(points)/sizeof(*points);i+=2)expected_color(points[i],points[i+1],0xff0000);
        check(names[mode]);
    }
    printf("XRENDER_SUMMARY {\"cases\":%u,\"failed\":%u,\"passed\":%s}\n",cases,failures,failures?"false":"true");fflush(stdout);
    return failures?1:0;
}
#endif
int main(int argc,char **argv) {
    if(argc!=2)return 2;
    directory=argv[1];
#ifdef USE_GLES
    EGLDisplay ed=eglGetDisplay(EGL_DEFAULT_DISPLAY);EGLint major,minor,count;EGLConfig config;
    const EGLint config_attrs[]={EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES3_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_ALPHA_SIZE,8,EGL_NONE};
    const EGLint context_attrs[]={EGL_CONTEXT_CLIENT_VERSION,3,EGL_NONE},surface_attrs[]={EGL_WIDTH,W,EGL_HEIGHT,H,EGL_NONE};
    if(!eglInitialize(ed,&major,&minor)||!eglChooseConfig(ed,config_attrs,&config,1,&count)||count!=1||!eglBindAPI(EGL_OPENGL_ES_API))return 2;
    EGLContext ec=eglCreateContext(ed,config,EGL_NO_CONTEXT,context_attrs);EGLSurface es=eglCreatePbufferSurface(ed,config,surface_attrs);
    if(ec==EGL_NO_CONTEXT||es==EGL_NO_SURFACE||!eglMakeCurrent(ed,es,es,ec))return 2;
#else
    Display *d=XOpenDisplay(NULL);if(!d)return 2;int screen=DefaultScreen(d);
    int attr[]={GLX_RGBA,GLX_RED_SIZE,8,GLX_GREEN_SIZE,8,GLX_BLUE_SIZE,8,None};XVisualInfo *vi=glXChooseVisual(d,screen,attr);if(!vi)return 2;
    XSetWindowAttributes wa={0};wa.colormap=XCreateColormap(d,RootWindow(d,screen),vi->visual,AllocNone);
    Window win=XCreateWindow(d,RootWindow(d,screen),0,0,W,H,0,vi->depth,InputOutput,vi->visual,CWColormap,&wa);XMapWindow(d,win);XSync(d,False);
    GLXContext context=glXCreateContext(d,vi,NULL,True);if(!context||!glXMakeCurrent(d,win,context))return 2;
 #endif
    const char *renderer=(const char *)glGetString(GL_RENDERER),*extensions=(const char *)glGetString(GL_EXTENSIONS);
    printf("GLFONT_CONTEXT renderer=%s version=%s glsl=%s gpu_shader4=%d instanced_arrays=%d\n",renderer,(const char *)glGetString(GL_VERSION),(const char *)glGetString(GL_SHADING_LANGUAGE_VERSION),!!strstr(extensions,"GL_EXT_gpu_shader4"),!!strstr(extensions,"GL_ARB_instanced_arrays"));
    if(!renderer||!strstr(renderer,"virgl")||strstr(renderer,"llvmpipe"))return 2;
#ifdef POINTS_ONLY
    int points_result=points_probe();glXMakeCurrent(d,None,NULL);glXDestroyContext(d,context);XCloseDisplay(d);return points_result;
#endif
#ifndef USE_GLES
    if(!strstr(extensions,"GL_EXT_gpu_shader4")||!strstr(extensions,"GL_ARB_instanced_arrays")){fprintf(stderr,"GLFONT_REQUIRED_EXTENSION_MISSING\n");return 2;}
#endif
    glDisable(GL_BLEND);glDisable(GL_DEPTH_TEST);glDisable(GL_CULL_FACE);glDisable(GL_DITHER);glViewport(0,0,W,H);
    GLuint fbo,color,texture,vbo;glGenFramebuffers(1,&fbo);glBindFramebuffer(GL_FRAMEBUFFER,fbo);glGenTextures(1,&color);glBindTexture(GL_TEXTURE_2D,color);glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA8,W,H,0,GL_RGBA,GL_UNSIGNED_BYTE,NULL);glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,color,0);
    if(glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE)return 2;
    glGenTextures(1,&texture);glBindTexture(GL_TEXTURE_2D,texture);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_NEAREST);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_NEAREST);glPixelStorei(GL_UNPACK_ALIGNMENT,1);glPixelStorei(GL_PACK_ALIGNMENT,1);
    for(int y=0;y<8;y++){atlas[y*2]=(unsigned char)(1u<<y);atlas[y*2+1]=(unsigned char)(255^(1u<<y));}
    glTexImage2D(GL_TEXTURE_2D,0,GL_R8UI,2,8,0,GL_RED_INTEGER,GL_UNSIGNED_BYTE,atlas);error("R8UI upload");
    unsigned char fetched[16];
#ifdef USE_GLES
    GLuint atlas_fbo;GLuint integer_pixels[16*4];glGenFramebuffers(1,&atlas_fbo);glBindFramebuffer(GL_FRAMEBUFFER,atlas_fbo);glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,texture,0);
    if(glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE)return 2;
    glReadPixels(0,0,2,8,GL_RGBA_INTEGER,GL_UNSIGNED_INT,integer_pixels);for(int i=0;i<16;i++)fetched[i]=integer_pixels[i*4];
    glBindFramebuffer(GL_FRAMEBUFFER,fbo);glDeleteFramebuffers(1,&atlas_fbo);
#else
    glGetTexImage(GL_TEXTURE_2D,0,GL_RED_INTEGER,GL_UNSIGNED_BYTE,fetched);
#endif
    error("R8UI readback");unsigned bad=0;for(int i=0;i<16;i++)bad+=fetched[i]!=atlas[i];cases++;failures+=!!bad;
    printf("XRENDER_CASE {\"name\":\"r8ui_upload_readback\",\"passed\":%s,\"mismatched_texels\":%u,\"total_texels\":16}\n",bad?"false":"true",bad);
    expected_fbo=fbo;expected_texture=texture;
    GLuint plain=program(prefix_vs,font_fs),instanced=program(instanced_vs,font_fs);glGenBuffers(1,&vbo);
    background();use(plain,1);quad();for(int y=0;y<H;y++)for(int x=0;x<W;x++){unsigned v=atlas[(y/8)*2+x/32];expected_color(x,y,(v<<16)|(v<<8)|v);}check("r8ui_texel_fetch");
    for(int mode=2;mode<=3;mode++){
        background();use(plain,mode);quad();for(int y=0;y<H;y++)for(int x=0;x<W;x++)if((atlas[(y/8)*2+(x/4)/8]>>((x/4)&7))&1)expected_color(x,y,0xff0000);
        check(mode==2?"r8ui_bit_shift":"r8ui_bit_discard");
    }
    const char *isolated_expr[]={"value","value>>2u","128u>>shift","uint(128>>int(shift))","1u<<shift","(value&1u)*255u","((value>>shift)&1u)*255u","uint((int(value)>>int(shift))&1)*255u","((value&(1u<<shift))!=0u)?255u:0u","uint(mod(floor(float(value)/exp2(float(shift))),2.0))*255u","uint(pos.x)"};
    const char *arithmetic_names[]={"same_coords_texel_fetch","uint_constant_right_shift","uint_dynamic_right_shift","int_dynamic_right_shift","uint_dynamic_left_shift","uint_and_one","uint_shift_and_one","int_shift_and_one","uint_left_mask_equivalent","float_arithmetic_bit","integer_coords"};
    for(int isolated=0;isolated<=1;isolated++)for(int mode=5;mode<=15;mode++){
        GLuint active=plain;
        if(isolated){
            char fs[4096];snprintf(fs,sizeof(fs),FS_HEADER
                "uniform usampler2D atlas;varying vec2 glyph_pos;void main(){"
                "ivec2 xy=ivec2(gl_FragCoord.xy);ivec2 pos=ivec2(xy.x/4,xy.y/8);"
                "uint value=texelFetch2D(atlas,ivec2(pos.x>>3,pos.y),0).r;uint shift=uint(pos.x&7);"
                "uint result=%s;gl_FragColor=vec4(vec3(float(result)/255.0),1.0);}",isolated_expr[mode-5]);
            active=program(prefix_vs,fs);
        }
        background();use(active,mode);quad();
        for(int y=0;y<H;y++)for(int x=0;x<W;x++){
            unsigned pos=x/4,shift=pos&7,value=atlas[(y/8)*2+pos/8],v=0;
            switch(mode){
                case 5:v=value;break;case 6:v=value>>2;break;case 7:case 8:v=128>>shift;break;
                case 9:v=1u<<shift;break;case 10:v=(value&1)*255;break;
                case 11:case 12:case 13:case 14:v=((value>>shift)&1)*255;break;case 15:v=pos;break;
            }
            expected_color(x,y,(v<<16)|(v<<8)|v);
        }
        char name[128];snprintf(name,sizeof(name),"%s%s",isolated?"isolated_":"",arithmetic_names[mode-5]);check(name);
        if(isolated)glDeleteProgram(active);
    }
    background();use(instanced,0);glVertexAttrib4f(0,8,12,24,16);glVertexAttrib2f(1,0,0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);
    for(int y=12;y<28;y++)for(int x=8;x<32;x++)expected_color(x,y,0xff0000);check("vertex_id_rectangle");
    const GLshort shorts[]={4,4,8,8,0,0,28,36,8,8,8,0};const GLfloat floats[]={4,4,8,8,0,0,28,36,8,8,8,0};
    background();use(instanced,0);attributes(vbo,GL_SHORT,shorts,sizeof(shorts),12,8);glDrawArraysInstanced(GL_TRIANGLE_STRIP,0,4,2);
    for(int y=0;y<8;y++)for(int x=0;x<8;x++){expected_color(4+x,4+y,0xff0000);expected_color(28+x,36+y,0xff0000);}check("instanced_short_rectangles");
    background();use(instanced,4);glDrawArraysInstanced(GL_TRIANGLE_STRIP,0,4,2);glyph_oracle(4,4,0);glyph_oracle(28,36,8);check("instanced_short_integer_glyphs");
    background();attributes(vbo,GL_FLOAT,floats,sizeof(floats),24,16);glDrawArraysInstanced(GL_TRIANGLE_STRIP,0,4,2);glyph_oracle(4,4,0);glyph_oracle(28,36,8);check("instanced_float_integer_glyphs");
    glVertexAttribDivisor(0,0);glVertexAttribDivisor(1,0);glDisableVertexAttribArray(0);glDisableVertexAttribArray(1);glBindBuffer(GL_ARRAY_BUFFER,0);
    background();glVertexAttrib4f(0,4,4,8,8);glVertexAttrib2f(1,0,0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);glVertexAttrib4f(0,28,36,8,8);glVertexAttrib2f(1,8,0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);glyph_oracle(4,4,0);glyph_oracle(28,36,8);check("separate_draw_integer_glyphs");
    const char *single_font_fs=FS_HEADER
        "uniform usampler2D atlas;varying vec2 glyph_pos;void main(){"
        "ivec2 pos=ivec2(glyph_pos);uint value=texelFetch2D(atlas,ivec2(pos.x>>3,pos.y),0).r;"
        "uint bit=(value>>uint(pos.x&7))&1u;if(bit==0u)discard;gl_FragColor=vec4(1.0,0.0,0.0,1.0);}";
    GLuint single=program(instanced_vs,single_font_fs);
    const char *single_names[]={"single_shader_short_glyphs","single_shader_float_glyphs","single_shader_separate_glyphs"};
    for(int mode=0;mode<3;mode++){
        background();use(single,4);
        if(mode<2){
            if(mode==0)attributes(vbo,GL_SHORT,shorts,sizeof(shorts),12,8);else attributes(vbo,GL_FLOAT,floats,sizeof(floats),24,16);
            glDrawArraysInstanced(GL_TRIANGLE_STRIP,0,4,2);
        }else{
            glVertexAttribDivisor(0,0);glVertexAttribDivisor(1,0);glDisableVertexAttribArray(0);glDisableVertexAttribArray(1);glBindBuffer(GL_ARRAY_BUFFER,0);
            glVertexAttrib4f(0,4,4,8,8);glVertexAttrib2f(1,0,0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);
            glVertexAttrib4f(0,28,36,8,8);glVertexAttrib2f(1,8,0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);
        }
        glyph_oracle(4,4,0);glyph_oracle(28,36,8);check(single_names[mode]);
    }
    printf("XRENDER_SUMMARY {\"cases\":%u,\"failed\":%u,\"passed\":%s}\n",cases,failures,failures?"false":"true");fflush(stdout);
#ifdef USE_GLES
    eglMakeCurrent(ed,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);eglDestroySurface(ed,es);eglDestroyContext(ed,ec);eglTerminate(ed);
#else
    glXMakeCurrent(d,None,NULL);glXDestroyContext(d,context);XCloseDisplay(d);
#endif
    return failures?1:0;
}
