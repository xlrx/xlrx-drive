# xlrx-drive auf der Synology einrichten (DS918+, DSM 7.2)

Stand M1 (in Arbeit): Anmeldung, Konten und Verwaltung laufen; „Meine Ablage“ (der Synology-Drive-Ordner `homes/NAME/Drive`) lässt sich im Web durchsuchen und herunterladen. Sync und Suche folgen.
Router, DNS und die Übernahme der DSM-Proxy-Regeln erledigst du selbst (PLAN 15.3) – hier steht, was dafür nötig ist.

## 1. Benutzer und Freigaben

1. **DSM-Benutzer `xlrx`** anlegen (Systemsteuerung → Benutzer und Gruppe), kein Administrator, Anmeldung an DSM-Diensten verweigern.
2. **Freigaben** (Systemsteuerung → Freigegebener Ordner):
   - `xlrx-state` – im Netzwerk verbergen, Datenprüfsumme **an**. Versionen, Papierkorb, Dumps (ab M1).
   - `xlrx-db` – im Netzwerk verbergen, **Datenprüfsumme aus** (dann schreibt Btrfs dort ohne Copy-on-Write; Postgres prüft selbst mit Datenprüfsummen). Ob das wirklich NOCOW ergibt, misst der Spike (`spikes/ds918/run.sh`).
3. **Rechte** direkt für den Benutzer `xlrx` (nicht nur über Gruppen): Lesen/Schreiben auf `xlrx-state`, `xlrx-db`, `docker` und `homes` (später auch auf die eingebundenen Team-Ordner). Die xlrx-Konten heißen wie die DSM-Konten: „Meine Ablage“ von `klaus` ist `/volume1/homes/klaus/Drive`.
4. Per SSH die IDs ermitteln und Ordner anlegen:

   ```sh
   id xlrx                       # → PUID (uid) und PGID (gid) für deploy/.env
   sudo mkdir -p /volume1/xlrx-db/postgres /volume1/docker/xlrx/caddy/{data,config}
   sudo chown -R xlrx:users /volume1/xlrx-db/postgres
   ```

## 2. Projekt ablegen

```sh
cd /volume1/docker/xlrx
git clone https://github.com/xlrx/xlrx-drive.git    # oder den Ordner deploy/ hierher kopieren
cd xlrx-drive/deploy
cp .env.example .env && vi .env                    # Host, IDs, Netz, Caddy-IP
```

**Passkey-Domain (`XLRX_RP_ID`):** vor dem ersten Passkey festlegen, später nicht mehr ändern (sonst werden alle
Passkeys ungültig). Empfohlen ist die Hauptdomain (`example.de`).

**Caddy-IP:** eine freie Adresse im Heimnetz **außerhalb** des DHCP-Bereichs des Routers.
`LAN_PARENT` ist `ovs_eth0`, wenn Open vSwitch aktiv ist (z.B. durch Virtual Machine Manager), sonst `eth0` (`ip -br link` zeigt es).

## 3. Geheimnisse

```sh
cd /volume1/docker/xlrx/xlrx-drive/deploy
umask 077
openssl rand -base64 32 | tr -d '/+=\n' > secrets/pg_password
printf 'postgres://xlrx:%s@postgres:5432/xlrx' "$(cat secrets/pg_password)" > secrets/database_url
sudo docker run --rm ghcr.io/xlrx/xlrx-drive-server:latest gen-secret > secrets/xlrx_secret_key
sudo chown xlrx secrets/*
```

`xlrx_secret_key` verschlüsselt die TOTP-Geheimnisse. **Zusätzlich offline sichern** (Passwortmanager). Ohne ihn
funktionieren Authenticator-Apps nach einer Wiederherstellung nicht mehr (Passkeys schon); dann müssten alle ihre
zweiten Faktoren neu einrichten. Er gehört bewusst **nicht** in die Datenbank und nicht ins Hyper-Backup der Datenbank.

## 4. Boot-Aufgabe

