/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Returns Guest RAM that QEMU discards (virtio-balloon free page reporting and
 * inflation) to macOS. QEMU discards with madvise(MADV_DONTNEED), which on
 * macOS keeps the pages resident and counted. For a range inside HVF-mapped
 * Guest RAM this replaces it with fresh zero-fill pages instead: unmap it from
 * the VM, map anonymous memory over it in place, map it back with the same
 * flags, the way Linux's MADV_DONTNEED leaves zero pages behind. The Guest does
 * not touch a range while it is reported or ballooned. QEMU reaches madvise
 * only for anonymous RAM (file-backed RAM needs fallocate, absent on macOS),
 * and VeynodeVM shares no Guest RAM with another process.
 *
 * The loader calls qemu_ram_reclaim_install() after dlopen of the QEMU image
 * and before qemu_init; it rebinds that image's imports of madvise and
 * hv_vm_map/unmap/protect. Any other madvise, and any range it cannot place
 * in one tracked mapping, goes to madvise unchanged. The runtime is not modified. */
#include <Hypervisor/Hypervisor.h>
#include <dlfcn.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach-o/nlist.h>
#include <mach/mach.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define QRR_MAX_REGIONS 128
struct qrr_region { uint8_t *host; uint64_t gpa; size_t size; hv_memory_flags_t flags; };
static struct qrr_region qrr_regions[QRR_MAX_REGIONS];
static int qrr_count;
/* Set once tracking is lost (table full); from then on madvise passes through. */
static int qrr_untracked;
static pthread_mutex_t qrr_lock = PTHREAD_MUTEX_INITIALIZER;
static int (*qrr_real_madvise)(void *, size_t, int);
static hv_return_t (*qrr_real_map)(void *, hv_ipa_t, size_t, hv_memory_flags_t);
static hv_return_t (*qrr_real_unmap)(hv_ipa_t, size_t);
static hv_return_t (*qrr_real_protect)(hv_ipa_t, size_t, hv_memory_flags_t);
static size_t qrr_reclaimed;

/* Splits the region holding gpa so a region starts there. */
static int qrr_split(uint64_t gpa) {
    for (int i = 0; i < qrr_count; i++) {
        struct qrr_region *r = &qrr_regions[i];
        if (gpa <= r->gpa || gpa >= r->gpa + r->size) continue;
        if (qrr_count == QRR_MAX_REGIONS) return -1;
        uint64_t head = gpa - r->gpa;
        qrr_regions[qrr_count++] = (struct qrr_region){r->host + head, gpa, r->size - head, r->flags};
        r->size = head;
        return 0;
    }
    return 0;
}

/* Bounds [gpa, gpa+size) by region edges; on overflow stops tracking for good. */
static int qrr_bound(uint64_t gpa, size_t size) {
    if (qrr_untracked) return -1;
    if (qrr_split(gpa) || qrr_split(gpa + size)) {
        qrr_untracked = 1;
        fprintf(stderr, "VEYNODE_RAM_RECLAIM off: too many HVF mappings\n");
        return -1;
    }
    return 0;
}

static hv_return_t qrr_map(void *host, hv_ipa_t gpa, size_t size, hv_memory_flags_t flags) {
    hv_return_t ret = qrr_real_map(host, gpa, size, flags);
    pthread_mutex_lock(&qrr_lock);
    if (ret == HV_SUCCESS && !qrr_untracked) {
        if (qrr_count < QRR_MAX_REGIONS) qrr_regions[qrr_count++] = (struct qrr_region){host, gpa, size, flags};
        else {
            qrr_untracked = 1;
            fprintf(stderr, "VEYNODE_RAM_RECLAIM off: too many HVF mappings\n");
        }
    }
    pthread_mutex_unlock(&qrr_lock);
    return ret;
}

