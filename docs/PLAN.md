# xlrx-drive – Umsetzungsplan

> „Google Drive für die eigene Synology“: Web-UI mit Startseite, Aktivitätsstream und sehr guter
> Volltextsuche, Teilen zwischen Benutzern, extrem effizienter und zuverlässiger Sync, Mac- und iOS-App.
> Hosting: Docker (Container Manager) auf Synology, amd64.

Stand: 2026-10-02 · Status: Entwurf zur Abstimmung

---

## 0. Inhalt

1. [Getroffene Entscheidungen & Annahmen](#1-getroffene-entscheidungen--annahmen)
2. [Leitprinzipien](#2-leitprinzipien)
3. [Systemüberblick](#3-systemüberblick)
4. [Speicherung auf dem NAS](#4-speicherung-auf-dem-nas)
5. [Sync-Protokoll & Sync-Engine](#5-sync-protokoll--sync-engine)
6. [Suche](#6-suche)
7. [KI-Analyse über Scaleway / Cloudflare](#7-ki-analyse-über-scaleway--cloudflare)
8. [Startseite, Aktivität, Benachrichtigungen](#8-startseite-aktivität-benachrichtigungen)
9. [Teilen & Berechtigungen](#9-teilen--berechtigungen)
10. [Web-UI](#10-web-ui)
11. [Mac-App](#11-mac-app)
12. [iOS-App](#12-ios-app)
13. [Datenmodell (Postgres)](#13-datenmodell-postgres)
14. [API-Überblick](#14-api-überblick)
15. [Betrieb auf der Synology](#15-betrieb-auf-der-synology)
16. [Sicherheit](#16-sicherheit)
17. [Teststrategie](#17-teststrategie)
18. [Repo-Struktur](#18-repo-struktur)
19. [Roadmap & Meilensteine](#19-roadmap--meilensteine)
20. [Risiken](#20-risiken)
21. [Offene Fragen](#21-offene-fragen)

---

## 1. Getroffene Entscheidungen & Annahmen

### Entschieden

| Thema | Entscheidung |
|---|---|
| Speichermodell | **Normale Dateien** auf dem Volume. Parallel nutzbar per SMB, File Station, Hyper Backup, Synology Photos. |
| Mac-Client | **Beides, umschaltbar pro Sync-Ordner:** Spiegel-Ordner mit Selective Sync (Standard) **und** File-Provider-Modus (Dateien auf Abruf). |
| Suche | Text aus PDF/Office/Mails, **OCR**, **semantische Suche**, **Bildsuche nach Inhalt**. |
| ML-Rechenleistung | **Cloud-APIs mit Open-Source-Modellen**: Scaleway Generative APIs und/oder Cloudflare Workers AI. |
| Nutzer | Familie/Team (bis ~20 Konten), Zugriff von unterwegs, **öffentliche Freigabe-Links**. |
| Hardware | 16–20 GB RAM, amd64. |
| Datenmenge | 300k – 3 Mio Dateien, mehrere TB. |
| Tech-Stack | **Rust** (Server + gemeinsame Sync-Engine), **Swift/SwiftUI** (Mac/iOS, Engine via UniFFI), **SvelteKit/TypeScript** (Web). |

### Annahmen (bitte korrigieren, falls falsch)

- **Login:** eigene Konten mit Passwort (argon2id) + **Passkeys** + TOTP-2FA. OIDC (z.B. Authentik, Synology SSO Server) erst später, optional.
- **Plattformen v1:** Web, macOS, iOS. Windows/Linux/Android später. Der Rust-Kern macht das vorbereitet möglich.
- **Apple Developer Program** (99 €/Jahr) ist vorhanden oder wird angelegt. Ohne geht es nicht: File-Provider-Entitlements, App Groups, Push, Notarisierung, TestFlight.
- **Nicht in v1:** Office-Bearbeitung im Browser (später Collabora/OnlyOffice via WOPI), Gesichtserkennung, Ende-zu-Ende-Verschlüsselung, Kommentare an Dateien.
- **Mindestversionen:** macOS 15, iOS 18 (Vorschlag, weil die File-Provider-APIs dort ausgereift sind).

### Warum Eigenbau und nicht Seafile/Nextcloud/oCIS?
Seafile hat einen hervorragenden Sync, speichert aber in einem eigenen Block-Store. Das widerspricht „normale Dateien“.
Nextcloud ist schwergewichtig (PHP), sein Sync ist vergleichsweise langsam, Suche und Startseite sind nur Plugins.
oCIS speichert ebenfalls nicht als einfache Ordnerstruktur. Synology Drive fehlen Suche, Startseite und Aktivität.
Die Kombination „normale Dateien + Google-Drive-UX + eigene Suche“ gibt es fertig nicht.

---

## 2. Leitprinzipien

1. **Kein Datenverlust, niemals.** Jeder Pfad, der Daten überschreibt oder löscht, hat ein Netz: Server-Versionen, Papierkorb, Konfliktkopien, Massenlösch-Schutz, Btrfs-Snapshots.
2. **Der Server ist die Wahrheit, das Dateisystem ist der Speicher.** Metadaten, Journal und Rechte liegen in Postgres. Inhalte sind normale Dateien. Alles andere (Suchindex, Vektoren, Thumbnails) ist **abgeleitet und jederzeit neu aufbaubar**.
3. **Sync-Logik einmal schreiben, gnadenlos testen.** Die Engine ist ein **sans-IO-Kern in Rust**. Sie trifft Entscheidungen, macht aber selbst kein I/O. Dadurch läuft sie in deterministischen Simulationen mit Millionen zufälliger Szenarien.
4. **Effizienz durch Inhalt-Adressierung.** BLAKE3-Hashes und Content-Defined Chunking (FastCDC) ermöglichen Delta-Uploads und Delta-Downloads sowie Dedup. Umbenennen oder Verschieben überträgt nie Inhalte.
5. **Früh Nutzen stiften.** Server, Web-UI und Suche laufen zuerst neben dem bestehenden Synology Drive. Der eigene Sync-Client löst Synology Drive erst ab, wenn er bewiesen ist.
6. **Datenschutz bei KI:** Es gehen nur abgeleitete, minimierte Daten an KI-Anbieter (Text-Chunks, verkleinerte Bilder ohne EXIF). Ordner sind abwählbar. Ein Budget-Limit ist eingebaut.

---

## 3. Systemüberblick

```
                         Internet / LAN
                               │
             DSM Reverse Proxy (TLS, Let's Encrypt)
                               │
┌──────────────────────────────┼───────────────────────────────────────────┐
│ Container Manager – Projekt "xlrx" (docker compose, amd64)                │
│                                                                           │
│  xlrx-server  (Rust, axum, tokio)                                         │
│   ├─ REST/JSON + WebSocket-API, liefert Web-UI (SvelteKit, statisch) aus  │
│   ├─ Sync: Journal, Uploads/Downloads (Chunks), Versionen, Papierkorb     │
│   ├─ Watcher: inotify + periodische Abgleich-Scans (externe Änderungen)   │
│   ├─ Suche: Tantivy (lexikalisch) + pgvector (semantisch), hybrid         │
│   ├─ Startseite / Aktivität / Benachrichtigungen (WebSocket, APNs)        │
│   └─ Auth, Freigaben, Links, Admin                                        │
│          │                            │                                   │
│  postgres 17 + pgvector         xlrx-worker  (Rust, gleiche Codebasis)    │
│   Metadaten, Journal, Rechte,    ├─ Thumbnails/Vorschau (libvips, ffmpeg, │
│   Job-Queue, Texte, Vektoren     │   LibreOffice → PDF)                   │
│                                  ├─ Text-Extraktion (→ tika), EXIF, Geo   │
│  tika  (Apache Tika Server)      ├─ OCR lokal (Tesseract, Fallback)       │
│                                  └─ KI-Provider ──────────────────────────┼──► Scaleway (Paris)
│                                     (OpenAI-kompatibel)                   │    Cloudflare Workers AI
└───────────────────────────────────────────────────────────────────────────┘
  /volume1/drive/…   Btrfs, normale Dateien     /volume?/xlrx-state/…  DB, Index, Thumbs (am besten SSD)

Clients
  Mac-App  ── xlrx-core (Rust via UniFFI) ── Spiegel-Modus (FSEvents) │ File-Provider-Modus
                                            FinderSync-Badges, Menüleiste, LaunchAgent
  iOS-App  ── xlrx-core ── File-Provider-Extension (Dateien-App), Share-Extension, Push
  Browser  ── Web-UI (SvelteKit, PWA)
```

**Warum ein separater Worker?** Parser für fremde Dateiformate (PDF, Office, Bilder, Videos) sind die
häufigste Quelle für Abstürze und Sicherheitslücken. Der Worker läuft isoliert: Er hat nur **lesenden** Zugriff
auf die Daten und Netz nur zum KI-Anbieter. Wenn er abstürzt, läuft der Sync ungestört weiter.

### RAM-Budget (16 GB-Szenario)

| Komponente | RAM |
|---|---|
| DSM + andere Dienste | 2–3 GB |
| Postgres (shared_buffers 3 GB) | ~4 GB |
| xlrx-server | 0,5–1 GB (+ Tantivy über Page-Cache/mmap) |
| xlrx-worker | 1–2 GB (Spitzen bei LibreOffice/ffmpeg) |
| Tika | ~1 GB Heap |
| Rest | Page-Cache für Index & Dateien |

### Plattenbedarf für abgeleitete Daten (bei ~1,5 Mio Dateien, grob)
Postgres 10–30 GB (inkl. extrahierter Texte und Vektoren), Tantivy 5–15 GB, Thumbnails/Vorschauen 50–100 GB.
→ **Wenn möglich auf ein SSD-Volume legen.** Die Nutzdaten bleiben auf den HDDs.

---

## 4. Speicherung auf dem NAS

### 4.1 Layout

```
/volume1/drive/                       ← Synology-Freigabe "drive" (Btrfs), im Container: /data
  users/klaus/…                       ← "Meine Ablage" von klaus
  users/anna/…
  spaces/familie/…                    ← "Geteilte Ablagen" (wie Google Shared Drives)
  spaces/buero/…
  .xlrx/                              ← nur für den Container-Benutzer sichtbar
    versions/ab/cd/<blake3>           ← alte Versionen (Reflink-Kopien, inhaltsadressiert)
    trash/<node-id>/…                 ← Papierkorb (Verschieben = rename, 30 Tage)
    staging/<upload-id>               ← laufende Uploads
```

- **Roots sind konfigurierbar:** Jede „Meine Ablage“ und jede geteilte Ablage zeigt auf einen Host-Pfad.
  Damit lassen sich **bestehende Synology-Drive-Ordner direkt einbinden** (z.B. `/volume1/homes/klaus/Drive`).
  Migration bedeutet dann Einbinden statt Kopieren.
- `.xlrx/` liegt **im selben Mount** wie die Daten. Nur so funktionieren Btrfs-Reflinks; über Mount-Grenzen
  hinweg lehnt der Kernel Clone-Operationen ab. Der Ordner gehört exklusiv dem Container-Benutzer. Mit
  „Zugriffsbasierter Aufzählung“ ist er für SMB-Nutzer unsichtbar.

### 4.2 Versionen ohne Platzkosten (Btrfs-Reflinks)
Bevor eine Datei überschrieben wird, legt der Server einen **Reflink-Klon** der alten Version unter
`.xlrx/versions/` an (`FICLONE`/`BTRFS_IOC_CLONE`). Das kostet keine Zeit und keinen Platz, solange sich die Blöcke nicht unterscheiden.
Fallback auf ext4: normale Kopie. Aufbewahrung konfigurierbar, z.B. alle Versionen 30 Tage, danach täglich/wöchentlich, max. N.

### 4.3 Atomare Schreibvorgänge & Absturzsicherheit
Commit eines Uploads:
1. **Intent** in Postgres schreiben (`pending_ops`: was soll passieren, mit welcher Basis-Version).
2. Datei in `staging/` zusammensetzen, `fsync`, BLAKE3 der ganzen Datei prüfen.
3. Prüfen, ob die Zieldatei noch der erwarteten Version entspricht (inode/size/mtime/ctime + ggf. Hash).
4. Alte Version per Reflink nach `versions/`, dann `rename(staging → ziel)`, `fsync` des Verzeichnisses.
5. DB-Transaktion: Knoten, Version, Journal-Eintrag, Intent erledigt.

Nach einem Absturz arbeitet der Server beim Start alle offenen Intents **idempotent** ab (vorwärts oder zurück).
Die mtime des Clients wird übernommen (`utimensat`), damit SMB-Nutzer echte Änderungsdaten sehen.

### 4.4 Externe Änderungen erkennen (SMB, File Station, Synology Drive, Fotos-App …)
- **inotify** auf allen Verzeichnissen. Das Limit `fs.inotify.max_user_watches` muss per DSM-Aufgabenplaner beim Boot angehoben werden
  (1 Watch pro Verzeichnis, bei 3 Mio Dateien grob 100k–300k Verzeichnisse).
- **Entprellen:** Eine Datei gilt erst als fertig, wenn `IN_CLOSE_WRITE` kam bzw. Größe und mtime N Sekunden stabil sind.
  Das ist wichtig bei großen SMB-Kopien.
- **Abgleich-Scan** beim Start und nächtlich: stat-Walk über alles, Vergleich mit (inode, size, mtime, ctime) aus der DB.
  Dabei wird nur neu gehasht, was sich geändert hat. Der Scan fängt verlorene inotify-Events und Änderungen bei gestopptem Container auf.
  Optionale spätere Optimierung: `btrfs subvolume find-new` (erfordert erweiterte Container-Rechte).
- Eigene Schreibvorgänge merkt sich der Server (erwartete inode/mtime) und ignoriert deren Events.
- Externe Änderungen werden ins **Journal** übernommen wie jede andere Änderung. Die Zuordnung zum Benutzer
  geschieht nach bestem Wissen über die Datei-UID; sonst wird „extern“ angezeigt.
- **Ignoriert:** `@eaDir`, `#recycle`, `#snapshot`, `.SynologyWorkingDirectory`, `@tmp`, `.DS_Store`, `._*`,
  `Thumbs.db`, `~$*`-Lock-Dateien.
- Hinweis: Der DSM-Kernel ist je nach Modell 4.4 oder 5.10. Darum setzt der Plan kein dateisystemweites fanotify voraus.

### 4.5 Namen & Plattform-Eigenheiten
- Namen werden serverseitig in **NFC** gespeichert. macOS liefert oft NFD („Ä“ zerlegt).
- **Case-Insensitivity:** APFS ist meist case-insensitiv, Btrfs nicht. Der Server erzwingt Eindeutigkeit **case-insensitiv pro Ordner**
  für Änderungen über die API. Entstehen über SMB trotzdem „Foo.txt“ und „foo.txt“, synchronisiert der Mac-Client
  eine davon als `foo (Groß-/Kleinschreibungskonflikt).txt`.
- Unzulässige Zeichen und zu lange Pfade bekommen eine Warnung in der UI und werden nie still verworfen.

---

## 5. Sync-Protokoll & Sync-Engine

Das ist das Herzstück. Es ist bewusst nach dem Vorbild der Dropbox-„Nucleus“-Architektur gebaut.

### 5.1 Grundbegriffe
- **Node:** stabile ID (64 Bit), `parent_id`, `name`, Typ (Datei/Ordner), aktuelle Version. Pfade sind abgeleitet.
  → **Umbenennen und Verschieben kosten O(1) und übertragen keine Daten.**
- **Version:** BLAKE3-Hash des Inhalts, Größe, mtime, Chunk-Liste, Autor, Gerät.
- **Chunks:** FastCDC (min 256 KiB / avg 1 MiB / max 4 MiB), je Chunk BLAKE3.
  Wenn sich in einer 2-GB-Datei 3 MB ändern, werden nur ~3–6 Chunks übertragen.
- **Journal:** global monotone Sequenznummer (`seq`). Jede Änderung (Upload, Rename, Move, Delete, Restore, extern) erzeugt einen Eintrag
  mit Zustand nach der Änderung **sowie alten und neuen Vorfahren**. Dadurch kommt ein Verschieben aus einer Freigabe heraus beim Empfänger als „entfernt“ an.

### 5.2 Änderungen holen (Remote → Client)
- `GET /api/sync/changes?cursor=…&roots=…` liefert verdichtete Änderungen seit dem Cursor (pro Node nur der letzte Zustand) und einen neuen Cursor.
- **Push statt Polling:** WebSocket `/api/sync/notify` sendet `{seq}` bei neuen Änderungen. Der Client holt dann gezielt ab.
  Ziel im LAN: unter 2 s vom Speichern bis zum Download-Start auf dem anderen Gerät.
- Ist der Cursor zu alt (Journal gekürzt), meldet der Server `reset`. Der Client macht dann ein seitenweises Snapshot-Listing und gleicht per Hash ab.
  Das ist sicher, weil inhaltsbasiert verglichen wird, und es löst keine Neu-Downloads aus.
- Rechteentzug (Freigabe beendet) wird als Root-Entfernung gemeldet. Lokal unveränderte Dateien werden entfernt, lokal geänderte als Kopie behalten.

### 5.3 Hochladen (Client → Server)
1. Der Client chunked und hasht die Datei. Ein Hash-Cache über (Gerät, inode, size, mtime_ns, ctime) vermeidet unnötiges Neu-Hashen.
2. `POST /api/uploads` mit `{parent, name, base_version, size, hash, chunks[]}`.
   Der Server antwortet: „Inhalt existiert schon“ (Dedup, sofort fertig) oder „diese Chunks fehlen“.
   Der Server kennt vorhandene Chunks über einen **Chunk-Index** (Chunk-Hash → Datei + Offset).
3. `PUT /api/uploads/{id}/chunks/{hash}` läuft parallel und wiederaufnehmbar, zstd-komprimiert, wenn es sich lohnt. Requests sind ≤ 8 MB,
   damit kein Proxy-Body-Limit greift (DSM-nginx, Cloudflare Tunnel 100 MB).
4. `POST /api/uploads/{id}/commit`: Der Server setzt aus vorhandenen Chunks (Hash wird beim Lesen verifiziert) und neuen Chunks zusammen
   und schreibt atomar (siehe 4.3).
   - `base_version` passt nicht zur aktuellen Version → **Konflikt**. Der Server legt die eingehende Version als
     `Name (Konflikt – Klaus' MacBook 2026-10-02 14.03).ext` daneben an. Beide bleiben erhalten.
     Diese Auflösung findet serverseitig statt, damit sich alle Clients gleich verhalten.
- **Viele kleine Dateien:** Batch-Endpunkte (mehrere Dateien pro Request, gestreamt) vermeiden ein Request-Gewitter, z.B. bei git-Checkouts.
- Ändert sich eine Datei während des Uploads, schlägt der Hash-Check am Ende fehl. Der Upload wird dann später wiederholt.
  Das betrifft z.B. Lightroom-Kataloge und SQLite-Dateien. Für geöffnete Datenbanken gilt zusätzlich eine Ruhezeit vor dem Upload.

### 5.4 Herunterladen
- **Neue Datei:** einfacher HTTP-GET mit Range-Support, im LAN mit Leitungsgeschwindigkeit.
- **Geänderte große Datei:** Der Client vergleicht die Chunk-Liste der neuen Version mit der lokal bekannten alten Version und lädt nur fehlende Chunks.
- Download in eine Temp-Datei im selben Volume, Hash prüfen, **direkt vor dem Ersetzen prüfen, ob die lokale Datei
  unverändert ist**, dann atomar ersetzen (`renamex_np` mit `RENAME_SWAP`/`RENAME_EXCL` auf macOS).

### 5.5 Engine-Architektur (Client)
Drei Bäume in einer lokalen SQLite-DB:
- **Remote-Baum R:** letzter bekannter Serverzustand
- **Lokaler Baum L:** letzter beobachteter Zustand der Festplatte
- **Synced-Baum S:** gemeinsame Basis, also der Zustand, auf den sich beide zuletzt geeinigt hatten

Der **Planner** berechnet R−S (Remote-Änderungen) und L−S (lokale Änderungen), führt einen Drei-Wege-Merge durch und
erzeugt eine geordnete Menge von Operationen: Ordner vor Inhalt, Moves vor Deletes, Zyklen-Erkennung bei Moves über Kreuz.
Konfliktregeln:

| Fall | Ergebnis |
|---|---|
| Beide Seiten ändern Inhalt | beide behalten (Konfliktkopie) |
| Löschen vs. Bearbeiten | Bearbeiten gewinnt (Datei bleibt bzw. wird wiederhergestellt) |
| Move vs. Move | Server gewinnt, lokales Ergebnis wird angepasst |
| Neuer Name kollidiert | Suffix „(2)“ |
| Case-only-Rename auf APFS | Sonderbehandlung über Zwischenschritt |

**Sans-IO:** Der Kern bekommt Ereignisse (Remote-Änderungen, FS-Events, Ergebnisse von Operationen) und liefert Operationen zurück.
Netzwerk, Dateisystem und Uhr sind austauschbare Adapter. Dieselbe Engine läuft auf Mac, iOS und im Simulator.

**Invarianten** werden laufend geprüft:
- S ist ein Baum.
- Kein Name kommt pro Ordner doppelt vor (case-insensitiv).
- Keine Datei wird lokal gelöscht, deren aktueller Inhalt nicht auf dem Server bestätigt ist.

Im Test führt eine Verletzung zum Abbruch. In Produktion folgt eine Selbstheilung: S wird per Hash-Abgleich neu aufgebaut, ohne Datenverlust.

### 5.6 Schutzmechanismen
- **Massenlösch-Schutz:** Werden lokal mehr als X % oder mehr als N Dateien auf einmal gelöscht, pausiert der Sync und fragt nach.
- **Sync-Root verschwunden** (externe Platte ab, Ordner gelöscht oder verschoben): Der Sync pausiert. Das gilt **nie** als „alles löschen“.
- Remote-Löschungen werden lokal nur angewendet, wenn die lokale Datei unverändert ist. Der Server hat sie ohnehin im Papierkorb.
- Uhrzeiten von Clients sind für Entscheidungen irrelevant. Es zählen nur Sequenznummern und Hashes.

### 5.7 Selective Sync
- Die Auswahl wird per **Node-ID** gespeichert, nicht per Pfad. Wird ein ausgewählter Ordner auf einem anderen Gerät umbenannt, bleibt die Auswahl erhalten.
- Ein abgewählter Ordner wird lokal nur gelöscht, wenn alles darin synchronisiert ist. Sonst wird nachgefragt.
- Neue Unterordner erben die Auswahl. Bei Freigaben ist pro Freigabe einstellbar, ob sie standardmäßig gesynct werden.
- **Ignore-Regeln:** global (`.DS_Store`, `node_modules`, `*.tmp`, `.Trash`, …) plus `.xlrxignore` pro Ordner.

### 5.8 Effizienz-Details
- LAN-Direktverbindung: Der Client erkennt, wenn der Server lokal erreichbar ist. Das geht per Split-DNS oder per LAN-URL mit gepinntem Server-Zertifikat. Dann umgeht er den Reverse Proxy.
- Parallele Transfers mit adaptiver Parallelität, Bandbreitenlimits und Zeitplänen.
- FSEvents mit persistierter Event-ID (`sinceWhen`). Nach Neustart oder Schlaf werden nur die Änderungen seitdem verarbeitet, nicht alles gescannt.
- Hintergrund-QoS (`utility`), Pausieren im Akkubetrieb oder Stromsparmodus optional.
- Zielwerte: Leerlauf ~0 % CPU, Client-RAM < 200 MB bei 1 Mio Dateien, BLAKE3 > 1 GB/s auf Apple Silicon.

---

## 6. Suche

### 6.1 Was indexiert wird
| Quelle | Wie |
|---|---|
| Dateiname, Pfad | Tantivy, Edge-N-Gramme für Suche während der Eingabe |
| Text aus PDF/Office/Pages/Numbers/Keynote/EML/MSG/TXT/MD/Code | Apache Tika im Worker. PDFs ohne Textebene gehen zur OCR. |
| Gescannte PDFs & Bilder mit Text | OCR per Vision-Modell (Cloud) oder Tesseract `deu+eng` lokal (Fallback bzw. sensible Ordner) |
| Bildinhalt | Vision-Modell erzeugt Beschreibung, Tags, erkannten Text und Dokumenttyp (siehe 7.2) |
| EXIF / Medien-Metadaten | Aufnahmedatum, Kamera, **Ort** (Offline-Reverse-Geocoding mit GeoNames: „Kroatien 2024“ findet Urlaubsfotos) |
| Metadaten | Typ, Größe, Besitzer, Änderungsdatum, „bearbeitet von“, Freigabestatus, markiert |

Ergebnisse werden **pro Inhalt (BLAKE3)** gespeichert. Duplikate werden nur einmal analysiert. Umbenennen oder Verschieben löst
keine erneute Analyse aus, nur die Pfadfelder im Index werden aktualisiert.

### 6.2 Lexikalische Suche – Tantivy (eingebettet im Server)
- BM25, Phrasensuche, Fuzzy-Suche (Tippfehler), Präfixe, Snippets mit Hervorhebung.
- **Spracherkennung pro Dokument.** Deutsch und Englisch kommen in eigene Felder mit passendem Stemmer
  („Rechnungen“ findet „Rechnung“). Die Anfrage geht an beide Felder.
- Der extrahierte Text liegt zusätzlich komprimiert in Postgres. Der Index lässt sich deshalb **ohne Neu-Extraktion** neu aufbauen, z.B. bei Schema-Änderungen.

### 6.3 Semantische Suche – pgvector
- Embeddings pro Text-Chunk (~500 Tokens, Überlappung) und pro Bildbeschreibung.
- **Speichersparend:** HNSW-Index über **binär quantisierte** Vektoren (1024 Bit = 128 Byte/Vektor), danach
  **Re-Ranking** der Top-Kandidaten mit `halfvec`. So bleiben auch 3–4 Mio Vektoren im RAM-Budget.
- Jeder Vektor trägt die Modell-ID. Ein Modellwechsel führt zu einem Neu-Embedding im Hintergrund, während der alte Index weiter antwortet.

### 6.4 Hybride Rangfolge
1. Lexikalisch Top-100 ‖ Vektor Top-100 laufen parallel. Das Query-Embedding hat ein Timeout von ~400 ms; bei Fehler gibt es nur lexikalische Treffer.
2. **Reciprocal Rank Fusion**, danach Boosts: Treffer im Dateinamen, Aktualität, eigene Nutzungshäufigkeit, „bei mir freigegeben“.
3. Optional ein Re-Ranker (Cross-Encoder) über die Top-30.
4. **Rechte:** Filter im Index über die Vorfahren-IDs (siehe 9.3). Zusätzlich prüft Postgres die finalen Treffer noch einmal (Defense in Depth).

### 6.5 Such-UI & Syntax
- Eine Suchleiste mit Live-Vorschlägen (Dateinamen sofort, Volltext nach Enter).
- Filter-Chips und Syntax: `typ:pdf`, `von:anna`, `in:"Steuer 2026"`, `nach:2026-01-01`, `vor:…`, `ist:geteilt`, `ist:markiert`,
  `dokument:rechnung`, `ort:kroatien`, `"exakte phrase"`, `-ausschluss`.
- Facetten: Dateityp, Besitzer, Zeitraum, Ort, Dokumenttyp.
- Zielwerte: p95 < 300 ms lexikalisch, < 800 ms hybrid.

### 6.6 Verarbeitungs-Pipeline
Job-Queue in Postgres (`FOR UPDATE SKIP LOCKED`), Retries mit Backoff, Dead-Letter-Liste und Admin-Ansicht.

```
Änderung ─► hash/chunk ─► metadaten (MIME-Sniffing, EXIF, Geo)
                       ─► thumbnail/vorschau
                       ─► text (Tika) ──► [keine Textebene?] ─► ocr
                       ─► bildanalyse (Vision-Modell)
                                  └──────────► embeddings ─► index (Tantivy/pgvector)
```

- **Prioritäten:** Was gerade hochgeladen oder geöffnet wurde, kommt zuerst. Die Erst-Indexierung des Bestands läuft mit niedriger Priorität.
  Sie macht Pausen bei hoher NAS-Last und nutzt für teure KI-Jobs die Batch-API (siehe 7.3).
- Fortschritt im Admin-Bereich: „412.000 von 1,3 Mio analysiert, ETA 2 Tage, Kosten bisher 38 €“.

---

## 7. KI-Analyse über Scaleway / Cloudflare

### 7.1 Provider-Abstraktion
Beide Anbieter bieten **OpenAI-kompatible Endpunkte** (`/v1/embeddings`, `/v1/chat/completions` mit Bildern).
Der Worker bekommt dafür einen Provider-Typ mit konfigurierbarer Basis-URL, API-Key und Modellnamen pro Aufgabe.
Später kann ohne Code-Änderung auch ein lokaler Server angeschlossen werden (Ollama/llama.cpp auf einem Mac).

| Aufgabe | Scaleway (Paris, EU) | Cloudflare Workers AI |
|---|---|---|
| Text-Embeddings | `qwen3-embedding-8b`: mehrsprachig, Dimension wählbar 32–4096 (Matryoshka), **0,10 €/Mio Tokens** | `bge-m3` oder `qwen3-embedding-0.6b`: ~0,012 $/Mio Tokens |
| Bildbeschreibung + OCR | `gemma-4-26b-a4b-it` (0,25 € / 0,50 € pro Mio Tokens ein/aus) oder `mistral-medium-3.5` für schwierige Scans | `llama-3.2-11b-vision-instruct` (0,049 $ / 0,676 $), `mistral-small-3.1-24b` |
| Audio (später) | `whisper-large-v3` (0,003 €/min) | – |
| Rabatt | Batches-API: −50 % | 10.000 Neuronen/Tag gratis |

Die Preise stammen von den Anbieterseiten (Stand Oktober 2026) und ändern sich häufig. Vor dem Start gibt es einen Probelauf mit echten Kosten.

**Empfehlung:** Scaleway als Standard. Die Daten bleiben in der EU (Paris), mit französischem Anbieter und AVV nach DSGVO.
`qwen3-embedding-8b` ist deutlich stärker als die günstigen Cloudflare-Embeddings, und mit 1024 Dimensionen passt es zur
binären Quantisierung. Cloudflare ist als günstige Alternative vorgesehen, z.B. für Bildbeschreibungen.
**Wichtig:** Für Indexierung **und** Suchanfragen muss dasselbe Embedding-Modell verwendet werden. Es wird also ein Modell pro Installation festgelegt.

### 7.2 Ein Vision-Aufruf pro Bild, strukturiertes Ergebnis
Statt separater Modelle für Bildsuche (CLIP), OCR und Klassifizierung macht **ein** Vision-Aufruf alles. Er liefert JSON:
```json
{ "beschreibung": "Handwerkerrechnung der Firma Müller über Heizungswartung",
  "tags": ["rechnung", "heizung", "dokument"],
  "text_im_bild": "…erkannter Text…",
  "dokumenttyp": "rechnung",
  "datum_erkannt": "2025-11-14" }
```
Die Beschreibung und die Tags werden **lexikalisch indexiert und als Text embedded**. Dadurch funktionieren deutsche Anfragen wie
„Hund am Strand“ oder „Rechnung Heizung“ im selben Vektorraum wie Dokumente. Der Dokumenttyp wird zur Facette.

### 7.3 Kosten (grobe Schätzung, ~1,5 Mio Dateien)
Annahmen: 250k Textdokumente (Ø 3k Tokens), 20k gescannte Seiten, 800k Fotos.

| Posten | Scaleway | mit Batch-API | Cloudflare |
|---|---|---|---|
| Text-Embeddings (~750 Mio Tokens) | ~75 € | ~38 € | ~9 $ |
| OCR 20k Seiten | ~15 € | ~8 € | ~10 $ |
| 800k Fotos beschreiben (+ Embedding) | ~260 € | ~130 € | ~110 $ |
| **Einmalig gesamt** | **~350 €** | **~175 €** | **~130 $** |
| Laufend | wenige €/Monat | | |

Die Bild-Token-Anzahl hängt vom Modell ab. Ein Probelauf über 500 Dateien liefert die echten Kosten pro Datei.
Kosten senken lässt sich so:
- winzige Bilder, Icons und Duplikate überspringen
- Screenshots optional ausnehmen
- nur die ersten ~16k Tokens langer Dokumente embedden; der Rest bleibt lexikalisch durchsuchbar

**Budget-Limit** pro Monat im Admin-Bereich: Ist es erreicht, pausiert die KI-Pipeline. Die lexikalische Suche läuft weiter.

### 7.4 Datenschutz
- **Pro Ordner/Ablage „KI-Analyse erlaubt“** (vererbt). Für ausgenommene Ordner (z.B. Gesundheit, Verträge) gibt es nur lokale
  Extraktion, lokale OCR (Tesseract) und lexikalische Suche, keine Vektoren.
- Es werden nur minimierte Daten gesendet: Text-Chunks oder Bilder verkleinert auf ≤ 1024 px, **ohne EXIF/GPS**.
  Dateinamen und Pfade werden standardmäßig nicht mitgeschickt.
- Bei geteilten Ablagen entscheidet der Besitzer bzw. Verwalter über die KI-Erlaubnis.
- Vor dem Go-live: AVV mit dem Anbieter abschließen und die Bedingungen zu Speicherung und Training prüfen. Laut Anbieterangaben wird nicht gespeichert und nicht trainiert.

---

## 8. Startseite, Aktivität, Benachrichtigungen

### 8.1 Ereignisse
- **`events`**: Hochladen, Bearbeiten, Umbenennen, Verschieben, Löschen, Wiederherstellen, Teilen, Link erstellt, Link aufgerufen.
  Sichtbar für alle, die das Element sehen dürfen.
- **`access_events`** (privat, nur für die eigene Person): Öffnen, Vorschau, Download. Quellen sind die Web-UI, iOS und der Mac.
  Auf dem Mac liest der Client **`kMDItemLastUsedDate`** per `NSMetadataQuery` im Sync-Ordner. So erfährt die Startseite,
  welche Dateien du lokal in Pages, Excel und anderen Apps geöffnet hast, ohne dass diese Apps etwas wissen müssen.

### 8.2 Startseite
- **Vorgeschlagen** (Karten mit Vorschaubild und **Begründung**). Der Score kombiniert:
  - eigene **Frecency** (Häufigkeit × Aktualität, Halbwertszeit ~7 Tage)
  - **Wochenmuster** („öffnest du meist montags morgens“)
  - **Ko-Nutzung** („wird oft zusammen mit X geöffnet“)
  - **Aktivität anderer** an Dateien, die du kennst („Anna hat das vor 2 Std. bearbeitet“)
  - **neu für dich freigegeben**

  Start mit festen Gewichten. Angezeigte und angeklickte Vorschläge werden protokolliert, um die Gewichte später zu tunen.
- **Vorgeschlagene Ordner**, **Zuletzt verwendet**, **Markiert**, „Weiterarbeiten“ (zuletzt bearbeitet, auf allen Geräten).

### 8.3 Aktivitätsstream
- Gruppiert nach (Person, Aktion, Ordner, Zeitfenster ~30 min): „Anna hat 23 Fotos zu *Urlaub/Kroatien* hinzugefügt“,
  mit Thumbnail-Leiste.
- Filter: alle / von anderen / nur Freigaben. Pro Datei gibt es einen Aktivitätsverlauf im Detailbereich.
- Rechte: Jede Person sieht nur Ereignisse zu Elementen, auf die sie Zugriff hat. Die Prüfung erfolgt beim Lesen über dieselbe Vorfahren-Logik.

### 8.4 Benachrichtigungen
Glocke im Web (live per WebSocket), Push auf iOS und Mac (APNs, direkt vom Server mit `.p8`-Schlüssel), optional E-Mail (SMTP)
bei neuen Freigaben. Pro Person einstellbar.

---

## 9. Teilen & Berechtigungen

### 9.1 Modell
- **Meine Ablage** pro Person und **Geteilte Ablagen** (Team-Ordner mit Mitgliedern und Rollen, gehören keiner Einzelperson).
- **Freigaben** auf Ordner oder Datei an Person oder Gruppe mit Rolle **Betrachter**, **Bearbeiter** oder **Verwalter** (darf weiter teilen).
  Optional mit Ablaufdatum. Rechte vererben sich nach unten.
- **„Für mich freigegeben“** listet alles, was andere mit dir teilen. Ein Element kann per Verknüpfung in „Meine Ablage“ gelegt werden
  und wird dann auch vom Mac-Client synchronisiert.
- **Gruppen** (z.B. „Familie“, „Eltern“).
- **Speicherkontingente** pro Person (optional).

### 9.2 Öffentliche Links
- Typen: Ansehen, Herunterladen, **Nur-Upload** (Dateianfrage), Bearbeiten.
- Passwort (argon2id), Ablaufdatum, maximale Downloads, jederzeit widerrufbar. Zugriffe erscheinen im Aktivitätsstream.
- Token mit 128 Bit Zufall. Die Linkseite hat Rate-Limiting und Brute-Force-Sperre. Inhalte kommen von einer separaten Origin bzw. mit
  `Content-Disposition: attachment` und CSP `sandbox` (Schutz vor XSS über hochgeladenes HTML/SVG).

### 9.3 Rechteprüfung, effizient
- Jeder Node speichert `ancestor_ids bigint[]` (materialisierter Pfad).
  Zugriff = eine Freigabe/Mitgliedschaft auf dem Node oder einem Vorfahren für die Person oder eine ihrer Gruppen.
- **Suche/Aktivität:** Pro Anfrage werden die **sichtbaren Wurzeln** der Person berechnet: eigene Ablage, Ablagen mit Mitgliedschaft,
  einzeln freigegebene Nodes. Der Index filtert dann über `ancestor_ids ∩ Wurzeln ≠ ∅`.
  → **Teilen und Freigabe-Entzug erfordern keine Neu-Indexierung.** Nur Verschiebungen aktualisieren die Vorfahren
  des Teilbaums. Bis das im Hintergrund nachgezogen ist, schützt die Postgres-Nachprüfung.

---

## 10. Web-UI

**SvelteKit** (statischer Build, vom Server ausgeliefert), TypeScript. Der API-Client wird aus OpenAPI generiert (utoipa).
Live-Updates kommen per WebSocket. PWA-fähig, Deutsch/Englisch, Dark Mode.

Bereiche wie bei Google Drive: **Startseite**, **Meine Ablage**, **Geteilte Ablagen**, **Für mich freigegeben**, **Zuletzt verwendet**,
**Markiert**, **Papierkorb**, **Aktivität**, **Admin**.

Funktionen:
- **Virtualisierte Listen/Raster:** flüssig auch mit 100k Einträgen in einem Ordner. Mehrfachauswahl, Drag & Drop, Tastaturkürzel.
- **Upload von Dateien und ganzen Ordnern** per Drag & Drop, chunked und wiederaufnehmbar. BLAKE3 läuft als WASM im Web Worker,
  damit Dedup und Delta auch im Browser funktionieren.
- **Vorschau:**
  - Bilder inkl. HEIC/RAW (serverseitig konvertiert) und PDF (pdf.js)
  - Office/Pages (LibreOffice → PDF im Worker)
  - Video/Audio (Range-Streaming)
  - Text/Markdown/Code (Highlighting)
- Detailbereich: Infos, **Versionen** (ansehen/wiederherstellen), Aktivität, Freigaben.
- Teilen-Dialog wie bei Google Drive (Personen/Gruppen + Rolle, Link-Einstellungen).
- Admin: Benutzer/Gruppen, Ablagen und Roots, KI-Provider und Budget, Index- und Job-Status, Geräte, Systemzustand.

---

## 11. Mac-App

### 11.1 Bausteine
| Baustein | Aufgabe |
|---|---|
| **App (SwiftUI)** | Menüleiste (`MenuBarExtra`): Status, letzte Aktivität, Konflikte, Pause. Einstellungsfenster: Konto, Sync-Ordner, **Selective-Sync-Baum**, Modus je Ordner, Bandbreite, Ignore-Regeln. Onboarding. |
| **Sync-Agent** | LaunchAgent (`SMAppService`) mit xlrx-core. Läuft auch ohne geöffnetes UI und kommuniziert per XPC mit der App. |
| **FinderSync-Extension** | Nur im Spiegel-Modus: Status-Badges (synchron/läuft/Fehler) und Kontextmenü „Link kopieren“, „Teilen…“, „Im Browser öffnen“, „Versionen…“. |
| **File-Provider-Extension** | Nur im FP-Modus: `NSFileProviderReplicatedExtension` mit xlrx-core. Badges per Decorations, Aktionen per Custom Actions. |
| **xlrx-core** | Rust-Bibliothek als XCFramework (UniFFI): API-Client, Transfer, Chunking, Sync-Engine, lokale SQLite. |

### 11.2 Zwei Modi, pro Sync-Ordner wählbar
- **Spiegel-Modus (Standard):** echter Ordner, z.B. `~/xlrx/`, mit Selective Sync. Funktioniert mit jedem Tool (git, Lightroom, Skripte).
  Lokale Änderungen kommen per FSEvents in den lokalen Baum L.
- **File-Provider-Modus:** Ablage unter `~/Library/CloudStorage/xlrx-…`. Alles ist sichtbar, Inhalte werden bei Bedarf geladen.
  „Offline verfügbar“ lässt sich pro Ordner setzen, ungenutzte Dateien werden automatisch ausgelagert.
  Das passt gut zum Journal: Apples `enumerateChanges(fromSyncAnchor:)` entspricht 1:1 unserem Cursor.
  Wiederverwendet werden Remote-Baum, Transfer und Chunk-Diff. Den lokalen Baum verwaltet hier das System.
- Kombination möglich, z.B. „Meine Ablage“ gespiegelt und die riesige Ablage „Familienfotos“ per File Provider.
  Beim Moduswechsel eines Ordners wird bereits lokal vorhandener Inhalt per Hash wiederverwendet statt neu geladen (ab Phase 7).
- **Sinnvolle Ausschlüsse ab Werk:** `.photoslibrary`, `.app`-Bundles (optional), Caches. Andere Pakete wie `.rtfd` werden als Einheit behandelt.

### 11.3 Später
Globale Such-Palette per Hotkey: Volltextsuche des Servers direkt vom Mac, auch für nicht lokal vorhandene Dateien.
Finder-Tags synchronisieren.

---

## 12. iOS-App

- **SwiftUI-App** mit gemeinsamen UI-Komponenten (Swift Package `XlrxUI`): Startseite (Vorschläge, Aktivität), Suche mit Filtern,
  Durchsuchen, Geteilt, Markiert, Papierkorb. Vorschau per QuickLook.
- **File-Provider-Extension:** volle Integration in die **Dateien-App**, Bearbeiten an Ort und Stelle aus anderen Apps,
  „Offline verfügbar“. Die Extension hat eine knappe Speichergrenze. Darum streamt xlrx-core strikt und puffert nichts im RAM.
- **Share-Extension:** „In xlrx sichern“ aus jeder App.
- **Uploads im Hintergrund** über `URLSession`-Background-Transfers.
- **Push** bei Freigaben und Aktivität (APNs).
- Optional später: Foto-Backup aus der Mediathek.

---

## 13. Datenmodell (Postgres)

Skizze der wichtigsten Tabellen:

```
users(id, username, display_name, email, password_hash, is_admin, quota_bytes, …)
groups(id, name) · group_members(group_id, user_id)
passkeys(id, user_id, credential, …) · devices(id, user_id, name, platform, refresh_token_hash, last_seen)
roots(id, kind[user|space], host_path, owner_user_id | space_id, ai_allowed)
spaces(id, name) · space_members(space_id, principal_type, principal_id, role)

nodes(id, root_id, parent_id, name, name_folded, kind, size, content_hash, current_version_id,
      mtime, ancestor_ids bigint[], fs_inode, fs_ctime, ai_allowed_override,
      created_by, updated_by, updated_at, deleted_at)
      UNIQUE (parent_id, name_folded) WHERE deleted_at IS NULL
versions(id, node_id, content_hash, size, mtime, created_at, created_by, device_id, storage_ref)
contents(hash PK, size, mime, chunk_list bytea, analysis_state …)
chunk_locations(chunk_hash, content_hash, offset, len)
journal(seq bigserial PK, node_id, op, old_ancestors, new_ancestors, state jsonb, actor, device_id, at)
pending_ops(id, kind, payload jsonb, created_at)            -- Intent-Log (4.3)

shares(id, node_id, principal_type, principal_id, role, expires_at, created_by, created_at)
links(id, token_hash, node_id, kind, password_hash, expires_at, max_downloads, downloads, created_by)
stars(user_id, node_id)

events(id, at, actor, kind, node_id, ancestor_ids, details jsonb)
access_events(user_id, node_id, at, source)                 -- privat, partitioniert nach Monat
user_item_stats(user_id, node_id, frecency, last_access, weekday_hour_hist, …)
notifications(id, user_id, event_id, read_at)

content_text(hash, lang, source[extract|ocr|vision], text)  -- komprimiert (TOAST/lz4)
content_vision(hash, model, caption, tags, doc_type, detected_date, raw jsonb)
content_embeddings(hash, chunk_no, model, vec halfvec(1024))
search_vectors(node_id, chunk_no, model, ancestor_ids, vec halfvec(1024))
      -- HNSW auf binary_quantize(vec), Filter auf ancestor_ids
jobs(id, kind, key, priority, state, attempts, run_after, last_error, …)
ai_usage(day, provider, model, tokens_in, tokens_out, cost)
```

Migrationen mit `sqlx migrate`. Vor jeder Migration erstellt der Server automatisch einen `pg_dump`.

---

## 14. API-Überblick

REST/JSON unter `/api`, OpenAPI-Spezifikation generiert. Auth per Session-Cookie (Web) oder Bearer-Token (Geräte).
Geräte melden sich per Browser-Login mit PKCE an. Pro Gerät gibt es einen widerrufbaren Refresh-Token.

```
Auth      POST /auth/login · /auth/webauthn/* · /auth/totp · /auth/device/* · GET /me/devices
Nodes     GET /nodes/{id} · /nodes/{id}/children · POST /nodes (Ordner) · PATCH /nodes/{id} (rename/move)
          DELETE /nodes/{id} (→ Papierkorb) · POST /nodes/{id}/restore · GET /nodes/{id}/versions
Inhalt    GET /content/{version} (Range) · GET /thumb/{hash}/{size} · GET /preview/{version}
Upload    POST /uploads · PUT /uploads/{id}/chunks/{hash} · POST /uploads/{id}/commit · POST /uploads/batch
Sync      GET /sync/changes?cursor&roots · GET /sync/snapshot?page · WS /sync/notify
Suche     GET /search?q&filter… · GET /search/suggest?q
Start     GET /home · GET /activity · GET /notifications
Teilen    /shares · /links · öffentlich: /s/{token}
Admin     /admin/users · /admin/groups · /admin/roots · /admin/ai · /admin/jobs · /admin/health
Metriken  /metrics (Prometheus) · /healthz
```

---

## 15. Betrieb auf der Synology

- **Container Manager → Projekt** mit `docker-compose.yml` (liegt unter `deploy/`). Images für amd64 werden per GitHub Actions gebaut und
  in GHCR veröffentlicht.
- Ein eigener DSM-Benutzer `xlrx` (PUID/PGID) mit Lese-/Schreibrechten auf die Drive-Freigabe. Der Worker mountet die Daten **read-only**.
- **Boot-Aufgabe** (Aufgabenplaner, root): `sysctl -w fs.inotify.max_user_watches=1048576`.
- **Erreichbarkeit von außen:** DSM-Reverse-Proxy mit Let's-Encrypt-Zertifikat und eigener Domain (WebSocket-Header aktivieren).
  Alternativ Cloudflare Tunnel (Body-Limit 100 MB, deshalb bleiben Chunks klein) oder Tailscale.
  QuickConnect funktioniert für eigene Container **nicht**.
- **Backup:**
  - Hyper Backup über die Drive-Freigabe (normale Dateien + `.xlrx/versions`)
  - nächtlicher `pg_dump` in einen Backup-Ordner, der mitgesichert wird
  - Btrfs-Snapshots (Snapshot Replication) als zusätzliches Netz
  - Suchindex, Vektoren und Thumbnails werden **nicht** gesichert; sie werden neu aufgebaut. KI-Ergebnisse liegen in Postgres und gehen nicht verloren.
- **Monitoring:** `/healthz`, Prometheus-Metriken, strukturierte Logs, Admin-Dashboard (Job-Queue, Index-Status, Fehler).
- **Updates:** neues Image ziehen, automatische Migration mit vorherigem Dump, Rollback-Anleitung.

---

## 16. Sicherheit

- TLS überall, HSTS, strikte CSP, `SameSite`-Cookies, CSRF-Schutz.
- Passwörter mit argon2id, Passkeys, TOTP. Login-Rate-Limiting und Sperren. Gerätetokens widerrufbar.
- Postgres und Tika nur im internen Docker-Netz. Der Worker darf nur zum KI-Anbieter.
- Parser-Isolation im Worker (read-only, Ressourcenlimits, Timeouts pro Datei).
- Nutzerinhalte von einer separaten Origin bzw. als Attachment mit CSP `sandbox` ausliefern.
- Audit-Log für Admin-Aktionen, Freigaben und Link-Zugriffe.
- `cargo audit`/`cargo deny` und `npm audit` in der CI. Security-Review vor dem Öffnen nach außen.

---

## 17. Teststrategie

Zuverlässigkeit entsteht durch Tests, nicht durch Hoffnung. Darum hat das Testen eine eigene Säule im Plan.

1. **Deterministische Simulation der Sync-Engine** (`xlrx-sim`):
   - simuliertes Dateisystem, simulierter Server und Netz
   - zufällige Operationen auf mehreren Clients gleichzeitig (Edit, Rename, Move über Kreuz, Delete, Case-Rename, NFD/NFC)
   - Fehlerinjektion: Absturz an jeder Stelle, Netzabbrüche, Teil-Schreibvorgänge, Zeitsprünge, verlorene FSEvents
   - Prüfungen: **Konvergenz** (alle Clients landen beim gleichen Zustand) und **kein Datenverlust** (jeder je bestätigte Inhalt ist
     als Datei, Version oder Konfliktkopie auffindbar)
   - jeder Fehler ist über den Seed reproduzierbar. In der CI laufen Tausende Seeds pro Push und Millionen nächtlich.
2. **Property-Tests** (proptest) für Chunker, Planner und Merge-Regeln.
3. **Server-Integrationstests:** echtes Postgres (testcontainers), **Btrfs-Loopback** für Reflink-Tests,
   Crash-Tests (`kill -9` mitten im Commit, danach Recovery prüfen).
4. **End-to-End mit echtem Mac-Client** auf macOS-Runnern. Folterszenarien:
   - git-Checkouts großer Repos und rsync-Stürme
   - „Atomic Save“-Muster von Word/Pages/vim
   - 100k kleine Dateien, eine 50-GB-Datei
   - SMB-Änderungen während des Syncs, Ruhezustand mitten im Upload
5. **Web:** Playwright-Tests für die Kernabläufe.
6. **Last/Skalierung:** synthetischer Baum mit 3 Mio Dateien. Gemessen werden Listing, Sync, Suche und Erst-Indexierung gegen die Zielwerte.
7. **Dogfooding:** mehrere Wochen produktiver Eigenbetrieb **bevor** Synology Drive abgelöst wird.

---

## 18. Repo-Struktur

```
xlrx-drive/
├─ Cargo.toml                    (Workspace)
├─ crates/
│  ├─ xlrx-proto/                API-Typen & Protokoll (Server + Clients teilen sie)
│  ├─ xlrx-chunk/                FastCDC + BLAKE3, Hash-Cache
│  ├─ xlrx-sync/                 sans-IO Sync-Engine (Bäume, Planner, Merge)
│  ├─ xlrx-client/               HTTP-Client, Transfers, lokale SQLite, FS-Adapter (macOS/iOS)
│  ├─ xlrx-ffi/                  UniFFI-Bindings → XCFramework
│  ├─ xlrx-server/               axum-API, Journal, Storage, Watcher, Suche, Auth, Freigaben
│  ├─ xlrx-worker/               Extraktion, Thumbnails, OCR, KI-Provider
│  └─ xlrx-sim/                  deterministische Simulation & Fuzzing
├─ web/                          SvelteKit-App
├─ apple/
│  ├─ Project.yml                (XcodeGen/Tuist)
│  ├─ Packages/XlrxUI/           gemeinsame SwiftUI-Komponenten
│  ├─ macOS/                     App, SyncAgent, FinderSync, FileProvider
│  └─ iOS/                       App, FileProvider, ShareExtension
├─ deploy/
│  ├─ docker-compose.yml
│  ├─ Dockerfile.server · Dockerfile.worker
│  └─ synology.md                Einrichtungsanleitung
└─ docs/
   ├─ PLAN.md                    (dieses Dokument)
   └─ adr/                       Architektur-Entscheidungen
```

---

## 19. Roadmap & Meilensteine

Zwei parallele Stränge: **A – Server/Web/Suche** liefert früh Nutzen, während Synology Drive weiter synchronisiert.
**B – Sync-Kern** beginnt sofort, weil er das größte Risiko trägt, und reift in der Simulation, bevor ein UI existiert.

| # | Meilenstein | Inhalt | Fertig, wenn … | Größe |
|---|---|---|---|---|
| **M0** | Fundament | Workspace, CI (Rust/Web/Apple), Docker-Images, compose, Postgres-Schema, Auth (Passwort + Passkey), Admin-Grundgerüst | `docker compose up` auf der Synology zeigt den Login | M |
| **M1** | Server-Kern + Web-Basis | Roots (bestehende Drive-Ordner einbinden), Watcher + Abgleich-Scan, Journal, Upload/Download (Chunks), Versionen (Reflink), Papierkorb, Web: Durchsuchen, Upload, Vorschau, Thumbnails | Web-UI zeigt die echten Daten aus Synology Drive, Änderungen per SMB erscheinen in Sekunden | L |
| **B1** | Sync-Kern in Simulation | xlrx-chunk, xlrx-sync (sans-IO), xlrx-sim, Konfliktregeln, Invarianten | 1 Mio Seeds ohne Verletzung | L |
| **M2** | Volltextsuche I | Tika-Extraktion, Tesseract-OCR, Tantivy (de/en), Filter/Syntax, Snippets, Rechte-Filter, Such-UI | Suche nach Inhalt in PDF/Office/Scans, p95 < 300 ms bei Bestandsgröße | M |
| **M3** | Teilen, Aktivität, Startseite | Ablagen, Gruppen, Freigaben, Links, Aktivitätsstream, Vorschläge, Benachrichtigungen | Familie nutzt Web-UI für Teilen und findet Dinge über die Startseite | L |
| **M4** | KI-Suche | Provider-Abstraktion (Scaleway/Cloudflare), Vision-Analyse, Embeddings, pgvector, hybride Rangfolge, Budget, KI-Ordnerregeln, Kostenschätzung | „Rechnung Heizung 2025“ und „Hund am Strand“ liefern sinnvolle Treffer, Kosten im Rahmen | M |
| **M5** | Mac-App (Spiegel-Modus) | xlrx-client + FFI, SyncAgent, Menüleisten-App, Selective Sync, FinderSync, LAN-Direktverbindung | 4 Wochen Dogfooding ohne Datenverlust → **Synology-Drive-Client abschalten** | XL |
| **M6** | iOS-App | Startseite, Suche, Durchsuchen, Vorschau, File Provider (Dateien-App), Share-Extension, Push | Alltagstauglich auf iPhone/iPad, über TestFlight verteilt | L |
| **M7** | Mac File-Provider-Modus | FP-Extension, Offline-Pinning, Moduswechsel pro Ordner | Große Ablagen auf Abruf, stabil im Dogfooding | L |
| **M8** | Härtung & Feinschliff | Lasttest mit 3 Mio Dateien, Security-Review, Backup/Restore-Probe, Dokumentation, Tuning der Vorschläge | Go-live für alle, Synology Drive deinstalliert | M |

Größen: S ≈ Tage, M ≈ 1–3 Wochen, L ≈ 3–6 Wochen, XL ≈ 6+ Wochen fokussierte Arbeit. Das sind grobe Richtwerte.
Am meisten Zeit kostet erfahrungsgemäß die Härtung des Syncs (M5).

**Sinnvoller nächster Schritt:** M0 + Beginn von B1 (Chunker + Engine-Skelett + Simulator).

---

## 20. Risiken

| Risiko | Gegenmaßnahme |
|---|---|
| Datenverlust durch Sync-Fehler | sans-IO + Simulation, Invarianten, Massenlösch-Schutz, Server-Versionen und Papierkorb, Btrfs-Snapshots, Synology Drive bis M5 als Rückfallebene |
| Eigenheiten der File-Provider-API | Spiegel-Modus ist der Standard, FP kommt erst in M7. Beide nutzen denselben Kern. |
| Datenschutz bei Cloud-KI | Scaleway (EU), KI-Erlaubnis pro Ordner, Datenminimierung, AVV, lokale Fallbacks |
| KI-Kosten laufen davon | Kostenschätzung vor Start, Batch-API, Dedup per Hash, Budget-Limit mit Auto-Pause |
| NAS-Ressourcen (CPU/RAM) | schwere Arbeit in der Cloud, Worker mit Lastdrosselung, RAM-sparende Vektor-Quantisierung, SSD für Index/DB |
| inotify-Limits / alter Kernel | Limit anheben, Abgleich-Scans als Sicherheitsnetz |
| Externe Änderungen kollidieren mit Uploads | Intent-Log, Prüfung vor dem Ersetzen, Konfliktkopie statt Überschreiben |
| Projektumfang | strikte Meilensteine, Nicht-Ziele für v1 (siehe 1), früher Nutzen durch Strang A |

---

## 21. Offene Fragen

1. **Welches Modell genau** (DS923+, DS920+, …)? Gibt es ein **SSD-Volume** oder M.2-Slots für DB und Index?
2. **Wo liegen die Daten heute?** Synology-Drive-Ordner in `homes/<user>/Drive`, Team-Ordner, andere Freigaben? Die Antwort bestimmt das Root-Mapping.
3. **Zugriff von außen:** eigene Domain + Portweiterleitung, Cloudflare Tunnel oder nur Tailscale?
4. **Login:** Reichen eigene Konten (Passwort + Passkey), oder sollen DSM-Konten (LDAP/SSO) genutzt werden?
5. **Apple Developer Account** vorhanden? Auf wen läuft er (Person/Firma)?
6. **KI-Anbieter:** Scaleway als Standard okay? Gibt es Ordner, die **nie** in die Cloud dürfen?
7. Sind **Windows** oder **Android** absehbar nötig? Das beeinflusst Prioritäten, nicht die Architektur.
8. Wird **Bearbeiten von Office-Dokumenten im Browser** gewünscht (Collabora/OnlyOffice), oder reicht Vorschau + Bearbeiten lokal?
