/* Small real OpenGL core-profile raster/readback oracle on an X11 window
 * (GLX). Usage: linux-gl-triangle [MAJOR.MINOR] [SECONDS]
 * Requests a core context of MAJOR.MINOR (default 3.3), draws two instanced
 * quads through a uniform block into an FBO and into the window, checks known
 * pixels in both, then redraws and swaps for SECONDS (default 5) and reports
 * the rate. Build: cc -O2 -o linux-gl-triangle linux-gl-triangle.c -lGL -lX11
 */
#define GL_GLEXT_PROTOTYPES 1
#include <GL/gl.h>
#include <GL/glext.h>
#include <GL/glx.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define W 256
#define H 256

typedef GLXContext (*CreateContextAttribs)(Display *, GLXFBConfig, GLXContext,
                                           Bool, const int *);

static void fail(const char *what) {
  fprintf(stderr, "GL_PIXEL_FAIL %s glError=0x%x\n", what, glGetError());
  exit(1);
}

static GLuint compile(GLenum type, const char *src) {
  GLuint s = glCreateShader(type);
  glShaderSource(s, 1, &src, NULL);
  glCompileShader(s);
  GLint ok = 0;
  glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
  if (!ok) {
    char log[1024];
    glGetShaderInfoLog(s, sizeof log, NULL, log);
    fprintf(stderr, "GL_PIXEL_FAIL compile: %s\n", log);
    exit(1);
  }
  return s;
}

/* Clear red; instance 0 is a green quad on the left half, instance 1 a blue
 * quad on the right half, both inset by the offset from the uniform block. */
static void draw(void) {
  glClearColor(1, 0, 0, 1);
  glClear(GL_COLOR_BUFFER_BIT);
  glDrawArraysInstanced(GL_TRIANGLE_STRIP, 0, 4, 2);
}

static int near(const unsigned char *p, int r, int g, int b) {
  return abs(p[0] - r) <= 2 && abs(p[1] - g) <= 2 && abs(p[2] - b) <= 2;
}

/* Returns the number of mismatching probe points in the bound framebuffer. */
static int check(void) {
  static const struct {
    int x, y, r, g, b;
  } probe[] = {
      {64, 128, 0, 255, 0},  {32, 200, 0, 255, 0},  {192, 128, 0, 0, 255},
      {224, 56, 0, 0, 255},  {128, 128, 255, 0, 0}, {64, 4, 255, 0, 0},
      {192, 251, 255, 0, 0},
  };
  unsigned char px[W * H * 4];
  glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
  int bad = 0;
  for (size_t i = 0; i < sizeof probe / sizeof probe[0]; i++) {
    const unsigned char *p = px + (probe[i].y * W + probe[i].x) * 4;
    if (!near(p, probe[i].r, probe[i].g, probe[i].b)) {
      fprintf(stderr, "mismatch at %d,%d: %d %d %d\n", probe[i].x, probe[i].y,
              p[0], p[1], p[2]);
      bad++;
    }
  }
  return bad;
}

