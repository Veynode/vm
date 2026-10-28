// The macOS Guest's port relay: the host reaches the Guest's own loopback
// services (sshd, Screen Sharing, published ports) over virtio-vsock instead
// of the Guest's vmnet address, which macOS's Local Network privacy keeps
// from a launchd-started daemon. It listens on vsock port 5022 for the host
// (CID 2) only; each connection opens with one line naming a TCP port
// ("22\n") and is then spliced to 127.0.0.1 at that port, half-closes
// included. The daemon writes it and its LaunchAgent into the Guest's disk
// at machine.create (crates/control/src/services/macos_relay.rs).
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/vsock.h>
#include <unistd.h>

#define RELAY_PORT 5022
#define HOST_CID 2

struct pair { int from, to; };

static void *splice_half(void *argument) {
    struct pair *p = argument;
    char buffer[65536];
    for (;;) {
        ssize_t n = read(p->from, buffer, sizeof buffer);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) break;
        for (ssize_t done = 0; done < n;) {
            ssize_t w = write(p->to, buffer + done, (size_t)(n - done));
            if (w < 0 && errno == EINTR) continue;
            if (w <= 0) goto out;
            done += w;
        }
    }
out:
    shutdown(p->to, SHUT_WR);
    shutdown(p->from, SHUT_RD);
    return NULL;
}

/// The port line: at most 6 bytes before "\n", 1..65535.
static int read_port(int fd) {
    char line[8];
    size_t used = 0;
    while (used < sizeof line) {
        ssize_t n = read(fd, line + used, 1);
        if (n < 0 && errno == EINTR) continue;
        if (n != 1) return -1;
        if (line[used] == '\n') {
            line[used] = 0;
            char *end;
            long port = strtol(line, &end, 10);
            return used > 0 && *end == 0 && port > 0 && port < 65536 ? (int)port : -1;
        }
        used++;
    }
    return -1;
}

static void *serve(void *argument) {
    int guest = (int)(intptr_t)argument;
    int port = read_port(guest);
    int local = -1;
    if (port > 0 && (local = socket(AF_INET, SOCK_STREAM, 0)) >= 0) {
        struct sockaddr_in address = {.sin_len = sizeof address, .sin_family = AF_INET,
                                      .sin_port = htons((uint16_t)port),
                                      .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
        int one = 1;
        setsockopt(local, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
        setsockopt(local, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
        if (connect(local, (struct sockaddr *)&address, sizeof address) == 0) {
            struct pair in = {guest, local}, out = {local, guest};
            pthread_t thread;
            if (pthread_create(&thread, NULL, splice_half, &in) == 0) {
                splice_half(&out);
                pthread_join(thread, NULL);
            }
        }
    }
    if (local >= 0) close(local);
    close(guest);
    return NULL;
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    int listener = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (listener < 0) { perror("veynode-relay: AF_VSOCK"); return 1; }
    struct sockaddr_vm address = {.svm_len = sizeof address, .svm_family = AF_VSOCK,
                                  .svm_port = RELAY_PORT, .svm_cid = VMADDR_CID_ANY};
    if (bind(listener, (struct sockaddr *)&address, sizeof address) != 0 || listen(listener, 64) != 0) {
        perror("veynode-relay: vsock listen");
        return 1;
    }
    for (;;) {
        struct sockaddr_vm peer;
        socklen_t length = sizeof peer;
        int fd = accept(listener, (struct sockaddr *)&peer, &length);
        if (fd < 0) { if (errno == EINTR || errno == ECONNABORTED) continue; perror("veynode-relay: accept"); return 1; }
        int one = 1;
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
        pthread_t thread;
        if (peer.svm_cid != HOST_CID || pthread_create(&thread, NULL, serve, (void *)(intptr_t)fd) != 0) {
            close(fd);
            continue;
        }
        pthread_detach(thread);
    }
}