Systemsteuerung → Aufgabenplaner → Erstellen → Ausgelöste Aufgabe → Benutzerdefiniertes Skript, Ereignis „Hochfahren“, Benutzer `root`:

```sh
sysctl -w fs.inotify.max_user_watches=1048576 net.core.rmem_max=7500000 net.core.wmem_max=7500000
```

(inotify für viele Ordner ab M1, größere UDP-Puffer für HTTP/3.)

## 5. Starten

Container Manager → Projekt → Erstellen → Pfad `/volume1/docker/xlrx/xlrx-drive/deploy`, vorhandene `docker-compose.yml` verwenden.
Oder per SSH:

```sh
sudo docker compose up -d
sudo docker compose logs -f server
```

Das Image wird aus GHCR geladen. Zum selbst Bauen: `sudo docker compose build server` (dauert auf dem J3455 lange; besser die fertigen Images).

## 6. Router und DNS

- **DNS:** `drive.example.de` → öffentliche IP (DynDNS, z.B. über DSM → Externer Zugriff → DDNS als CNAME).
- **Portweiterleitung:** TCP 80, TCP 443 und **UDP 443** (HTTP/3) auf die **Caddy-IP**.
- **Im LAN:** Split-DNS, damit `drive.example.de` zu Hause direkt auf die Caddy-IP zeigt.
- Das NAS selbst erreicht die Caddy-IP nicht (Eigenheit von macvlan). Für xlrx ist das egal.

Caddy holt das Zertifikat beim ersten Aufruf selbst.

## 7. Erstes Konto

```sh
sudo docker compose exec server xlrx-server create-user klaus "Klaus" --admin
```

Den ausgegebenen Link öffnen: Passwort festlegen, Passkey (Face ID/Touch ID) oder Authenticator-App einrichten,
Wiederherstellungscodes sichern. Weitere Konten legst du danach in der Web-Oberfläche unter **Verwaltung** an.

## 8. Prüfen

```sh
curl -sI https://drive.example.de | grep -i alt-svc       # h3 wird angeboten
curl --http3 -sI https://drive.example.de/healthz          # (curl mit HTTP/3, z.B. Homebrew)
```

Anmeldung nur mit Passwort darf nicht gehen; nur Passwort + Code oder Passkey.

## 9. Passwort-Prüfung kalibrieren

```sh
sudo docker compose exec server xlrx-server bench-argon2 19456 2 1
sudo docker compose exec server xlrx-server bench-argon2 32768 2 1
```

Werte wählen, die ~250 ms ergeben, in `.env` eintragen (`XLRX_ARGON2_M_KIB`, `XLRX_ARGON2_T`), `docker compose up -d`.
Bestehende Passwörter bleiben gültig; sie tragen ihre Parameter in sich.

## 10. DSM-Reverse-Proxy ablösen

1. Bestehende Regeln (Systemsteuerung → Anmeldeportal → Erweitert → Reverse Proxy) als Blöcke in `Caddyfile` übernehmen.
2. Caddy läuft parallel; über die Caddy-IP testen (`curl --resolve fotos.example.de:443:192.168.1.20 https://fotos.example.de`).
3. Portweiterleitung und Split-DNS auf die Caddy-IP umstellen. Zurück geht jederzeit: Weiterleitung wieder auf die NAS-IP.

## Betrieb

- **Update:** `sudo docker compose pull && sudo docker compose up -d` – Migrationen laufen beim Start.
- **Datenbank sichern (bis M1 automatisch):**
  `sudo docker compose exec -T postgres pg_dump -U xlrx -Fc xlrx > /volume1/xlrx-state/dumps/xlrx-$(date +%F).dump`
- **Admin ausgesperrt** (Handy weg, keine Codes):
  `sudo docker compose exec server xlrx-server reset-factors klaus` → neuer Einrichtungslink.
- **Neuer Einrichtungslink** (abgelaufen): `xlrx-server invite NAME` bzw. in der Verwaltung.
