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
: > secrets/s3_access_key; : > secrets/s3_secret_key   # bleiben leer bis zum Außen-Beschleuniger (Abschnitt 14)
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

(inotify: xlrx überwacht jeden Ordner der Ablagen und bemerkt Änderungen über SMB oder File Station so in Sekunden – eine Überwachung pro Ordner. Größere UDP-Puffer für HTTP/3.)

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

## 10. Suche

Die Suche baut ihren Index beim ersten Start selbst auf und liest danach den Text der Dateien:
PDFs und gescannte Dokumente (Texterkennung mit Tesseract, Deutsch und Englisch) im Server-Container,
Word, Excel, PowerPoint, Pages, Numbers, Keynote, OpenDocument und Mails im Container `tika`.
Beide laufen nur im internen Netz ohne Internetzugang. Der Server schickt Dokumente ausschließlich an
Adressen im Heimnetz (`XLRX_TIKA_URL`); eine Adresse außerhalb lehnt er beim Start ab. Darum dürfen auch
„Nur lokal“-Ordner gelesen werden.

Den Fortschritt zeigt die **Verwaltung** unter „Suche“: wie weit der Index ist, wie viele Inhalte gelesen
sind und was noch wartet. Beim ersten Durchlauf über alle Dateien braucht vor allem die Texterkennung
Zeit (einige Sekunden je Seite, höchstens 30 Seiten je Dokument). Was zuletzt geändert wurde, kommt zuerst.
Ein zweiter Durchlauf parallel: `XLRX_EXTRACT_WORKERS=2` in `.env` (mehr nicht, sonst wird das NAS träge).

Den Index neu aufbauen (z. B. nach einem Plattenfehler): Server stoppen, `/volume1/xlrx-state/index`
löschen, Server starten. Gelesene Texte liegen in der Datenbank und werden nicht noch einmal gelesen.

## 11. Datenklassen

Jeder Ordner ist **„Nur lokal“** oder **„Cloud erlaubt“**; die Einstellung gilt für alles darin, bis ein
Ordner weiter unten etwas anderes sagt. Ohne Einstellung gilt `XLRX_DEFAULT_DATA_CLASS` aus `.env`
(Standard `local`). Das so lassen und einzelne Ordner in der Web-Oberfläche freigeben: im Ordner auf
„Nur lokal“ tippen oder in den Aktionen eines Ordners „Datenklasse“ wählen. Ändern darf nur, wer den Ordner
verwaltet, mit erneuter Bestätigung (Code oder Passkey); jede Änderung steht im Protokoll.

Solange weder der Außen-Beschleuniger (Abschnitt 14) noch die KI-Suche (ab M4) eingeschaltet ist, verlässt nichts das
NAS. Die **Verwaltung** zeigt unter „Datenklassen“ jeden Ordner mit eigener Einstellung – das Verzeichnis,
welche Daten wohin dürfen.

## 12. Öffentliche Links

Wer ein Element verwaltet, kann im Teilen-Dialog einen Link anlegen: zum **Ansehen**, **Herunterladen**,
**Nur hochladen** (Dateianfrage: andere legen Dateien in einen Ordner, ohne zu sehen, was darin ist) oder
**Bearbeiten**. Die Linkseite (`https://drive.example.de/s/…`) braucht kein Konto. Optional mit Passwort,
Ablaufdatum und einer Höchstzahl an Downloads; beenden geht jederzeit und wirkt sofort.

- Über einen Link wird nie etwas gelöscht, umbenannt oder überschrieben: Gibt es einen Namen schon, bekommt die
  neue Datei einen freien („Name (1).pdf“); eine neue Fassung über einen Bearbeiten-Link lässt die alte als Version.
- Ein Link kann nie mehr als die Person, die ihn angelegt hat. Verliert sie die Rechte oder wird ihr Konto gesperrt,
  funktioniert der Link nicht mehr.
- Wer Passwörter oder Links rät, wird gesperrt (je Link und je Adresse). Alles steht im Protokoll der Verwaltung.
- Größte Datei über einen Link: `XLRX_LINK_UPLOAD_MAX_MB` in `.env` (Standard 10 240 MB).
- Links laufen über den Heimanschluss – oder, wenn eingerichtet, große Dateien über den Außen-Beschleuniger
  (Abschnitt 14); „Nur lokal“-Inhalte gehen nie über S3.

## 13. Startseite und Aktivität

Die Startseite schlägt vor, was du wahrscheinlich gleich brauchst, und sagt warum („Öffnest du meist montags“,
„Anna hat das vor 2 Std. geändert“). Grundlage ist, was du im Web öffnest oder herunterlädst – das sieht nur
du selbst. Die Zeitzone dafür steht in `XLRX_TIMEZONE` (Standard `Europe/Berlin`).

