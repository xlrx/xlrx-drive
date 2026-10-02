#!/bin/sh
# DS918+ spike (M0): measures and verifies on the NAS what the plan assumes. Does not modify any user data.
#   sudo sh spikes/ds918/run.sh [test folder with real files, e.g. /volume1/homes/klaus/Drive]
# Result: spikes/ds918/results-<date>.md (please commit it or send it to me).
set -u
cd "$(dirname "$0")"
SAMPLE="${1:-}"
IMAGE="${XLRX_IMAGE:-ghcr.io/xlrx/xlrx-drive-server:latest}"
PUID="$(id -u xlrx 2>/dev/null || echo)"
PGID="$(id -g xlrx 2>/dev/null || echo)"
OUT="results-$(date +%Y-%m-%d).md"
STATE=/volume1/xlrx-state/spike
HOMEDIR=/volume1/homes/xlrx/spike
DB=/volume1/xlrx-db/spike

say() { printf '%s\n' "$*" | tee -a "$OUT"; }
sec() { say ""; say "## $*"; say ""; }
run() { say '```'; say "\$ $*"; sh -c "$*" 2>&1 | tee -a "$OUT"; say '```'; }
as_xlrx() { docker run --rm -u "$PUID:$PGID" -v /volume1:/mnt/volume1 debian:bookworm-slim sh -c "$1"; }

: > "$OUT"
say "# DS918+-Spike $(date -Iseconds)"
[ -n "$PUID" ] || { say "Benutzer xlrx fehlt (deploy/synology.md, Schritt 1)."; exit 1; }

sec "System"
run "uname -r; cat /etc.defaults/VERSION 2>/dev/null | grep -E 'productversion|buildnumber'"
run "grep -m1 'model name' /proc/cpuinfo; nproc; free -m | head -2"
run "grep -o -w -E 'avx|avx2|sse4_2|popcnt|sha_ni' /proc/cpuinfo | sort | uniq -c"
run "sysctl fs.inotify.max_user_watches net.core.rmem_max"
run "btrfs --version; mount | grep ' /volume1 '"

sec "Reflink zwischen Subvolumes im selben Mount (PLAN 4.1/4.2)"
mkdir -p "$STATE" "$HOMEDIR" && chown xlrx "$STATE" "$HOMEDIR"
run "dd if=/dev/urandom of=$HOMEDIR/a bs=1M count=256 status=none && chown xlrx $HOMEDIR/a && echo ok"
say "Als Benutzer xlrx im Container, /volume1 als ein Mount:"
run "docker run --rm -u $PUID:$PGID -v /volume1:/mnt/volume1 debian:bookworm-slim sh -c 'cp --reflink=always /mnt/volume1/homes/xlrx/spike/a /mnt/volume1/xlrx-state/spike/a && echo REFLINK_OK'"
run "btrfs filesystem du -s $HOMEDIR/a $STATE/a 2>/dev/null || du -sh $HOMEDIR/a $STATE/a"
say "Gegenprobe ohne gemeinsamen Mount (erwartet: Fehler 'Invalid cross-device link'):"
run "docker run --rm -u $PUID:$PGID -v $HOMEDIR:/a -v $STATE:/b debian:bookworm-slim cp --reflink=always /a/a /b/b"

sec "Rechte: Kernel setzt Synology-ACLs auch im Container durch (PLAN 4.1)"
say "Eine Freigabe ohne Rechte für xlrx (sollte 'Permission denied' melden):"
for d in /volume1/photo /volume1/web /volume1/homes/admin; do
  [ -d "$d" ] && run "docker run --rm -u $PUID:$PGID -v /volume1:/mnt/volume1 debian:bookworm-slim ls /mnt$d"
done
say "Neue Datei als xlrx – erbt sie die ACL des Ordners (für SMB-Nutzer bearbeitbar)?"
as_xlrx "touch /mnt/volume1/homes/xlrx/spike/neu.txt" >/dev/null 2>&1
run "synoacltool -get $HOMEDIR | head -20; synoacltool -get $HOMEDIR/neu.txt | head -20"

sec "NOCOW für xlrx-db (PLAN 13.2)"
mkdir -p "$DB" && touch "$DB/test"
run "lsattr -d /volume1/xlrx-db; lsattr $DB/test"
say "Ein 'C' bedeutet NOCOW. Fehlt es: leeren Ordner mit 'chattr +C /volume1/xlrx-db/postgres' markieren (vor dem ersten Start)."

sec "Hilfsdateien von Synology Drive (PLAN 4.4)"
run "find /volume1/homes/*/Drive /volume1/*/ -maxdepth 4 \\( -name '.Synology*' -o -name '@*' -o -name '#*' -o -name '.~*' -o -name '*.synotemp' \\) 2>/dev/null | sed 's|.*/||' | sort | uniq -c | sort -rn | head -30"

sec "Passwort-Prüfung (argon2id, Ziel ~250 ms)"
for p in "19456 2 1" "32768 2 1" "47104 1 1" "65536 2 1"; do
  run "docker run --rm $IMAGE bench-argon2 $p"
done

if [ -n "$SAMPLE" ]; then
  sec "Scannen und Hashen echter Daten (FastCDC + BLAKE3, PLAN 5.8)"
  run "docker run --rm -u $PUID:$PGID -v $SAMPLE:/data:ro --entrypoint hashdir $IMAGE /data"
fi

sec "Aufräumen"
rm -rf "$STATE" "$HOMEDIR" "$DB"
say "Testdateien entfernt."
echo
echo "Fertig: $(pwd)/$OUT"