static hv_return_t qrr_unmap(hv_ipa_t gpa, size_t size) {
    hv_return_t ret = qrr_real_unmap(gpa, size);
    pthread_mutex_lock(&qrr_lock);
    if (ret == HV_SUCCESS && !qrr_bound(gpa, size)) {
        for (int i = 0; i < qrr_count;) {
            struct qrr_region *r = &qrr_regions[i];
            if (r->gpa >= gpa && r->gpa + r->size <= gpa + size) *r = qrr_regions[--qrr_count];
            else i++;
        }
    }
    pthread_mutex_unlock(&qrr_lock);
    return ret;
}

static hv_return_t qrr_protect(hv_ipa_t gpa, size_t size, hv_memory_flags_t flags) {
    hv_return_t ret = qrr_real_protect(gpa, size, flags);
    pthread_mutex_lock(&qrr_lock);
    if (ret == HV_SUCCESS && !qrr_bound(gpa, size)) {
        for (int i = 0; i < qrr_count; i++) {
            struct qrr_region *r = &qrr_regions[i];
            if (r->gpa >= gpa && r->gpa + r->size <= gpa + size) r->flags = flags;
        }
    }
    pthread_mutex_unlock(&qrr_lock);
    return ret;
}

static int qrr_madvise(void *addr, size_t len, int advice) {
    uint8_t *start = addr;
    size_t page = vm_page_size;
    if (advice != MADV_DONTNEED || !len || ((uintptr_t)start | len) & (page - 1)) return qrr_real_madvise(addr, len, advice);
    pthread_mutex_lock(&qrr_lock);
    /* Exactly one mapping may hold the range: an alias would keep the old pages. */
    struct qrr_region *r = NULL;
    int overlaps = 0;
    for (int i = 0; !qrr_untracked && i < qrr_count; i++) {
        struct qrr_region *c = &qrr_regions[i];
        if (start >= c->host + c->size || start + len <= c->host) continue;
        overlaps++;
        if (start >= c->host && start + len <= c->host + c->size) r = c;
    }
    if (!r || overlaps != 1) {
        pthread_mutex_unlock(&qrr_lock);
        return qrr_real_madvise(addr, len, advice);
    }
    hv_ipa_t gpa = r->gpa + (uint64_t)(start - r->host);
    hv_memory_flags_t flags = r->flags;
    if (qrr_real_unmap(gpa, len) != HV_SUCCESS) {
        pthread_mutex_unlock(&qrr_lock);
        return qrr_real_madvise(addr, len, advice);
    }
    /* A failed mmap leaves the old pages in place; either way they go back. */
    int fresh = mmap(start, len, PROT_READ | PROT_WRITE, MAP_FIXED | MAP_PRIVATE | MAP_ANON, -1, 0) == start;
    if (qrr_real_map(start, gpa, len, flags) != HV_SUCCESS) {
        fprintf(stderr, "VEYNODE_RAM_RECLAIM could not map Guest RAM back at 0x%llx\n", (unsigned long long)gpa);
        abort();
    }
    if (fresh) qrr_reclaimed += len;
    pthread_mutex_unlock(&qrr_lock);
    return fresh ? 0 : qrr_real_madvise(addr, len, advice);
}

/* Total bytes returned so far (for tests and diagnostics). */
static inline size_t qemu_ram_reclaim_total(void) {
    pthread_mutex_lock(&qrr_lock);
    size_t total = qrr_reclaimed;
    pthread_mutex_unlock(&qrr_lock);
    return total;
}

/* Finds (write 0) or rebinds (write 1) the symbol pointers to the hooked functions in
 * the image that defines `inside`; returns a bit per hooked function seen, or -1. */
