#!/bin/bash
# Keep the TLC unit (m3big) frozen while flint-53 runs its kind A/B test:
# the M4 record chain thaws it when its control ends; re-freeze it then,
# and keep it frozen until /mnt/nvme/leanp1-m4-2026-10-07/release-tlc exists.
R=/mnt/nvme/leanp1-m4-2026-10-07/release-tlc
until test -f $R; do
  [ "$(systemctl --user show m3big -p FreezerState --value)" = running ] && systemctl --user freeze m3big
  sleep 5
done
systemctl --user thaw m3big
