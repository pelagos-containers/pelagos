/*
 * iouring_probe.c — minimal io_uring syscall probe for seccomp integration tests.
 *
 * Calls io_uring_setup(0, NULL) directly via syscall(2).  The kernel will
 * always reject this call (invalid args), but the *kind* of rejection tells
 * us whether the syscall was reached at all:
 *
 *   EPERM  (exit 1) — blocked by seccomp (Docker default profile)
 *   other  (exit 0) — syscall reached the kernel; io_uring is permitted
 *                      (EINVAL / EFAULT expected with these bogus args)
 *
 * Used by:
 *   test_seccomp_docker_blocks_io_uring   — expects exit 1
 *   test_seccomp_iouring_profile_allows   — expects exit 0
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_io_uring_setup
#define SYS_io_uring_setup 425
#endif

int main(void) {
    long ret = syscall(SYS_io_uring_setup, 0, NULL);
    if (ret == -1 && errno == EPERM) {
        /* Blocked by seccomp */
        return 1;
    }
    /* Reached the kernel (EINVAL/EFAULT with bogus args is fine) */
    return 0;
}
