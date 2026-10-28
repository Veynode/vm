/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Built twice by test-qemu-ram-reclaim.sh: with QRR_TEST_IMAGE as a stand-in for
 * the QEMU image (it imports madvise and hv_vm_*, and exports qemu_init), and
 * without it as the loader that installs qemu-ram-reclaim.h over that image. */
#include <Hypervisor/Hypervisor.h>
#include <stdint.h>
#include <sys/mman.h>
#ifdef QRR_TEST_IMAGE
int qemu_init(void) { return hv_vm_create(NULL); }
int image_map(void *host, uint64_t gpa, size_t size) { return hv_vm_map(host, gpa, size, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC); }
int image_unmap(uint64_t gpa, size_t size) { return hv_vm_unmap(gpa, size); }
int image_protect(uint64_t gpa, size_t size) { return hv_vm_protect(gpa, size, HV_MEMORY_READ | HV_MEMORY_WRITE); }
int image_discard(void *host, size_t size) { return madvise(host, size, MADV_DONTNEED); }
#else
#include "qemu-ram-reclaim.h"

static int failures;
#define CHECK(cond) do { if (!(cond)) { fprintf(stderr, "FAIL %s:%d %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static uint64_t footprint(void) {
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count);
    return info.phys_footprint;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    void *image = dlopen(argv[1], RTLD_LOCAL | RTLD_NOW);
    if (!image) { fprintf(stderr, "%s\n", dlerror()); return 2; }
    int (*init)(void) = dlsym(image, "qemu_init");
    int (*map)(void *, uint64_t, size_t) = dlsym(image, "image_map");
    int (*unmap)(uint64_t, size_t) = dlsym(image, "image_unmap");
    int (*protect)(uint64_t, size_t) = dlsym(image, "image_protect");
    int (*discard)(void *, size_t) = dlsym(image, "image_discard");
    CHECK(qemu_ram_reclaim_install(image) == 0);
    CHECK(init() == HV_SUCCESS);

    const size_t size = 256 << 20, part = 64 << 20;
    const uint64_t gpa = 0x40000000;
    uint8_t *ram = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    CHECK(map(ram, gpa, size) == HV_SUCCESS);
    memset(ram, 0xab, size);
    uint64_t before = footprint();

    /* A discarded range inside Guest RAM is freed and reads zero; the rest is kept. */
    CHECK(discard(ram + part, part) == 0);
    CHECK(qemu_ram_reclaim_total() == part);
    CHECK(before - footprint() >= part - (1 << 20));
    CHECK(ram[part] == 0 && ram[2 * part - 1] == 0);
    CHECK(ram[part - 1] == 0xab && ram[2 * part] == 0xab);
    /* Its VM mapping came back: protecting and unmapping the whole range still works. */
    CHECK(protect(gpa, size) == HV_SUCCESS);
    CHECK(discard(ram, part) == 0);
    CHECK(qemu_ram_reclaim_total() == 2 * part);

    /* Unaligned ranges, other advice and memory outside Guest RAM pass through. */
    CHECK(discard(ram + 3 * part + 4096, part / 2) == 0);
    uint8_t *other = mmap(NULL, part, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    memset(other, 1, part);
    CHECK(discard(other, part) == 0 && other[0] == 1);
    CHECK(qemu_ram_reclaim_total() == 2 * part);

    /* An alias (the same pages at a second address) is never replaced. */
    CHECK(map(ram + 3 * part, gpa + size, part) == HV_SUCCESS);
    CHECK(discard(ram + 3 * part, part) == 0 && ram[3 * part] == 0xab);
    CHECK(qemu_ram_reclaim_total() == 2 * part);
    CHECK(unmap(gpa + size, part) == HV_SUCCESS);

    /* Unmapped Guest RAM is no longer tracked. */
    CHECK(unmap(gpa, size) == HV_SUCCESS);
    CHECK(discard(ram + 3 * part, part) == 0 && ram[3 * part] == 0xab);
    CHECK(qemu_ram_reclaim_total() == 2 * part);

    printf("%s\n", failures ? "qemu-ram-reclaim: FAILED" : "qemu-ram-reclaim: ok");
    return failures != 0;
}
#endif
