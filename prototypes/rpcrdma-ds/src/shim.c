/* All verbs/rdma-cm state lives here so Rust never touches an ibv
 * struct (their layouts come from the system headers — hand-writing
 * repr(C) mirrors is the classic FFI landmine). The surface Rust sees
 * is a handful of blocking calls over plain buffers.
 *
 * Single-connection server, deliberately: the prototype's question is
 * "does the kernel's xprtrdma accept our RPC-over-RDMA framing and move
 * data through our chunks", not "does it scale".
 *
 * M2 adds the data path: one registered DATA buffer that READ payloads
 * are RDMA-Written from and WRITE payloads are RDMA-Read into, and a
 * SEPARATE send CQ so a receive that completes while we wait on a send
 * is never consumed by the wrong poller (M1 shared one CQ and dropped
 * such receives — harmless only while the client sent one request at a
 * time).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <stdint.h>
#include <arpa/inet.h>
#include <rdma/rdma_cma.h>
#include <infiniband/verbs.h>

#define NRECV 32
#define RECV_SZ 8192
#define SEND_SZ 8192
#define DATA_SZ (4u << 20)
#define MAX_SEND_WR 256

static struct rdma_event_channel *ec;
static struct rdma_cm_id *listen_id, *conn;
static struct ibv_pd *pd;
static struct ibv_cq *send_cq, *recv_cq;
static char *recv_buf, *send_buf, *data_buf;
static struct ibv_mr *recv_mr, *send_mr, *data_mr;
static int outstanding; /* signaled send-queue WRs not yet completed */

static int post_recv(int idx) {
    struct ibv_sge sge = {
        .addr = (uintptr_t)(recv_buf + (size_t)idx * RECV_SZ),
        .length = RECV_SZ,
        .lkey = recv_mr->lkey,
    };
    struct ibv_recv_wr wr = { .wr_id = idx, .sg_list = &sge, .num_sge = 1 };
    struct ibv_recv_wr *bad;
    return ibv_post_recv(conn->qp, &wr, &bad);
}

static void teardown(void) {
    if (conn) {
        rdma_disconnect(conn);
        if (conn->qp) rdma_destroy_qp(conn);
    }
    if (recv_mr) ibv_dereg_mr(recv_mr);
    if (send_mr) ibv_dereg_mr(send_mr);
    if (data_mr) ibv_dereg_mr(data_mr);
    recv_mr = send_mr = data_mr = NULL;
    free(recv_buf); free(send_buf); free(data_buf);
    recv_buf = send_buf = data_buf = NULL;
    if (send_cq) ibv_destroy_cq(send_cq);
    if (recv_cq) ibv_destroy_cq(recv_cq);
    send_cq = recv_cq = NULL;
    if (pd) ibv_dealloc_pd(pd);
    pd = NULL;
    if (conn) rdma_destroy_id(conn);
    conn = NULL;
    outstanding = 0;
}

int rshim_listen(unsigned short port) {
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    ec = rdma_create_event_channel();
    if (!ec) return -1;
    if (rdma_create_id(ec, &listen_id, NULL, RDMA_PS_TCP)) return -2;
    if (rdma_bind_addr(listen_id, (struct sockaddr *)&a)) return -3;
    if (rdma_listen(listen_id, 4)) return -4;
    return 0;
}

static unsigned min_u(unsigned a, unsigned b) { return a < b ? a : b; }

/* Block until a client connection is fully ESTABLISHED. Tears down the
 * previous connection's resources first. */
int rshim_accept(void) {
    struct rdma_cm_event *ev;
    teardown();
    /* The poll loop leaves the CM channel non-blocking; restore
     * blocking mode so we park here instead of hot-spinning EAGAIN. */
    fcntl(ec->fd, F_SETFL, 0);
    for (;;) {
        if (rdma_get_cm_event(ec, &ev)) return -1;
        if (ev->event == RDMA_CM_EVENT_CONNECT_REQUEST) {
            struct rdma_conn_param cp;
            memcpy(&cp, &ev->param.conn, sizeof(cp));
            conn = ev->id;
            rdma_ack_cm_event(ev);

            struct ibv_device_attr da;
            if (ibv_query_device(conn->verbs, &da)) return -7;

            pd = ibv_alloc_pd(conn->verbs);
            send_cq = ibv_create_cq(conn->verbs, MAX_SEND_WR, NULL, NULL, 0);
            recv_cq = ibv_create_cq(conn->verbs, NRECV, NULL, NULL, 0);
            if (!pd || !send_cq || !recv_cq) return -2;
            struct ibv_qp_init_attr qa = {
                .send_cq = send_cq, .recv_cq = recv_cq,
                .cap = { .max_send_wr = MAX_SEND_WR, .max_recv_wr = NRECV,
                         .max_send_sge = 1, .max_recv_sge = 1 },
                .qp_type = IBV_QPT_RC,
            };
            if (rdma_create_qp(conn, pd, &qa)) return -3;

            recv_buf = aligned_alloc(4096, (size_t)NRECV * RECV_SZ);
            send_buf = aligned_alloc(4096, SEND_SZ);
            data_buf = aligned_alloc(4096, DATA_SZ);
            recv_mr = ibv_reg_mr(pd, recv_buf, (size_t)NRECV * RECV_SZ,
                                 IBV_ACCESS_LOCAL_WRITE);
            send_mr = ibv_reg_mr(pd, send_buf, SEND_SZ, 0);
            /* LOCAL_WRITE: RDMA Read responses land here. Nothing is
             * remote-accessible — the DS pushes and pulls, the client
             * never touches our memory. */
            data_mr = ibv_reg_mr(pd, data_buf, DATA_SZ, IBV_ACCESS_LOCAL_WRITE);
            if (!recv_mr || !send_mr || !data_mr) return -4;
            for (int i = 0; i < NRECV; i++)
                if (post_recv(i)) return -5;

            /* WE are the RDMA Read initiator (WRITE payloads are pulled
             * from the client). rdma-cm hands the CONNECT_REQUEST's
             * depths over from OUR side already: cp.initiator_depth is
             * the RDMA Reads the client lets us have outstanding (its
             * responder resources). Mirror them, capped by the device.
             * Swapping them (an M2 draft did) makes our depth 0 against
             * xprtrdma, which offers responder resources and never
             * initiates — every WRITE's RDMA Read would then fail. */
            struct rdma_conn_param acc = {
                .initiator_depth = min_u(cp.initiator_depth, da.max_qp_init_rd_atom),
                .responder_resources = min_u(cp.responder_resources, da.max_qp_rd_atom),
                .retry_count = 7, .rnr_retry_count = 7,
            };
            fprintf(stderr, "accept: initiator_depth=%u responder_resources=%u\n",
                    acc.initiator_depth, acc.responder_resources);
            if (rdma_accept(conn, &acc)) return -6;
        } else if (ev->event == RDMA_CM_EVENT_ESTABLISHED) {
            rdma_ack_cm_event(ev);
            /* CM channel goes non-blocking so the poll loop can watch
             * for DISCONNECT without stalling the CQ. */
            fcntl(ec->fd, F_SETFL, O_NONBLOCK);
            return 0;
        } else {
            rdma_ack_cm_event(ev);
        }
    }
}

