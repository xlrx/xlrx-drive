#!/bin/sh
# HTTP/3 (QUIC) vs. HTTP/2 throughput – from the Mac, once on the LAN and once on the go (hotspot).
#   1. On the NAS: create a test file (served only for the measurement):
#        sudo mkdir -p /volume1/docker/xlrx/caddy/spike
#        sudo dd if=/dev/urandom of=/volume1/docker/xlrx/caddy/spike/1g.bin bs=1M count=1024
#      and add this to the xlrx block in the Caddyfile (remove it again afterwards):
#        handle_path /_spike/* {
#            root * /srv/spike
#            file_server
#        }
#      plus, in docker-compose.yml under caddy: - ${CADDY_DIR}/spike:/srv/spike:ro
#   2. On the Mac (curl with HTTP/3, e.g. `brew install curl`):
#        sh spikes/ds918/transfer.sh drive.example.de
set -eu
HOST="$1"
CURL="${CURL:-$(brew --prefix curl 2>/dev/null)/bin/curl}"
[ -x "$CURL" ] || CURL=curl
URL="https://$HOST/_spike/1g.bin"
for proto in --http2 --http3-only; do
  for i in 1 2 3; do
    "$CURL" $proto -s -o /dev/null -w "$proto Lauf $i: %{speed_download} B/s, TTFB %{time_starttransfer}s, gesamt %{time_total}s\n" "$URL" ||
      echo "$proto Lauf $i: fehlgeschlagen"
  done
done
echo "Zum Vergleich die NAS-CPU während der Übertragung beobachten (DSM Ressourcen-Monitor oder 'top')."
