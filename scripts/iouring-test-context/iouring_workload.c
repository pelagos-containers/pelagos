/*
 * iouring_workload.c — real io_uring end-to-end workload for seccomp tests.
 *
 * Exercises all three io_uring syscalls by doing actual async I/O:
 *   io_uring_setup    (425) — create submission/completion ring
 *   io_uring_enter    (426) — submit request + wait for completion
 *
 * Specifically: submits an IORING_OP_NOP (no-op), waits for its CQE, and
 * verifies result == 0.  A NOP is sufficient to prove the kernel io_uring
 * machinery is reachable and functional end-to-end without needing a file
 * or socket target.
 *
 * Exit codes:
 *   0  — io_uring works: NOP submitted, CQE received, result == 0
 *   1  — EPERM from io_uring_setup: syscall blocked by seccomp
 *   2  — other failure (kernel too old, mmap failed, bad CQE result, etc.)
 *
 * Compiled as a static binary so it runs inside the Alpine (musl) rootfs
 * without glibc.  All io_uring structures are defined inline to avoid a
 * dependency on linux/io_uring.h.
 *
 * Used by:
 *   test_seccomp_docker_blocks_io_uring        — expects exit 1
 *   test_seccomp_iouring_profile_allows_io_uring — expects exit 0
 *   test_seccomp_iouring_e2e                   — expects exit 0
 */

#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_io_uring_setup
#define SYS_io_uring_setup   425
#endif
#ifndef SYS_io_uring_enter
#define SYS_io_uring_enter   426
#endif

/* Offsets for mmap(2) into the ring fd */
#define IORING_OFF_SQ_RING  0ULL
#define IORING_OFF_CQ_RING  0x8000000ULL
#define IORING_OFF_SQES     0x10000000ULL

#define IORING_ENTER_GETEVENTS  1U
#define IORING_OP_NOP           0

typedef uint8_t  __u8;
typedef uint16_t __u16;
typedef uint32_t __u32;
typedef uint64_t __u64;
typedef int32_t  __s32;

struct io_sqring_offsets {
    __u32 head, tail, ring_mask, ring_entries, flags, dropped, array;
    __u32 resv1;
    __u64 resv2;
};

struct io_cqring_offsets {
    __u32 head, tail, ring_mask, ring_entries, overflow, cqes, flags;
    __u32 resv1;
    __u64 resv2;
};

struct io_uring_params {
    __u32 sq_entries, cq_entries, flags, sq_thread_cpu, sq_thread_idle, features, wq_fd;
    __u32 resv[3];
    struct io_sqring_offsets sq_off;
    struct io_cqring_offsets cq_off;
};

struct io_uring_sqe {
    __u8  opcode, flags;
    __u16 ioprio;
    __s32 fd;
    __u64 off, addr;
    __u32 len, rw_flags;
    __u64 user_data;
    __u16 buf_index, personality;
    __s32 splice_fd_in;
    __u64 addr3, pad;
};

struct io_uring_cqe {
    __u64 user_data;
    __s32 res;
    __u32 flags;
};

int main(void) {
    struct io_uring_params params;
    memset(&params, 0, sizeof(params));

    long ring_fd = syscall(SYS_io_uring_setup, 8, &params);
    if (ring_fd < 0) {
        if (errno == EPERM) return 1; /* blocked by seccomp */
        return 2;
    }

    /* Map the submission ring */
    size_t sq_ring_sz = params.sq_off.array + params.sq_entries * sizeof(__u32);
    char *sq_ring = mmap(NULL, sq_ring_sz, PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_POPULATE, (int)ring_fd, IORING_OFF_SQ_RING);
    if (sq_ring == MAP_FAILED) { close((int)ring_fd); return 2; }

    /* Map the SQE array */
    size_t sqe_sz = params.sq_entries * sizeof(struct io_uring_sqe);
    struct io_uring_sqe *sqes = mmap(NULL, sqe_sz, PROT_READ | PROT_WRITE,
                                     MAP_SHARED | MAP_POPULATE, (int)ring_fd, IORING_OFF_SQES);
    if (sqes == MAP_FAILED) { close((int)ring_fd); return 2; }

    /* Map the completion ring */
    size_t cq_ring_sz = params.cq_off.cqes + params.cq_entries * sizeof(struct io_uring_cqe);
    char *cq_ring = mmap(NULL, cq_ring_sz, PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_POPULATE, (int)ring_fd, IORING_OFF_CQ_RING);
    if (cq_ring == MAP_FAILED) { close((int)ring_fd); return 2; }

    /* Place a NOP SQE at the current tail */
    __u32 sq_tail = *(__u32 *)(sq_ring + params.sq_off.tail);
    __u32 sq_mask = *(__u32 *)(sq_ring + params.sq_off.ring_mask);
    __u32 *sq_arr = (__u32 *)(sq_ring + params.sq_off.array);

    __u32 idx = sq_tail & sq_mask;
    memset(&sqes[idx], 0, sizeof(sqes[idx]));
    sqes[idx].opcode    = IORING_OP_NOP;
    sqes[idx].user_data = 0x10U;
    sq_arr[idx] = idx;

    __sync_synchronize();
    *(__u32 *)(sq_ring + params.sq_off.tail) = sq_tail + 1;
    __sync_synchronize();

    /* Submit 1 SQE and wait for 1 CQE */
    long ret = syscall(SYS_io_uring_enter, (int)ring_fd, 1, 1,
                       IORING_ENTER_GETEVENTS, NULL, 0);
    if (ret < 0) { close((int)ring_fd); return 2; }

    /* Read the CQE */
    __u32 cq_head = *(__u32 *)(cq_ring + params.cq_off.head);
    __u32 cq_mask = *(__u32 *)(cq_ring + params.cq_off.ring_mask);
    struct io_uring_cqe *cqe =
        (struct io_uring_cqe *)(cq_ring + params.cq_off.cqes
            + (cq_head & cq_mask) * sizeof(struct io_uring_cqe));

    __s32 result = cqe->res;

    /* Advance CQ head so the kernel can reuse the slot */
    __sync_synchronize();
    *(__u32 *)(cq_ring + params.cq_off.head) = cq_head + 1;

    close((int)ring_fd);

    /* NOP must complete with result 0 */
    return (result == 0) ? 0 : 2;
}
