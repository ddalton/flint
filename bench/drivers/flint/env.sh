# Sourced by the bench runners. spdk_tgt = the SPDK target (reactor +
# NVMe-oF); csi-driver = node agent and controller; flint-nfs-server = the
# NFS/pNFS server (RWX only).
CPU_PATTERNS='spdk=spdk_tgt,agent=csi-driver,nfs=flint-nfs-server'
SC_R1=flint-bench-r1
SC_R3=flint-bench-r3
DEFAULT_CORES=1   # the chart's reactor mask (0x1): Pass A and Pass B coincide
# Busy/idle ticks of spdk_tgt's reactors (SPDK renames its main thread
# reactor_0, so match the cmdline, bracketed so it cannot match this shell),
# read over its RPC socket from inside
# its own mount namespace (where rpc.py and python3 live).
REACTOR_TICKS='p=$(pgrep -o -f "[b]in/spdk_tgt ") && nsenter -t "$p" -m -- python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock framework_get_reactors | nsenter -t "$p" -m -- python3 -c "import json,sys; r=json.load(sys.stdin)[\"reactors\"]; print(sum(x[\"busy\"] for x in r), sum(x[\"idle\"] for x in r), len(r))"'