/* Poll for one inbound message. Returns the recv-slot index (>=0),
 * -2 on disconnect. buf/len receive the message bytes. */
int rshim_wait_recv(unsigned char **buf, unsigned int *len) {
    struct ibv_wc wc;
    for (;;) {
        int n = ibv_poll_cq(recv_cq, 1, &wc);
        if (n < 0) return -1;
        if (n == 1) {
            if (wc.status != IBV_WC_SUCCESS) {
                fprintf(stderr, "recv wc status %d (%s)\n", wc.status,
                        ibv_wc_status_str(wc.status));
                return -2;
            }
            *buf = (unsigned char *)(recv_buf + wc.wr_id * (size_t)RECV_SZ);
            *len = wc.byte_len;
            return (int)wc.wr_id;
        }
        struct rdma_cm_event *ev;
        if (rdma_get_cm_event(ec, &ev) == 0) {
            int dis = ev->event == RDMA_CM_EVENT_DISCONNECTED;
            rdma_ack_cm_event(ev);
            if (dis) return -2;
        }
        usleep(20);
    }
}

int rshim_repost(int idx) { return post_recv(idx); }

void rshim_data_buf(unsigned char **buf, unsigned int *len) {
    *buf = (unsigned char *)data_buf;
    *len = DATA_SZ;
}

/* Post one RDMA Write (is_read=0: data_buf[local_off..+len] → client
 * rkey/raddr) or RDMA Read (is_read=1: client → data_buf). Every WR is
 * signaled so a failure is attributable; rshim_drain() reaps them. */
int rshim_rdma(int is_read, unsigned long local_off, unsigned int len,
               unsigned int rkey, unsigned long long raddr) {
    if (local_off + len > DATA_SZ) return -1;
    if (outstanding >= MAX_SEND_WR) return -2;
    struct ibv_sge sge = { .addr = (uintptr_t)(data_buf + local_off),
                           .length = len, .lkey = data_mr->lkey };
    struct ibv_send_wr wr = {
        .sg_list = &sge, .num_sge = 1,
        .opcode = is_read ? IBV_WR_RDMA_READ : IBV_WR_RDMA_WRITE,
        .send_flags = IBV_SEND_SIGNALED,
        .wr.rdma = { .remote_addr = raddr, .rkey = rkey },
    };
    struct ibv_send_wr *bad;
    if (ibv_post_send(conn->qp, &wr, &bad)) return -3;
    outstanding++;
    return 0;
}

/* Wait for every outstanding send-queue WR. Returns 0, or the negated
 * ibv_wc_status of the first failure (all WRs are still reaped). */
int rshim_drain(void) {
    int rc = 0;
    struct ibv_wc wc;
    while (outstanding > 0) {
        int n = ibv_poll_cq(send_cq, 1, &wc);
        if (n < 0) return -1000;
        if (n == 0) continue;
        outstanding--;
        if (wc.status != IBV_WC_SUCCESS && rc == 0) {
            fprintf(stderr, "send-queue wc status %d (%s) opcode %d\n",
                    wc.status, ibv_wc_status_str(wc.status), wc.opcode);
            rc = -(int)wc.status;
        }
    }
    return rc;
}

/* Send one RPC-over-RDMA message and drain the send queue. RC ordering
 * guarantees any RDMA Writes posted before this Send are placed in the
 * client's memory before the client sees the Send. */
int rshim_send(const unsigned char *msg, unsigned int len) {
    if (len > SEND_SZ) return -1;
    memcpy(send_buf, msg, len);
    struct ibv_sge sge = { .addr = (uintptr_t)send_buf, .length = len,
                           .lkey = send_mr->lkey };
    struct ibv_send_wr wr = { .sg_list = &sge, .num_sge = 1,
                              .opcode = IBV_WR_SEND,
                              .send_flags = IBV_SEND_SIGNALED };
    struct ibv_send_wr *bad;
    if (ibv_post_send(conn->qp, &wr, &bad)) return -2;
    outstanding++;
    return rshim_drain() == 0 ? 0 : -3;
}
