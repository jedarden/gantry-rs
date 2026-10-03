/*
 * connect_probe.c — positive control for the no-phone-home integration
 * test (tests/no_phone_home_integration.rs, plan S-7 / INV-6).
 *
 * Attempts one TCP connect to <ip> <port> and exits. The connection is
 * expected to fail — nothing is listening — because the attempt is the
 * thing under test: run under the netobserver preload with a netlog path,
 * it must leave exactly one record behind, proving the observer actually
 * sees outbound connections (a silently broken observer would otherwise
 * let the main assertion pass vacuously). The connect result is
 * deliberately not reflected in the exit code.
 */

#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    if (argc != 3)
        return 2;

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons((uint16_t)atoi(argv[2]));
    if (inet_pton(AF_INET, argv[1], &sa.sin_addr) != 1)
        return 2;

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0)
        return 0;
    (void)connect(fd, (struct sockaddr *)&sa, sizeof(sa));
    close(fd);
    return 0;
}
