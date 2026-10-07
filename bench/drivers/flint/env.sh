# Sourced by the bench runners. spdk_tgt = the SPDK target (reactor +
# NVMe-oF); csi-driver = node agent and controller; flint-nfs-server = the
# NFS/pNFS server (RWX only).
CPU_PATTERNS='spdk=spdk_tgt,agent=csi-driver,nfs=flint-nfs-server'
SC_R1=flint-bench-r1
SC_R3=flint-bench-r3
DEFAULT_CORES=1   # the chart's reactor mask (0x1): Pass A and Pass B coincide
