#!/usr/bin/env bash
# End-to-end test: fresh database, server with the built web app, Playwright.
#   DATABASE_ADMIN_URL=postgres://postgres@127.0.0.1:5432/postgres web/e2e/run.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${DATABASE_ADMIN_URL:?DATABASE_ADMIN_URL fehlt}"
db="xlrx_e2e_$$"
psql "$DATABASE_ADMIN_URL" -qc "CREATE DATABASE $db"
data="$(mktemp -d)"
s3dir="$(mktemp -d)"
trap 'kill ${server:-0} ${s3:-0} ${ai:-0} 2>/dev/null || true; psql "$DATABASE_ADMIN_URL" -qc "DROP DATABASE IF EXISTS $db WITH (FORCE)" || true; rm -rf "$data" "$s3dir"' EXIT

export DATABASE_URL="${DATABASE_ADMIN_URL%/*}/$db"
export XLRX_PUBLIC_URL=http://localhost:8080
export XLRX_BIND=127.0.0.1:8080
export XLRX_WEB_DIR="$PWD/web/.output/public"
export XLRX_SECRET_KEY="$(target/release/xlrx-server gen-secret)"
export XLRX_ARGON2_M_KIB=1024 XLRX_ARGON2_T=1
# Stand-in for the NAS volume; the test puts files into homes/admin/Drive.
export XLRX_DATA_DIR="$data" XLRX_E2E_DATA="$data"

# The outside cache against a local S3 server, if installed
# (cargo install s3s-fs --version 0.17.0 --features binary --locked).
if command -v s3s-fs >/dev/null; then
  mkdir -p "$s3dir/xlrx-cache"
  s3s-fs --host 127.0.0.1 --port 8014 --access-key e2ekey --secret-key e2esecret "$s3dir" >/dev/null 2>&1 &
  s3=$!
  export XLRX_S3_BUCKET=xlrx-cache XLRX_S3_ENDPOINT=http://127.0.0.1:8014 XLRX_S3_REGION=us-east-1
  export XLRX_S3_ACCESS_KEY=e2ekey XLRX_S3_SECRET_KEY=e2esecret XLRX_S3_MIN_MB=0 XLRX_S3_UPLOAD_MBIT=0
  # The test plays "away from home" with X-Forwarded-For.
  export XLRX_TRUST_PROXY=1 XLRX_E2E_S3=1
fi

# The AI search against a stand-in for the provider (web/e2e/fake-ai.mjs).
node web/e2e/fake-ai.mjs &
ai=$!
export XLRX_AI_URL=http://127.0.0.1:8015/v1 XLRX_AI_KEY=e2e-schluessel XLRX_AI_EMBED_DIM=256
# The stand-in's word vectors are far apart even when they match.
export XLRX_AI_MAX_DISTANCE=0.9

XLRX_SETUP_URL="$(target/release/xlrx-server create-user admin "Admin" --admin | tail -1)"
export XLRX_SETUP_URL
target/release/xlrx-server &
server=$!
for _ in $(seq 50); do curl -sf http://localhost:8080/healthz >/dev/null && break; sleep 0.2; done
(cd web && pnpm exec playwright test "$@")
