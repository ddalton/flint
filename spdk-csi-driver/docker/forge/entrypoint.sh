#!/bin/sh
# fcgiwrap and nginx, in one container, with the socket in /tmp so the
# repository volume carries nothing but the repository.
set -eu

rm -f /tmp/fcgiwrap.sock
spawn-fcgi -s /tmp/fcgiwrap.sock -M 0666 -F "${FCGIWRAP_CHILDREN:-4}" -- /usr/bin/fcgiwrap

# nginx is the foreground process: if it dies the container restarts,
# and the syncer beside it keeps its lease either way.
exec nginx -c /etc/nginx/nginx.conf -g 'daemon off;'
