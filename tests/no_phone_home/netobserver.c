/*
 * netobserver.c — LD_PRELOAD network observer for the no-phone-home
 * integration test (tests/no_phone_home_integration.rs, plan S-7 / INV-6).
 *
 * Observe, never block: every AF_INET/AF_INET6 destination handed to
 * connect(2), sendto(2), or sendmsg(2) is appended as one line to the file
 * named by GANTRY_TEST_NETLOG, and the real syscall then runs unchanged.
 * Every process in the observed run's process tree inherits this library,
 * so any component that reaches for IP networking leaves a line behind;
 * the test asserts that after a full gantry run none do.
 *
 * Escapes exist by construction — a static binary, a raw syscall, or
 * glibc's internal resolver traffic bypasses LD_PRELOAD interposition.
 * What this enforces is the pledge that matters: nothing linked into the
 * run (gantry itself or any library/subprocess it spawns) may open a
 * network connection through the normal libc socket API. With the
 * loopback fixture backend and the file:// git remote there is no
 * legitimate IP destination at all, so the allow-list is empty and any
 * line in the log is a phone-home.
 *
 * The file is opened per event with O_APPEND, so per-process file offsets
 * never rewind each other and lines from concurrent processes interleave
 * whole. Without GANTRY_TEST_NETLOG the library is inert — production
 * binaries that happen to inherit it record nothing.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Format one IP destination as text and append the record line. Best
 * effort by design: a logging failure must never alter the observed run,
 * so every error path just drops the record. */
static void observe(const char *op, const struct sockaddr *sa, socklen_t len)
{
    const char *path = getenv("GANTRY_TEST_NETLOG");
    if (path == NULL || sa == NULL || len < sizeof(sa_family_t))
        return;
    if (sa->sa_family != AF_INET && sa->sa_family != AF_INET6)
        return;

    char dst[INET6_ADDRSTRLEN + 8];
    if (sa->sa_family == AF_INET) {
        const struct sockaddr_in *in4 = (const struct sockaddr_in *)sa;
        char ip[INET_ADDRSTRLEN];
        if (inet_ntop(AF_INET, &in4->sin_addr, ip, sizeof(ip)) == NULL)
            return;
        snprintf(dst, sizeof(dst), "%s:%u", ip,
                 (unsigned)ntohs(in4->sin_port));
    } else {
        const struct sockaddr_in6 *in6 = (const struct sockaddr_in6 *)sa;
        char ip[INET6_ADDRSTRLEN];
        if (inet_ntop(AF_INET6, &in6->sin6_addr, ip, sizeof(ip)) == NULL)
            return;
        snprintf(dst, sizeof(dst), "[%s]:%u", ip,
                 (unsigned)ntohs(in6->sin6_port));
    }

    int fd = open(path, O_WRONLY | O_APPEND | O_CREAT, 0644);
    if (fd < 0)
        return;
    char line[192];
    int n = snprintf(line, sizeof(line), "%ld %s dst=%s\n", (long)getpid(),
                     op, dst);
    if (n > 0) {
        ssize_t written = write(fd, line, (size_t)n);
        (void)written;
    }
    close(fd);
}

int connect(int fd, const struct sockaddr *sa, socklen_t len)
{
    static int (*real_connect)(int, const struct sockaddr *, socklen_t);
    if (real_connect == NULL)
        real_connect = dlsym(RTLD_NEXT, "connect");
    observe("connect", sa, len);
    if (real_connect != NULL)
        return real_connect(fd, sa, len);
    return (int)syscall(SYS_connect, fd, sa, len);
}

ssize_t sendto(int fd, const void *buf, size_t n, int flags,
               const struct sockaddr *sa, socklen_t len)
{
    static ssize_t (*real_sendto)(int, const void *, size_t, int,
                                  const struct sockaddr *, socklen_t);
    if (real_sendto == NULL)
        real_sendto = dlsym(RTLD_NEXT, "sendto");
    observe("sendto", sa, len);
    if (real_sendto != NULL)
        return real_sendto(fd, buf, n, flags, sa, len);
    return (ssize_t)syscall(SYS_sendto, fd, buf, n, flags, sa, len);
}

ssize_t sendmsg(int fd, const struct msghdr *msg, int flags)
{
    static ssize_t (*real_sendmsg)(int, const struct msghdr *, int);
    if (real_sendmsg == NULL)
        real_sendmsg = dlsym(RTLD_NEXT, "sendmsg");
    if (msg != NULL)
        observe("sendmsg", (const struct sockaddr *)msg->msg_name,
                msg->msg_namelen);
    if (real_sendmsg != NULL)
        return real_sendmsg(fd, msg, flags);
    return (ssize_t)syscall(SYS_sendmsg, fd, msg, flags);
}
