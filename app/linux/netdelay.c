/*
 * Narrow network gate for the Wine/Proton game process.
 *
 * Wine calls the host libc connect() for TCP sockets.  We delay only the
 * configured world-server port until the keypatch helper creates the ready
 * file.  This library does not read or write the game's memory, does not need
 * root, and leaves Battle.net (1119) and all other connections untouched.
 *
 * Environment:
 *   FOREVER_READY_FILE  host path; connection proceeds once it exists
 *   FOREVER_WORLD_PORT  TCP port of the worldserver (e.g. 8085)
 *   FOREVER_NET_TIMEOUT timeout in milliseconds (default 20000)
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static int (*real_connect_fn)(int, const struct sockaddr *, socklen_t);

static int world_port(void) {
    const char *value = getenv("FOREVER_WORLD_PORT");
    if (!value || !*value) return 0;
    return atoi(value);
}

static int timeout_ms(void) {
    const char *value = getenv("FOREVER_NET_TIMEOUT");
    int parsed = value ? atoi(value) : 0;
    return parsed > 0 ? parsed : 20000;
}

static int destination_port(const struct sockaddr *address, socklen_t length) {
    if (!address) return 0;
    if (address->sa_family == AF_INET && length >= (socklen_t)sizeof(struct sockaddr_in))
        return ntohs(((const struct sockaddr_in *)address)->sin_port);
    if (address->sa_family == AF_INET6 && length >= (socklen_t)sizeof(struct sockaddr_in6))
        return ntohs(((const struct sockaddr_in6 *)address)->sin6_port);
    return 0;
}

static void wait_for_ready(void) {
    const char *path = getenv("FOREVER_READY_FILE");
    if (!path || !*path) return;
    long deadline = timeout_ms();
    struct timespec pause = {0, 100 * 1000 * 1000};
    for (long waited = 0; waited < deadline; waited += 100) {
        if (access(path, F_OK) == 0) return;
        nanosleep(&pause, NULL);
    }
}

int connect(int fd, const struct sockaddr *address, socklen_t length) {
    if (!real_connect_fn) {
        real_connect_fn = (int (*)(int, const struct sockaddr *, socklen_t))
            dlsym(RTLD_NEXT, "connect");
        if (!real_connect_fn) {
            errno = ENOSYS;
            return -1;
        }
    }
    int port = destination_port(address, length);
    if (port && port == world_port()) wait_for_ready();
    return real_connect_fn(fd, address, length);
}
