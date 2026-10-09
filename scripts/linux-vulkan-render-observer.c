/* Diagnostic only: authenticated local ACK before exec, never STOP/CONT. */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#ifndef VN_SERVER
#error fixed VN_SERVER required
#endif
#ifndef VN_DEADLINE_MS
#define VN_DEADLINE_MS 5000
#endif
static int64_t now_ms(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static int transfer(int fd, void *data, size_t size, int writing, int64_t end) {
    size_t offset = 0;
    while (offset < size) {
        int64_t left = end - now_ms();
        if (left <= 0 || left > VN_DEADLINE_MS) return -1;
        struct pollfd p = {.fd = fd, .events = writing ? POLLOUT : POLLIN};
        int n = poll(&p, 1, (int)left);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0 || !(p.revents & p.events)) return -1;
        ssize_t done = writing ? send(fd, (char *)data + offset, size - offset, MSG_DONTWAIT)
                               : recv(fd, (char *)data + offset, size - offset, MSG_DONTWAIT);
        if (done < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) continue;
        if (done <= 0) return -1;
        offset += (size_t)done;
    }
    return 0;
}
int main(int argc, char **argv) {
    (void)argc;
    const char *path = getenv("VN_RENDER_OBSERVER_SOCKET");
    const char *expected = getenv("VN_RENDER_OBSERVER_PID");
    char *tail = NULL;
    if (!path || !expected || path[0] != '/' || !*expected) return 110;
    errno = 0;
    long parent = strtol(expected, &tail, 10);
    if (errno || *tail || parent <= 1 || parent > INT_MAX) return 110;
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    if (strlen(path) >= sizeof(a.sun_path)) return 110;
    memcpy(a.sun_path, path, strlen(path) + 1);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return 111;
    int one = 1;
    if (setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one))) { close(fd); return 111; }
    /* Local connect is nonblocking too; include it in the single deadline. */
    if (fcntl(fd, F_SETFL, O_NONBLOCK) || fcntl(fd, F_SETFD, FD_CLOEXEC)) { close(fd); return 111; }
    int64_t end = now_ms() + VN_DEADLINE_MS;
    if (connect(fd, (struct sockaddr *)&a, sizeof(a)) && errno != EINPROGRESS) { close(fd); return 112; }
    struct pollfd p = {.fd = fd, .events = POLLOUT};
    int n;
    for (;;) {
        int64_t left = end - now_ms();
        if (left <= 0 || left > VN_DEADLINE_MS) { close(fd); return 112; }
        n = poll(&p, 1, (int)left);
        if (n < 0 && errno == EINTR) continue;
        break;
    }
    int error = 0; socklen_t size = sizeof(error);
    if (n <= 0 || getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &size) || error) { close(fd); return 112; }
    uid_t uid; gid_t gid; pid_t peer = 0; size = sizeof(peer);
    if (getpeereid(fd, &uid, &gid) || uid != getuid() ||
        getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &peer, &size) || size != sizeof(peer) || peer != parent) {
        close(fd); return 113;
    }
    uint32_t tuple[3] = {htonl((uint32_t)getpid()), htonl((uint32_t)getppid()), htonl((uint32_t)getpgrp())};
    char ack = 0;
    if (transfer(fd, tuple, sizeof(tuple), 1, end) || transfer(fd, &ack, 1, 0, end) || ack != '1') {
        close(fd); return 114;
    }
    close(fd);
    argv[0] = VN_SERVER;
    execv(argv[0], argv);
    return 115;
}
