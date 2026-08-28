#!/usr/bin/env python3
"""Raw RPC probes for the GSS negative legs.

Two things the kernel client will never do for you:

  * `capture` — sit between the client and the server, forward everything,
    and keep a copy of one client->server RPC record that carried
    RPCSEC_GSS *data* (credential flavor 6, procedure 0 = DATA). That
    record is a complete, correctly-MICed call. Replaying it needs no key
    material, which is the point: a replay attacker has no key either.

  * `send` — open a fresh connection and put chosen bytes on it, optionally
    with one byte flipped, and report what comes back.

RPC-over-TCP frames every message with a 4-byte record mark whose top bit
is "last fragment" and whose low 31 bits are the length, so records are
self-delimiting and a proxy can split the stream without parsing XDR.
"""
import socket, struct, sys, threading, os

RECORD_LAST = 0x80000000

# ---- RPC call header offsets, for a message that has already been split
# off the stream (i.e. the 4-byte mark is gone):
#   xid(4) mtype(4) rpcvers(4) prog(4) vers(4) proc(4) then cred{flavor,len}
CRED_FLAVOR_OFF = 24
AUTH_RPCSEC_GSS = 6


def records(buf):
    """Split a byte buffer into complete RPC records; return (recs, rest)."""
    out, i = [], 0
    while len(buf) - i >= 4:
        mark = struct.unpack(">I", buf[i:i + 4])[0]
        n = mark & ~RECORD_LAST
        if len(buf) - i - 4 < n:
            break
        out.append(buf[i:i + 4 + n])
        i += 4 + n
    return out, buf[i:]


def is_gss_data(rec):
    """True if this record is an RPC CALL whose credential is RPCSEC_GSS
    with procedure DATA (the gss proc field sits after version)."""
    body = rec[4:]
    if len(body) < CRED_FLAVOR_OFF + 8:
        return False
    if struct.unpack(">I", body[4:8])[0] != 0:          # mtype must be CALL
        return False
    flavor = struct.unpack(">I", body[CRED_FLAVOR_OFF:CRED_FLAVOR_OFF + 4])[0]
    if flavor != AUTH_RPCSEC_GSS:
        return False
    clen = struct.unpack(">I", body[CRED_FLAVOR_OFF + 4:CRED_FLAVOR_OFF + 8])[0]
    cred = body[CRED_FLAVOR_OFF + 8:CRED_FLAVOR_OFF + 8 + clen]
    if len(cred) < 16:
        return False
    # rpc_gss_cred_vers_1 { version, proc, seq_num, service, handle }
    _ver, proc, seq, svc = struct.unpack(">IIII", cred[:16])
    return proc == 0 and seq > 0 and svc in (1, 2, 3)


def gss_seq(rec):
    body = rec[4:]
    clen = struct.unpack(">I", body[CRED_FLAVOR_OFF + 4:CRED_FLAVOR_OFF + 8])[0]
    cred = body[CRED_FLAVOR_OFF + 8:CRED_FLAVOR_OFF + 8 + clen]
    return struct.unpack(">I", cred[8:12])[0]


def capture(listen_port, upstream_port, out_path):
    """Forward one client connection to the server, saving the first
    RPCSEC_GSS DATA record seen going up."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", listen_port))
    srv.listen(8)
    saved = {"done": False}

    def one(client):
        up = socket.create_connection(("127.0.0.1", upstream_port))
        buf = b""

        def c2s():
            nonlocal buf
            try:
                while True:
                    d = client.recv(65536)
                    if not d:
                        break
                    up.sendall(d)
                    if not saved["done"]:
                        buf += d
                        recs, buf = records(buf)
                        for r in recs:
                            if is_gss_data(r) and not saved["done"]:
                                with open(out_path, "wb") as f:
                                    f.write(r)
                                saved["done"] = True
                                sys.stderr.write(
                                    "captured %d-byte GSS DATA record, seq=%d\n"
                                    % (len(r), gss_seq(r)))
                                sys.stderr.flush()
            except OSError:
                pass
            finally:
                try: up.shutdown(socket.SHUT_WR)
                except OSError: pass

        def s2c():
            try:
                while True:
                    d = up.recv(65536)
                    if not d:
                        break
                    client.sendall(d)
            except OSError:
                pass

        t = threading.Thread(target=c2s, daemon=True); t.start()
        s2c()

    while True:
        c, _ = srv.accept()
        threading.Thread(target=one, args=(c,), daemon=True).start()


def set_seq(rec, seq):
    """Rewrite the credential's seq_num, leaving the call verifier alone.

    The call verifier is a MIC over the seq_num (RFC 2203 §5.3.1), so a
    record whose seq is moved forward to an UNUSED number carries a MIC
    that no longer matches. That separates the two refusals: the replay
    window cannot explain this one, only checksum verification can.
    """
    body = bytearray(rec[4:])
    off = CRED_FLAVOR_OFF + 8 + 8          # into cred: version, proc, then seq
    body[off:off + 4] = struct.pack(">I", seq)
    return rec[:4] + bytes(body)


def send(port, path, flip=None, seq=None, timeout=8.0):
    """Send a recorded record on a fresh connection and report the reply."""
    rec = bytearray(open(path, "rb").read())
    if seq is not None:
        rec = bytearray(set_seq(bytes(rec), seq))
    if flip is not None:
        rec[flip] ^= 0xFF
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.sendall(bytes(rec))
    try:
        head = s.recv(4)
    except socket.timeout:
        print("NO_REPLY"); return
    if len(head) < 4:
        print("CLOSED"); return
    n = struct.unpack(">I", head)[0] & ~RECORD_LAST
    body = b""
    while len(body) < n:
        chunk = s.recv(n - len(body))
        if not chunk:
            break
        body += chunk
    # reply: xid(4) mtype(4) reply_stat(4) ...
    if len(body) < 12:
        print("SHORT"); return
    _xid, _mt, stat = struct.unpack(">III", body[:12])
    if stat == 1:                       # MSG_DENIED
        rej = struct.unpack(">I", body[12:16])[0]
        if rej == 1:                    # AUTH_ERROR
            print("DENIED auth_stat=%d" % struct.unpack(">I", body[16:20])[0])
        else:
            print("DENIED reject_stat=%d" % rej)
    else:                               # MSG_ACCEPTED
        vf_len = struct.unpack(">I", body[16:20])[0]
        off = 20 + ((vf_len + 3) & ~3)
        print("ACCEPTED accept_stat=%d" % struct.unpack(">I", body[off:off + 4])[0])


if __name__ == "__main__":
    if sys.argv[1] == "capture":
        capture(int(sys.argv[2]), int(sys.argv[3]), sys.argv[4])
    elif sys.argv[1] == "send":
        # send <port> <path> [--flip OFF | --seq N]
        flip = seq = None
        rest = sys.argv[4:]
        if rest and rest[0] == "--flip":
            flip = int(rest[1])
        elif rest and rest[0] == "--seq":
            seq = int(rest[1], 0)
        send(int(sys.argv[2]), sys.argv[3], flip, seq)
    elif sys.argv[1] == "seqof":
        print(gss_seq(open(sys.argv[2], "rb").read()))
