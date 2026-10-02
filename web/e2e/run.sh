#!/usr/bin/env bash
# End-to-end test: fresh database, server with the built web app, Playwright.
#   DATABASE_ADMIN_URL=postgres://postgres@127.0.0.1:5432/postgres web/e2e/run.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${DATABASE_ADMIN_URL:?DATABASE_ADMIN_URL fehlt}"
db="xlrx_e2e_$$"
psql "$DATABASE_ADMIN_URL" -qc "CREATE DATABASE $db"
data="$(mktemp -d)"
trap 'kill ${server:-0} 2>/dev/null || true; psql "$DATABASE_ADMIN_URL" -qc "DROP DATABASE IF EXISTS $db WITH (FORCE)" || true; rm -rf "$data"' EXIT

export DATABASE_URL="${DATABASE_ADMIN_URL%/*}/$db"
export XLRX_PUBLIC_URL=http://localhost:8080
export XLRX_BIND=127.0.0.1:8080
export XLRX_WEB_DIR="$PWD/web/.output/public"
export XLRX_SECRET_KEY="$(target/release/xlrx-server gen-secret)"
export XLRX_ARGON2_M_KIB=1024 XLRX_ARGON2_T=1
# Stand-in for the NAS volume; the test puts files into homes/admin/Drive.
export XLRX_DATA_DIR="$data" XLRX_E2E_DATA="$data"
# The test checks "Neu einlesen"; the watcher (tested in the server's own tests) would race it.
export XLRX_WATCH=0

XLRX_SETUP_URL="$(target/release/xlrx-server create-user admin "Admin" --admin | tail -1)"
export XLRX_SETUP_URL
target/release/xlrx-server &
server=$!
for _ in $(seq 50); do curl -sf http://localhost:8080/healthz >/dev/null && break; sleep 0.2; done
(cd web && pnpm exec playwright test "$@")
