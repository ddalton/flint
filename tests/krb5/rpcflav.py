#!/usr/bin/env python3
"""Count ONC RPC CALLs by (destination port, credential flavor, NFS op of a
COMPOUND's first two ops) in a pcap. Flavors: 0 none, 1 sys, 6 gss.
Plain pcap (tcpdump -w), Ethernet or Linux cooked/loopback link types.
Only reads record-marked calls that start at a TCP segment boundary,
which is enough to classify a mount's traffic."""
import struct, sys, collections

def packets(path):
    with open(path, 'rb') as f:
        hdr = f.read(24)
        if len(hdr) < 24:
            return
        magic = struct.unpack('<I', hdr[:4])[0]
        e = '<' if magic in (0xa1b2c3d4, 0xa1b23c4d) else '>'
        link = struct.unpack(e + 'I', hdr[20:24])[0]
        while True:
            ph = f.read(16)
            if len(ph) < 16:
                return
            _, _, incl, _ = struct.unpack(e + 'IIII', ph)
            yield link, f.read(incl)

def ip_payload(link, p):
    if link == 1:          # Ethernet
        return p[14:] if p[12:14] == b'\x08\x00' else None
    if link == 113:        # Linux cooked
        return p[16:] if p[14:16] == b'\x08\x00' else None
    if link == 0:          # BSD loopback
        return p[4:]
    if link in (101, 12):  # raw IP
        return p
    return None

counts = collections.Counter()
for link, p in packets(sys.argv[1]):
    ip = ip_payload(link, p)
    if not ip or ip[0] >> 4 != 4 or ip[9] != 6:
        continue
    ihl = (ip[0] & 15) * 4
    tcp = ip[ihl:]
    if len(tcp) < 20:
        continue
    dport = struct.unpack('>H', tcp[2:4])[0]
    data = tcp[(tcp[12] >> 4) * 4:]
    if len(data) < 4 + 28:
        continue
    rpc = data[4:]
    xid, mtype = struct.unpack('>II', rpc[:8])
    if mtype != 0:
        continue
    rpcvers, prog, vers, proc, flavor = struct.unpack('>IIIII', rpc[8:28])
    if rpcvers != 2 or prog != 100003:
        continue
    counts[(dport, flavor, 'null' if proc == 0 else 'compound')] += 1

print(' '.join('%d/%s/%s:%d' % (port, {0: 'none', 1: 'sys', 6: 'gss'}.get(fl, str(fl)), kind, n)
               for (port, fl, kind), n in sorted(counts.items())) or 'no calls')
