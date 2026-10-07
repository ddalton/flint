# Sourced by the bench runners: which processes are Flint's storage path.
# spdk_tgt = the SPDK target (reactor + NVMe-oF); csi-driver = the node
# agent and controller; flint-nfs-server = the NFS/pNFS server (RWX only).
CPU_PATTERNS='spdk=spdk_tgt,agent=csi-driver,nfs=flint-nfs-server'
