/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Development-only CLI adapter for the pinned official UTM QEMU dylib.
 * Does not patch/re-sign upstream binaries or relax macOS security settings.
 * Calling convention follows UTM v5.0.8 QEMULauncher/Bootstrap.c. */
#include <dlfcn.h>
#include <stdio.h>
#include "qemu-ram-reclaim.h"
extern char **environ;
int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: windows-utm-loader QEMU_DYLIB QEMU_ARGUMENTS...\n");
        return 2;
    }
    void *handle = dlopen(argv[1], RTLD_LOCAL | RTLD_LAZY);
    if (!handle) { fprintf(stderr, "%s\n", dlerror()); return 3; }
    int (*swtpm)(int, char **, const char *, const char *) = dlsym(handle, "swtpm_main");
    if (swtpm) return swtpm(argc - 1, argv + 1, "swtpm", "socket");
    int (*init)(int, char **, char **) = dlsym(handle, "qemu_init");
    void (*loop)(void) = dlsym(handle, "qemu_main_loop");
    void (*cleanup)(void) = dlsym(handle, "qemu_cleanup");
    if (!init || !loop || !cleanup) {
        fprintf(stderr, "QEMU lifecycle symbols missing\n");
        return 4;
    }
    qemu_ram_reclaim_install(handle);
    init(argc - 1, argv + 1, environ);
    loop();
    cleanup();
    return 0;
}
