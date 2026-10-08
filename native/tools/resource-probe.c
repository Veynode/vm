// macOS process ledger sampler. No process arguments, paths or screenshots collected.
#include <errno.h>
#include <inttypes.h>
#include <libproc.h>
#include <limits.h>
#include <mach/mach_time.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>

#define MAX_PIDS 128
struct process_sample {
    pid_t pid;
    struct rusage_info_v4 previous, current;
    uint64_t previous_tick, current_tick;
    double cpu;
};
static struct process_sample processes[MAX_PIDS];
static uint64_t number(const char *text, uint64_t low, uint64_t high) {
    if (!text[0] || text[0] == '-') goto bad;
    errno = 0;
    char *end;
    unsigned long long value = strtoull(text, &end, 10);
    if (errno || *end || value < low || value > high) goto bad;
    return value;
bad:
    fprintf(stderr, "invalid bounded integer: %s\n", text);
    exit(2);
}
static bool read_process(struct process_sample *p, bool baseline) {
    uint64_t before = mach_absolute_time();
    if (proc_pid_rusage(p->pid, RUSAGE_INFO_V4, (rusage_info_t *)&p->current) != 0) {
        fprintf(stderr, "proc_pid_rusage pid=%d errno=%d\n", p->pid, errno);
        return false;
    }
    p->current_tick = before + (mach_absolute_time() - before) / 2;
    if (p->current.ri_proc_exit_abstime != 0 || p->current.ri_proc_start_abstime == 0) {
        fprintf(stderr, "target exited or has no process identity: pid=%d\n", p->pid);
        return false;
    }
    if (!baseline) {
        if (p->previous.ri_proc_start_abstime != p->current.ri_proc_start_abstime ||
            memcmp(p->previous.ri_uuid, p->current.ri_uuid, sizeof(p->current.ri_uuid))) {
            fprintf(stderr, "target identity changed: pid=%d\n", p->pid);
            return false;
        }
        uint64_t old_cpu = p->previous.ri_user_time + p->previous.ri_system_time;
        uint64_t new_cpu = p->current.ri_user_time + p->current.ri_system_time;
        if (new_cpu < old_cpu || p->current_tick <= p->previous_tick) {
            fprintf(stderr, "non-monotonic counters: pid=%d\n", p->pid);
            return false;
        }
        // XNU task_power_info uses Mach absolute ticks for both CPU counters.
        // Ratio uses the same tick domain; 100% is one full logical CPU.
        p->cpu = 100.0 * (double)(new_cpu-old_cpu) / (double)(p->current_tick-p->previous_tick);
    }
    return true;
}
int main(int argc, char **argv) {
    if (argc < 5 || argc > 4 + MAX_PIDS) {
        fprintf(stderr, "usage: resource-probe SAMPLE_COUNT INTERVAL_MS GUEST_ASSIGNED_BYTES PID [PID ...]\n");
        return 2;
    }
    uint64_t count = number(argv[1], 1, 86400);
    uint64_t interval = number(argv[2], 100, 60000);
    uint64_t assigned = number(argv[3], 0, UINT64_MAX);
    if (count * interval > 21600000) {
        fprintf(stderr, "sampling duration must be at most six hours\n");
        return 2;
    }
    int n = argc - 4;
    for (int i = 0; i < n; i++) {
        processes[i].pid = (pid_t)number(argv[i+4], 1, INT_MAX);
        for (int j = 0; j < i; j++) if (processes[j].pid == processes[i].pid) {
            fprintf(stderr, "duplicate PID would double-count resources\n"); return 2;
        }
        if (!read_process(&processes[i], true)) return 1;
        processes[i].previous = processes[i].current;
        processes[i].previous_tick = processes[i].current_tick;
    }
    mach_timebase_info_data_t timebase;
    if (mach_timebase_info(&timebase) != KERN_SUCCESS || !timebase.denom) return 1;
    setvbuf(stdout, NULL, _IOLBF, 0);
    for (uint64_t sample = 0; sample < count; sample++) {
        struct timespec delay = {.tv_sec=(time_t)(interval/1000), .tv_nsec=(long)(interval%1000)*1000000};
        while (nanosleep(&delay, &delay) != 0) if (errno != EINTR) return 1;
        uint64_t begin = mach_absolute_time(), footprint = 0;
        double cpu = 0;
        for (int i = 0; i < n; i++) {
            if (!read_process(&processes[i], false)) return 1;
            if (UINT64_MAX-footprint < processes[i].current.ri_phys_footprint) return 1;
            footprint += processes[i].current.ri_phys_footprint;
            cpu += processes[i].cpu;
        }
        uint64_t end = mach_absolute_time();
        struct timespec wall;
        if (clock_gettime(CLOCK_REALTIME, &wall) != 0) return 1;
        printf("{\"kind\":\"resource\",\"tool\":\"veynode-libproc-v1\",\"sample_index\":%" PRIu64
               ",\"timestamp_unix_ns\":%" PRIu64 ",\"mach_tick\":%" PRIu64
               ",\"mach_timebase_numer\":%u,\"mach_timebase_denom\":%u,\"sample_span_ticks\":%" PRIu64
               ",\"host_cpu_percent\":%.6f,\"host_physical_footprint_bytes\":%" PRIu64
               ",\"guest_assigned_bytes\":%" PRIu64 ",\"processes\":[",
               sample, (uint64_t)wall.tv_sec*1000000000+(uint64_t)wall.tv_nsec, end,
               timebase.numer, timebase.denom, end-begin, cpu, footprint, assigned);
        for (int i = 0; i < n; i++) {
            struct process_sample *p = &processes[i];
            printf("%s{\"pid\":%d,\"start_mach_tick\":%" PRIu64
                   ",\"sample_mach_tick\":%" PRIu64 ",\"interval_ticks\":%" PRIu64
                   ",\"user_cpu_ticks\":%" PRIu64 ",\"system_cpu_ticks\":%" PRIu64
                   ",\"cpu_percent\":%.6f,\"physical_footprint_bytes\":%" PRIu64
                   ",\"resident_bytes\":%" PRIu64 ",\"wired_bytes\":%" PRIu64 "}",
                   i ? "," : "", p->pid, p->current.ri_proc_start_abstime,
                   p->current_tick, p->current_tick-p->previous_tick,
                   p->current.ri_user_time, p->current.ri_system_time, p->cpu,
                   p->current.ri_phys_footprint, p->current.ri_resident_size, p->current.ri_wired_size);
            p->previous = p->current;
            p->previous_tick = p->current_tick;
        }
        if (puts("]}") == EOF || fflush(stdout) == EOF) return 1;
    }
    return 0;
}
