#!/usr/bin/env bash
# Development: PostgreSQL (Docker, or DATABASE_URL), xlrx-server via cargo, web app built or with
# hot reload. Prints a setup link for the first admin account.
#
#   scripts/dev.sh          # server + built web app on http://localhost:8080
#   scripts/dev.sh --hot    # additionally Nuxt dev server with hot reload on http://localhost:3000
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -z "${DATABASE_URL:-}" ]; then
	docker compose -f deploy/docker-compose.local.yml up -d --wait postgres
	export DATABASE_URL=postgres://xlrx:xlrx@127.0.0.1:54320/xlrx
fi
export XLRX_PUBLIC_URL=http://localhost:8080
export XLRX_EXTRA_ORIGINS=http://localhost:3000
export XLRX_BIND=127.0.0.1:8080
export XLRX_SECRET_KEY=eGxyeC1sb2NhbC1kZXZlbG9wbWVudC1vbmx5LWtleSE=
export XLRX_ARGON2_M_KIB=8192 XLRX_ARGON2_T=1
export XLRX_WEB_DIR="$PWD/web/.output/public"
# Stand-in for the NAS volume: "My Drive" of account NAME is local-data/homes/NAME/Drive.
export XLRX_DATA_DIR="${XLRX_DATA_DIR:-$PWD/local-data}"
mkdir -p "$XLRX_DATA_DIR"
export RUST_LOG="${RUST_LOG:-info,sqlx=warn}"

(cd web && pnpm install --silent && pnpm run build >/dev/null)
cargo build -q -p xlrx-server
bin=target/debug/xlrx-server

# First run: create an admin account and print its setup link.
if ! psql "$DATABASE_URL" -tAc "select 1 from users limit 1" 2>/dev/null | grep -q 1; then
	"$bin" migrate >/dev/null
	"$bin" create-user admin "Admin" --admin
fi

"$bin" &
server=$!
trap 'kill $server 2>/dev/null' EXIT
if [ "${1:-}" = "--hot" ]; then
	(cd web && pnpm dev --port 3000)
else
	echo "xlrx läuft auf http://localhost:8080 (Strg+C beendet), Dateien in $XLRX_DATA_DIR"
	wait $server
fi