static double now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
  int major = 3, minor = 3;
  if (argc > 1 && sscanf(argv[1], "%d.%d", &major, &minor) != 2)
    fail("usage");
  double seconds = argc > 2 ? atof(argv[2]) : 5;
  Display *dpy = XOpenDisplay(NULL);
  if (!dpy)
    fail("XOpenDisplay");
  static const int fb_attrs[] = {GLX_X_RENDERABLE, True, GLX_DRAWABLE_TYPE,
                                 GLX_WINDOW_BIT, GLX_RENDER_TYPE, GLX_RGBA_BIT,
                                 GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8,
                                 GLX_BLUE_SIZE, 8, GLX_DOUBLEBUFFER, True, None};
  int n = 0;
  GLXFBConfig *fbc = glXChooseFBConfig(dpy, DefaultScreen(dpy), fb_attrs, &n);
  if (!fbc || n == 0)
    fail("glXChooseFBConfig");
  XVisualInfo *vi = glXGetVisualFromFBConfig(dpy, fbc[0]);
  Window root = RootWindow(dpy, vi->screen);
  XSetWindowAttributes swa = {
      .colormap = XCreateColormap(dpy, root, vi->visual, AllocNone)};
  Window win = XCreateWindow(dpy, root, 0, 0, W, H, 0, vi->depth, InputOutput,
                             vi->visual, CWColormap, &swa);
  XStoreName(dpy, win, "linux-gl-triangle");
  XMapWindow(dpy, win);
  XSync(dpy, False);
  CreateContextAttribs create = (CreateContextAttribs)glXGetProcAddressARB(
      (const GLubyte *)"glXCreateContextAttribsARB");
  int ctx_attrs[] = {GLX_CONTEXT_MAJOR_VERSION_ARB, major,
                     GLX_CONTEXT_MINOR_VERSION_ARB, minor,
                     GLX_CONTEXT_PROFILE_MASK_ARB,
                     GLX_CONTEXT_CORE_PROFILE_BIT_ARB, None};
  GLXContext ctx = create ? create(dpy, fbc[0], NULL, True, ctx_attrs) : NULL;
  if (!ctx || !glXMakeCurrent(dpy, win, ctx))
    fail("core context");
  printf("renderer=%s\nversion=%s\nglsl=%s\n", glGetString(GL_RENDERER),
         glGetString(GL_VERSION), glGetString(GL_SHADING_LANGUAGE_VERSION));

  int v330 = major * 10 + minor >= 33;
  const char *header = v330 ? "#version 330 core\n"
                            : "#version 140\n#extension "
                              "GL_ARB_explicit_attrib_location : require\n";
  char vs[1024], fs[512];
  snprintf(vs, sizeof vs,
           "%s"
           "layout(location = 0) in vec2 corner;\n"
           "layout(std140) uniform Inset { vec4 inset; };\n"
           "out vec4 color;\n"
           "void main() {\n"
           "  float left = gl_InstanceID == 0 ? -1.0 : 0.0;\n"
           "  vec2 p = vec2(left + inset.x + corner.x * (1.0 - 2.0 * inset.x),\n"
           "                -1.0 + inset.y + corner.y * (2.0 - 2.0 * inset.y));\n"
           "  color = gl_InstanceID == 0 ? vec4(0, 1, 0, 1) : vec4(0, 0, 1, 1);\n"
           "  gl_Position = vec4(p, 0.0, 1.0);\n"
           "}\n",
           header);
  snprintf(fs, sizeof fs,
           "%s"
           "in vec4 color;\n"
           "layout(location = 0) out vec4 frag;\n"
           "void main() { frag = color; }\n",
           header);
  GLuint prog = glCreateProgram();
  glAttachShader(prog, compile(GL_VERTEX_SHADER, vs));
  glAttachShader(prog, compile(GL_FRAGMENT_SHADER, fs));
  glLinkProgram(prog);
  GLint linked = 0;
  glGetProgramiv(prog, GL_LINK_STATUS, &linked);
  if (!linked)
    fail("link");
  glUseProgram(prog);
  /* Inset of 1/16 of the viewport on every edge of each half. */
  static const float inset[4] = {0.125f, 0.125f, 0, 0};
  GLuint ubo;
  glGenBuffers(1, &ubo);
  glBindBuffer(GL_UNIFORM_BUFFER, ubo);
  glBufferData(GL_UNIFORM_BUFFER, sizeof inset, inset, GL_STATIC_DRAW);
  glUniformBlockBinding(prog, glGetUniformBlockIndex(prog, "Inset"), 0);
  glBindBufferBase(GL_UNIFORM_BUFFER, 0, ubo);
  static const float corners[] = {0, 0, 1, 0, 0, 1, 1, 1};
  GLuint vao, vbo;
  glGenVertexArrays(1, &vao);
  glBindVertexArray(vao);
  glGenBuffers(1, &vbo);
  glBindBuffer(GL_ARRAY_BUFFER, vbo);
  glBufferData(GL_ARRAY_BUFFER, sizeof corners, corners, GL_STATIC_DRAW);
  glEnableVertexAttribArray(0);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
  glViewport(0, 0, W, H);

  GLuint tex, fbo;
  glGenTextures(1, &tex);
  glBindTexture(GL_TEXTURE_2D, tex);
  glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE,
               NULL);
  glGenFramebuffers(1, &fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, fbo);
  glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                         tex, 0);
  if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
    fail("framebuffer");
  draw();
  int fbo_bad = check();
  glBindFramebuffer(GL_FRAMEBUFFER, 0);
  int win_bad = 0;
  for (int i = 0; i < 3; i++) {
    draw();
    glFinish();
    win_bad += check();
    glXSwapBuffers(dpy, win);
  }
  if (glGetError() != GL_NO_ERROR)
    fail("glGetError");

  int frames = 0;
  double start = now();
  while (now() - start < seconds) {
    draw();
    glXSwapBuffers(dpy, win);
    frames++;
  }
  glFinish();
  double elapsed = now() - start;
  printf("%s fbo_mismatches=%d window_mismatches=%d frames=%d fps=%.0f\n",
         fbo_bad || win_bad ? "GL_PIXEL_FAIL" : "GL_PIXEL_OK", fbo_bad, win_bad,
         frames, frames / elapsed);
  return fbo_bad || win_bad;
}
