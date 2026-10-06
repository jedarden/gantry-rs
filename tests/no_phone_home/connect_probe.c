/*
 * connect_probe.c — positive control for the no-phone-home integration
 * tests (tests/no_phone_home_integration.rs and
 * tests/init_no_phone_home_integration.rs, plan S-7 / INV-6).
 *
 * Attempts one connect to <ip> <port> and exits. The connection is
 * expected to fail — nothing is listening — because the attempt is the
 * thing under test: run under the netobserver preload with a netlog path,
 * it must leave exactly one record behind, proving the observer actually
 * sees outbound connections (a silently broken observer would otherwise
 * let the main assertion pass vacuously). The connect result is
 * deliberately not reflected in the exit code.
 *
 * Modes (optional argv[3]):
 *   (absent)  TCP (SOCK_STREAM) — the positive control's shape: dialed at
 *             loopback, the connect is refused by the kernel instantly.
 *   udp       UDP (SOCK_DGRAM) — connect(2) only pins the default
 *             destination: nothing is sent and nothing waits, so a dial to
 *             a *non-loopback* address completes instantly with no packet
 *             on the wire. The onboarding mutation run uses this to inject
 *             a phone-home to an RFC 5737 TEST-NET address, which a TCP
 *             dial could not do: the blackholed SYN would stall the leg
 *             for the full retransmit window instead of failing at once.
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
    if (argc != 3 && argc != 4)
        return 2;

    int type = SOCK_STREAM;
    if (argc == 4) {
        if (strcmp(argv[3], "udp") == 0)
            type = SOCK_DGRAM;
        else
            return 2;
    }

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons((uint16_t)atoi(argv[2]));
    if (inet_pton(AF_INET, argv[1], &sa.sin_addr) != 1)
        return 2;

    int fd = socket(AF_INET, type, 0);
    if (fd < 0)
        return 0;
    (void)connect(fd, (struct sockaddr *)&sa, sizeof(sa));
    close(fd);
    return 0;
}
