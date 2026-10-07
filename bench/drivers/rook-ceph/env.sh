# Sourced by the bench runners. Every Ceph daemon on the node plus the RBD
# CSI plugin (kernel RBD work shows as kernel time, not in these processes).
CPU_PATTERNS='osd=ceph-osd,mon=ceph-mon,mgr=ceph-mgr,csi=cephcsi'
SC_R1=ceph-bench-r1
SC_R3=ceph-bench-r3
DEFAULT_CORES=   # no polling cores: Pass B does not apply