static int qrr_rebind(const void *inside, int write) {
    Dl_info info;
    if (!dladdr(inside, &info)) return 0;
    const struct mach_header_64 *header = info.dli_fbase;
    intptr_t slide = 0;
    int found = 0;
    for (uint32_t i = 0; i < _dyld_image_count(); i++)
        if ((const void *)_dyld_get_image_header(i) == header) { slide = _dyld_get_image_vmaddr_slide(i); found = 1; break; }
    if (!found || header->magic != MH_MAGIC_64) return 0;
    const struct segment_command_64 *linkedit = NULL;
    const struct symtab_command *symtab = NULL;
    const struct dysymtab_command *dysymtab = NULL;
    const struct load_command *command = (const void *)(header + 1);
    for (uint32_t i = 0; i < header->ncmds; i++, command = (const void *)((const char *)command + command->cmdsize)) {
        if (command->cmd == LC_SEGMENT_64 && !strcmp(((const struct segment_command_64 *)command)->segname, SEG_LINKEDIT)) linkedit = (const void *)command;
        else if (command->cmd == LC_SYMTAB) symtab = (const void *)command;
        else if (command->cmd == LC_DYSYMTAB) dysymtab = (const void *)command;
    }
    if (!linkedit || !symtab || !dysymtab) return 0;
    uintptr_t base = (uintptr_t)slide + linkedit->vmaddr - linkedit->fileoff;
    const struct nlist_64 *symbols = (const void *)(base + symtab->symoff);
    const char *strings = (const char *)(base + symtab->stroff);
    const uint32_t *indirect = (const void *)(base + dysymtab->indirectsymoff);
    const struct { const char *name; void *replacement; } hooks[] = {
        {"_madvise", (void *)qrr_madvise}, {"_hv_vm_map", (void *)qrr_map},
        {"_hv_vm_unmap", (void *)qrr_unmap}, {"_hv_vm_protect", (void *)qrr_protect},
    };
    int seen = 0;
    command = (const void *)(header + 1);
    for (uint32_t i = 0; i < header->ncmds; i++, command = (const void *)((const char *)command + command->cmdsize)) {
        if (command->cmd != LC_SEGMENT_64) continue;
        const struct segment_command_64 *segment = (const void *)command;
        const struct section_64 *sections = (const void *)(segment + 1);
        for (uint32_t s = 0; s < segment->nsects; s++) {
            uint32_t type = sections[s].flags & SECTION_TYPE;
            if (type != S_LAZY_SYMBOL_POINTERS && type != S_NON_LAZY_SYMBOL_POINTERS) continue;
            void **slots = (void **)((uintptr_t)slide + sections[s].addr);
            for (uint64_t n = 0; n < sections[s].size / sizeof(void *); n++) {
                uint32_t index = indirect[sections[s].reserved1 + n];
                if (index & (INDIRECT_SYMBOL_ABS | INDIRECT_SYMBOL_LOCAL)) continue;
                const char *name = strings + symbols[index].n_un.n_strx;
                for (size_t h = 0; h < sizeof hooks / sizeof hooks[0]; h++) {
                    if (strcmp(name, hooks[h].name)) continue;
                    seen |= 1 << h;
                    if (!write) continue;
                    vm_address_t at = (vm_address_t)&slots[n] & ~(vm_address_t)(vm_page_size - 1);
                    if (vm_protect(mach_task_self(), at, vm_page_size, 0, VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY) != KERN_SUCCESS) return -1;
                    slots[n] = hooks[h].replacement;
                }
            }
        }
    }
    return seen;
}

/* Returns 0 once the QEMU image's imports are rebound, -1 if left as they were. */
static int qemu_ram_reclaim_install(void *qemu) {
    qrr_real_madvise = dlsym(RTLD_DEFAULT, "madvise");
    qrr_real_map = dlsym(RTLD_DEFAULT, "hv_vm_map");
    qrr_real_unmap = dlsym(RTLD_DEFAULT, "hv_vm_unmap");
    qrr_real_protect = dlsym(RTLD_DEFAULT, "hv_vm_protect");
    void *init = dlsym(qemu, "qemu_init");
    if (!qrr_real_madvise || !qrr_real_map || !qrr_real_unmap || !qrr_real_protect || !init) return -1;
    /* All four or none: a tracked map without its unmap would go stale. */
    if (qrr_rebind(init, 0) != 0xf || qrr_rebind(init, 1) != 0xf) {
        fprintf(stderr, "VEYNODE_RAM_RECLAIM off: QEMU imports not found\n");
        return -1;
    }
    return 0;
}