Unter **Aktivität** steht, was mit Dateien geschah, die du sehen darfst: hochgeladen, geändert, umbenannt,
verschoben, gelöscht, geteilt. Änderungen über SMB oder Synology Drive erscheinen als „auf dem NAS“, sobald der
Abgleich sie findet; der allererste Import einer Ablage erscheint dort nicht.

## 14. Außen-Beschleuniger (S3)

Wer unterwegs große Dateien lädt – vor allem über Freigabe-Links –, bekommt sie aus einem S3-Bucket statt über den
Heimanschluss und die NAS-CPU. Gespiegelt wird nur aus Ordnern mit **„Cloud erlaubt“**; „Nur lokal“-Inhalte kommen nie in
den Bucket (auch keine Kopie davon). Im Heimnetz wird nie umgeleitet.

1. Bei Hetzner (Cloud Console → Object Storage) einen **privaten** Bucket anlegen, z. B. `xlrx-cache` in `fsn1`, und dafür
   **eigene** Zugangsdaten (nicht die von Hyper Backup).
2. Unvollständige Uploads automatisch entfernen lassen (einmalig, z. B. mit der AWS-CLI vom Mac):
   ```sh
   aws s3api put-bucket-lifecycle-configuration --endpoint-url https://fsn1.your-objectstorage.com \
     --bucket xlrx-cache --lifecycle-configuration \
     '{"Rules":[{"ID":"abort-mpu","Status":"Enabled","Filter":{"Prefix":""},"AbortIncompleteMultipartUpload":{"DaysAfterInitiation":1}}]}'
   ```
3. Zugangsdaten eintragen und in `.env` den Bucket setzen:
   ```sh
   umask 077
   printf '%s' 'ACCESS-KEY' > secrets/s3_access_key
   printf '%s' 'SECRET-KEY' > secrets/s3_secret_key
   ```
   ```
   XLRX_S3_BUCKET=xlrx-cache
   XLRX_S3_ENDPOINT=https://fsn1.your-objectstorage.com
   XLRX_S3_REGION=fsn1
   ```
4. `sudo docker compose up -d`. Die **Verwaltung** zeigt unter „Außen-Beschleuniger“, was im Bucket liegt und warum.
5. Prüfen: einen Link auf eine große Datei anlegen, ein paar Minuten warten (Verwaltung: „1 für Links“) und ihn auf dem
   Handy **ohne WLAN** öffnen. Der Download muss schnell sein und den richtigen Dateinamen tragen; im WLAN kommt dieselbe
   Datei vom NAS.

Was gespiegelt wird: Dateien hinter Freigabe-Links (ab 8 MB, sobald der Link angelegt ist), Dateien, die innerhalb
einer Woche mehrfach von außen geladen wurden, und für Personen mit „Unterwegs vorausladen“ (Einstellungen) ihre
markierten und vorgeschlagenen Dateien – nachts zwischen 1 und 6 Uhr. Hochgeladen wird mit höchstens
`XLRX_S3_UPLOAD_MBIT` (Standard 20 Mbit/s), damit der Anschluss frei bleibt.

Aufräumen geschieht von selbst: Endet ein Link, ist seine Datei sofort weg (auch bereits verschickte Download-Adressen
gehen dann nicht mehr). Eine neue Fassung ersetzt die alte. Sonst bleibt der Bucket unter `XLRX_S3_BUDGET_GB`
(Standard 200), und was 30 Tage niemand geladen hat, verschwindet. „Leeren“ in der Verwaltung entfernt alles.

Wird ein Ordner auf „Nur lokal“ gestellt, verschwinden seine Dateien sofort aus dem Bucket und werden nie mehr von
dort ausgeliefert. Im Bucket stehen nur zufällige Namen – keine Dateinamen, keine Pfade.

**Heimnetz erkennen:** Private Adressen (192.168.…, fd…) gelten immer als zu Hause. Nutzen Geräte zu Hause **IPv6**,
kommen sie mit öffentlichen Adressen an; dann den IPv6-Präfix des Anschlusses in `XLRX_LAN_NETS` eintragen (Fritz!Box:
Heimnetz → Netzwerk → Netzwerkeinstellungen → IPv6). Wechselt der Präfix, laden Geräte zu Hause bis zur Anpassung über
den Bucket – das kostet nur Bandbreite, ist aber kein Risiko.

Vor dem Einschalten: AVV mit Hetzner abschließen (Cloud Console → Datenschutz).

## 15. DSM-Reverse-Proxy ablösen

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
