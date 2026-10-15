// Test-only network guard. Deny the live proxy and all non-loopback connections
// before libc can send anything; no request or account data is ever recorded.
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#ifndef __APPLE__
#include <dlfcn.h>
#endif

static int denied(const struct sockaddr *address) {
    unsigned short port = 0;
    int loopback = 0;
    if (address->sa_family == AF_INET) {
        const struct sockaddr_in *a = (const struct sockaddr_in *)address;
        port = ntohs(a->sin_port);
        loopback = (ntohl(a->sin_addr.s_addr) >> 24) == 127;
    } else if (address->sa_family == AF_INET6) {
        const struct sockaddr_in6 *a = (const struct sockaddr_in6 *)address;
        port = ntohs(a->sin6_port);
        loopback = IN6_IS_ADDR_LOOPBACK(&a->sin6_addr);
        if (IN6_IS_ADDR_V4MAPPED(&a->sin6_addr)) loopback = a->sin6_addr.s6_addr[12] == 127;
    } else {
        return 0;
    }
    const char *configured = getenv("TCR_TEST_DENIED_PORT");
    const unsigned short forbidden = configured ? (unsigned short)strtoul(configured, 0, 10) : 3456;
    return !loopback || port == forbidden;
}

static int reject_connection(void) {
    const char *path = getenv("TCR_TEST_CONNECT_LOG");
    if (path) {
        const int file = open(path, O_WRONLY | O_CREAT | O_APPEND, 0600);
        if (file >= 0) { (void)write(file, "connect\n", 8); (void)close(file); }
    }
    errno = ECONNREFUSED;
    return -1;
}

#ifdef __APPLE__
static int guarded_connect(int socket, const struct sockaddr *address, socklen_t length) {
    if (denied(address)) return reject_connection();
    return connect(socket, address, length);
}
__attribute__((used, section("__DATA,__interpose")))
static struct { const void *replacement; const void *original; } binding = {
    (const void *)guarded_connect, (const void *)connect
};
#else
int connect(int socket, const struct sockaddr *address, socklen_t length) {
    if (denied(address)) return reject_connection();
    int (*original)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "connect");
    return original(socket, address, length);
}
#endif
