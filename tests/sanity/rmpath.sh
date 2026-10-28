#!/bin/sh
# csi-sanity --csi.removemountpathcmd / --csi.removestagingpathcmd, run
# inside the kind node with the path as its only argument. csi-sanity's own
# default is os.Remove, so a file the driver left inside the path fails the
# NEXT spec's setup (mkdir: file exists) and cascades. This logs what was
# left, then removes it -- unless something under it is still mounted, which
# it reports and leaves (rm -rf through a live mount would delete data).
p="$1"
log=/root/rmpath.log
[ -e "$p" ] || exit 0
mounted=$(awk -v p="$p" '$2 == p || index($2, p "/") == 1 {print $2}' /proc/mounts)
if [ -n "$mounted" ]; then
  echo "$(date -u +%H:%M:%S) STILL-MOUNTED $p: $(echo $mounted)" >> "$log"
  exit 1
fi
left=$(find "$p" -mindepth 1 2>/dev/null | tr '\n' ' ')
[ -n "$left" ] && echo "$(date -u +%H:%M:%S) LEFTOVER $p: $left" >> "$log"
rm -rf "$p"
