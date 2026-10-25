/* Dedicated diagnostic loader: select one exact independent virgl host image.
 * QEMU and vendor frameworks remain unchanged. No substitute API implementation. */
#include <dlfcn.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern char **environ;
static int verified(void *qemu, void *handle, const char *symbol, const char *needle) {
    void *expected = dlsym(handle, symbol), *bound = dlsym(qemu, symbol);
    unsigned count = 0;
    for (uint32_t i = 0; i < _dyld_image_count(); i++) {
        if (strstr(_dyld_get_image_name(i), needle)) count++;
    }
    if (!expected || expected != bound || count != 1) return 0;
    Dl_info info = {0};
    if (!dladdr(bound, &info)) return 0;
    fprintf(stderr, "VEYNODE_BOUND_LIBRARY %s %s count=%u\n", symbol, info.dli_fname, count);
    return 1;
}
int main(int argc, char **argv) {
    if (argc < 3) return 2;
    const char *path = getenv("VEYNODE_VIRGL_LIBRARY");
    if (!path || path[0] != '/') return 2;
    void *virgl = dlopen(path, RTLD_LOCAL | RTLD_NOW);
    if (!virgl) { fprintf(stderr, "virgl dlopen failed: %s\n", dlerror()); return 3; }
    const char *spice_path = getenv("VEYNODE_SPICE_LIBRARY");
    void *spice = spice_path ? dlopen(spice_path, RTLD_LOCAL | RTLD_NOW) : NULL;
    if (spice_path && !spice) return 3;
    void *qemu = dlopen(argv[1], RTLD_LOCAL | RTLD_NOW);
    if (!qemu) { fprintf(stderr, "qemu dlopen failed: %s\n", dlerror()); return 3; }
    if (!verified(qemu, virgl, "virgl_renderer_init", "virglrenderer")) return 4;
    if (spice && !verified(qemu, spice, "spice_server_new", "spice-server")) return 4;
    int (*init)(int, char **, char **) = dlsym(qemu, "qemu_init");
    void (*loop)(void) = dlsym(qemu, "qemu_main_loop");
    void (*cleanup)(void) = dlsym(qemu, "qemu_cleanup");
    if (!init || !loop || !cleanup) return 5;
    init(argc-1, argv+1, environ); loop(); cleanup(); return 0;
}
