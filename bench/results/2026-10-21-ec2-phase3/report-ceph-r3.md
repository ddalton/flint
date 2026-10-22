
### idle (no I/O)

| driver | SC | storage cores | node busy cores (sum) |
|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 0.06 | 1.32 |

### iops_mixed70_4k

| driver | SC | reps | IOPS | MiB/s | read p50 µs | read p99 µs | read p99.9 µs | write p50 µs | write p99 µs | write p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 36,057 [35,747–36,415] | 141 [140–142] | 1,745 [1,745–1,843] | 9,110 [9,110–9,634] | 13,697 [13,697–14,221] | 5,472 [5,210–5,538] | 17,170 [17,170–17,433] | 26,083 [25,559–26,870] | 11.64 [11.62–11.72] | 3,077 [3,072–3,135] |

### iops_randread_4k

| driver | SC | reps | IOPS | MiB/s | read p50 µs | read p99 µs | read p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 68,988 [68,523–69,143] | 269 [268–270] | 1,004 [1,004–1,012] | 11,469 [11,338–11,600] | 13,697 [13,697–13,697] | 6.84 [6.80–6.93] | 10,011 [9,975–10,145] |

### iops_randwrite_4k

| driver | SC | reps | IOPS | MiB/s | write p50 µs | write p99 µs | write p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 17,305 [17,225–18,272] | 68 [67–71] | 6,455 [6,193–6,455] | 19,005 [18,219–19,005] | 27,656 [25,559–28,443] | 12.19 [12.16–12.30] | 1,424 [1,413–1,485] |

### lat_randread_4k

| driver | SC | reps | IOPS | MiB/s | read p50 µs | read p99 µs | read p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 2,400 [2,369–2,420] | 9 [9–9] | 399 [395–403] | 987 [979–1,028] | 9,241 [9,241–9,372] | 0.39 [0.38–0.39] | 6,170 [6,028–6,352] |

### lat_randwrite_4k

| driver | SC | reps | IOPS | MiB/s | write p50 µs | write p99 µs | write p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 839 [824–849] | 3 [3–3] | 881 [872–897] | 9,634 [9,503–9,765] | 16,712 [16,581–17,170] | 0.93 [0.90–0.94] | 903 [880–949] |

### seq_read_128k

| driver | SC | reps | IOPS | MiB/s | read p50 µs | read p99 µs | read p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 4,510 [4,501–4,518] | 564 [563–565] | 3,523 [3,523–3,523] | 7,766 [7,635–8,454] | 14,352 [14,221–14,483] | 1.07 [1.07–1.09] | 4,191 [4,149–4,206] |

### seq_read_1m

| driver | SC | reps | IOPS | MiB/s | read p50 µs | read p99 µs | read p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 1,311 [1,310–1,312] | 1,311 [1,310–1,312] | 6,717 [6,586–6,783] | 28,705 [28,443–28,967] | 34,341 [33,817–34,865] | 1.33 [1.33–1.34] | 984 [974–989] |

### seq_write_128k

| driver | SC | reps | IOPS | MiB/s | write p50 µs | write p99 µs | write p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 3,093 [3,047–3,146] | 387 [381–393] | 4,489 [4,489–4,555] | 14,221 [13,959–14,615] | 19,792 [18,743–20,316] | 4.62 [4.61–4.66] | 671 [660–675] |

### seq_write_1m

| driver | SC | reps | IOPS | MiB/s | write p50 µs | write p99 µs | write p99.9 µs | storage cores | IOPS/core |
|---|---|---|---|---|---|---|---|---|---|
| rook-ceph-a-r3 | ceph-bench-r3 | 3 | 745 [745–746] | 746 [745–746] | 20,316 [20,316–20,316] | 43,254 [42,205–43,778] | 51,118 [49,021–51,642] | 4.00 [3.97–4.00] | 187 [186–188] |
