# xlrx-drive

Selbst gehostetes „Google Drive“ für die Synology: Web-UI mit Startseite, Aktivitätsstream und
Volltext-/KI-Suche, Teilen zwischen Benutzern, effizienter und zuverlässiger Sync, Mac- und iOS-App.

- Umsetzungsplan: [docs/PLAN.md](docs/PLAN.md)
- Sync-Engine (Regeln, Sicherheitsnetze, Simulation): [docs/adr/0001-sync-engine.md](docs/adr/0001-sync-engine.md)
- Einrichtung auf der Synology: [deploy/synology.md](deploy/synology.md) · Messungen auf dem DS918+: [spikes/ds918](spikes/ds918)

## Stand

| Teil | Inhalt |
|---|---|
| `crates/xlrx-chunk` | FastCDC + BLAKE3, Hash-Cache mit Schutz vor groben Zeitstempeln |
| `crates/xlrx-sync` | Sync-Engine (sans-IO, drei Bäume, inkrementelle Planung) |
| `crates/xlrx-sim` | Deterministischer Simulator: Abstürze, Netzfehler, späte Ergebnisse, SMB-Namensvarianten, grobe Zeitstempel |
| `crates/xlrx-server` | Konten und Anmeldung: Passwort + TOTP oder Passkey, Wiederherstellungscodes, Step-up, Verwaltung, Audit-Log |
| `web/` | SvelteKit-App: Anmeldung, Einrichtung, Sicherheit, Verwaltung |
| `deploy/` | Dockerfile, compose (PostgreSQL 18 + pgvector, Server, Caddy mit eigener IP und HTTP/3) |

## Entwickeln

```sh
cargo test --workspace                                  # inkl. Simulator-Seeds (XLRX_SIM_SEEDS=…)
XLRX_TEST_DATABASE_URL=postgres://postgres@localhost/postgres cargo test -p xlrx-server
cargo run --release -p xlrx-sim -- --strict --count 10000 --ci --exact-names 400
cd web && pnpm install && pnpm run check && pnpm run build
DATABASE_ADMIN_URL=postgres://postgres@localhost/postgres web/e2e/run.sh   # Browser-Test
```
