# Sourced by the bench runners. spdk_tgt = the v2 data engine (inside the
# instance-manager pod); instance-manager / longhorn-manager = its control
# processes; csi = the CSI plugin.
CPU_PATTERNS='spdk=spdk_tgt,im=longhorn-instance-manager,mgr=longhorn-manager,csi=longhorn-csi-plugin'
SC_R1=longhorn-v2-bench-r1
SC_R3=longhorn-v2-bench-r3
DEFAULT_CORES=2   # data-engine-cpu-mask default 0x3
