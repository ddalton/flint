/* All verbs/rdma-cm state lives here so Rust never touches an ibv
 * struct (their layouts come from the system headers — hand-writing
 * repr(C) mirrors is the classic FFI landmine). The surface Rust sees
 * is five blocking calls over plain buffers.
 *
 * Single-connection server, deliberately: the prototype's question is
 * "does the kernel's xprtrdma accept our RPC-over-RDMA framing", not
 * "does it scale".
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <arpa/inet.h>
#include <rdma/rdma_cma.h>
#include <infiniband/verbs.h>

#define NRECV 32
#define RECV_SZ 8192
#define SEND_SZ 8192

static struct rdma_event_channel *ec;
static struct rdma_cm_id *listen_id, *conn;
static struct ibv_pd *pd;
static struct ibv_cq *cq;
static char *recv_buf, *send_buf;
static struct ibv_mr *recv_mr, *send_mr;
static int established;

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

int rshim_listen(unsigned short port) {
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    ec = rdma_create_event_channel();
    if (!ec) return -1;
    if (rdma_create_id(ec, &listen_id, NULL, RDMA_PS_TCP)) return -2;
    if (rdma_bind_addr(listen_id, (struct sockaddr *)&a)) return -3;
    if (rdma_listen(listen_id, 4)) return -4;
    return 0;
}

/* Block until a client connection is fully ESTABLISHED. */
int rshim_accept(void) {
    struct rdma_cm_event *ev;
    /* The poll loop leaves the CM channel non-blocking; restore
     * blocking mode so we park here instead of hot-spinning EAGAIN. */
    fcntl(ec->fd, F_SETFL, 0);
    established = 0;
    for (;;) {
        if (rdma_get_cm_event(ec, &ev)) return -1;
        if (ev->event == RDMA_CM_EVENT_CONNECT_REQUEST) {
            struct rdma_conn_param cp;
            memcpy(&cp, &ev->param.conn, sizeof(cp));
            conn = ev->id;
            rdma_ack_cm_event(ev);

            pd = ibv_alloc_pd(conn->verbs);
            cq = ibv_create_cq(conn->verbs, 2 * NRECV, NULL, NULL, 0);
            if (!pd || !cq) return -2;
            struct ibv_qp_init_attr qa = {
                .send_cq = cq, .recv_cq = cq,
                .cap = { .max_send_wr = NRECV, .max_recv_wr = NRECV,
                         .max_send_sge = 2, .max_recv_sge = 1 },
                .qp_type = IBV_QPT_RC,
            };
            if (rdma_create_qp(conn, pd, &qa)) return -3;

            recv_buf = aligned_alloc(4096, (size_t)NRECV * RECV_SZ);
            send_buf = aligned_alloc(4096, SEND_SZ);
            recv_mr = ibv_reg_mr(pd, recv_buf, (size_t)NRECV * RECV_SZ,
                                 IBV_ACCESS_LOCAL_WRITE);
            send_mr = ibv_reg_mr(pd, send_buf, SEND_SZ, 0);
            if (!recv_mr || !send_mr) return -4;
            for (int i = 0; i < NRECV; i++)
                if (post_recv(i)) return -5;

            /* Mirror the client's depths; it never RDMA-Reads us but
             * being generous is harmless. */
            struct rdma_conn_param acc = {
                .responder_resources = cp.responder_resources,
                .initiator_depth = cp.initiator_depth,
                .retry_count = 7, .rnr_retry_count = 7,
            };
            if (rdma_accept(conn, &acc)) return -6;
        } else if (ev->event == RDMA_CM_EVENT_ESTABLISHED) {
            rdma_ack_cm_event(ev);
            established = 1;
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
 * -2 on disconnect. *buf/*len describe the message bytes. */
int rshim_wait_recv(unsigned char **buf, unsigned int *len) {
    struct ibv_wc wc;
    for (;;) {
        int n = ibv_poll_cq(cq, 1, &wc);
        if (n < 0) return -1;
        if (n == 1) {
            if (wc.status != IBV_WC_SUCCESS) {
                fprintf(stderr, "wc status %d opcode %d\n", wc.status, wc.opcode);
                return -2;
            }
            if (wc.opcode == IBV_WC_RECV) {
                *buf = (unsigned char *)(recv_buf + wc.wr_id * (size_t)RECV_SZ);
                *len = wc.byte_len;
                return (int)wc.wr_id;
            }
            continue; /* send completion */
        }
        struct rdma_cm_event *ev;
        if (rdma_get_cm_event(ec, &ev) == 0) {
            int dis = ev->event == RDMA_CM_EVENT_DISCONNECTED;
            rdma_ack_cm_event(ev);
            if (dis) return -2;
        }
        usleep(50);
    }
}

int rshim_repost(int idx) { return post_recv(idx); }

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
    /* Wait for the send completion so send_buf can be reused. RECV
     * completions seen meanwhile are queued nowhere — with the
     * client's strict request/response cadence at M1 this is safe. */
    struct ibv_wc wc;
    for (;;) {
        int n = ibv_poll_cq(cq, 1, &wc);
        if (n == 1 && wc.opcode == IBV_WC_SEND)
            return wc.status == IBV_WC_SUCCESS ? 0 : -3;
        if (n == 1 && wc.opcode == IBV_WC_RECV) {
            /* Extremely unlikely at M1; drop with a loud note. */
            fprintf(stderr, "recv completion during send wait — dropped\n");
            post_recv((int)wc.wr_id);
        }
        if (n < 0) return -4;
        usleep(20);
    }
}
