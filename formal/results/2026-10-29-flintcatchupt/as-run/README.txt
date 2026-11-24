The FlintCatchupS model files AS RUN, one directory per revision, copied from the box
(10.0.0.249:/mnt/nvme/catchup-n-2026-10-28/ms*) on 2026-10-31; MD5SUMS lists every file.
Only the .tla and .cfg files; logs and traces are generated output and stay on the box.

  ms7   rev 7   FlintCatchupS 6859c997, Reach 7d5d053b   c21 (ms7p was a byte-identical copy for two parallel runners)
  ms8   rev 8   ab167ffe, Reach 3e9e01e2                 c22
  ms9   rev 9   c41c6986, Reach 01df6860                 c23
  ms10  rev 10  2a9567c7, Reach 2e192540                 c24
  ms11  rev 11  46c89207, Reach 2e192540                 c25
  ms12  rev 12  8e54566d, Reach 2e192540                 c26, c27 (EC2: its stage/formal/ms12 is identical), c28
  ms13  rev 13  41fa0652, FlintCatchupSSym 34243d06      c29 (SYMMETRY validation; rev 13 = rev 12 with ids that may be model values)

Reproducible from the kept generators (formal/gen-FlintCatchupS-rN.py, -cfg-rN.py, run on FlintCatchupT.tla
8ac3cf00), checked byte for byte 2026-10-31: the models of rev 8-13 and the cfgs of rev 9-12 (and the current
generator's 136 rev 12 cfgs).  NOT reproducible, kept here only: rev 7's model, Reach and 42 cfgs (no rev 7
generator was kept), rev 8's Reach (3e9e01e2) and its 10 Reach/identity cfgs.
