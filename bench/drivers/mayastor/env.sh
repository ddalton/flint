# Sourced by the bench runners. io-engine = Mayastor's SPDK data plane;
# csi-node / agent-* = its CSI node plugin and control-plane agents.
CPU_PATTERNS='engine=io-engine,csi=csi-node,agents=agent-core|agent-ha'
SC_R1=mayastor-bench-r1
SC_R3=mayastor-bench-r3
DEFAULT_CORES=2   # chart default io_engine.cpuCount
