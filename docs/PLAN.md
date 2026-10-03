# xlrx-drive – Umsetzungsplan

> „Google Drive für die eigene Synology“: Web-UI mit Startseite, Aktivitätsstream und sehr guter
> Volltextsuche, Teilen zwischen Benutzern, extrem effizienter und zuverlässiger Sync, Mac- und iOS-App.
> Hosting: Docker (Container Manager) auf Synology, amd64.

Stand: 2026-10-02 · Status: v5 – alle Grundsatzfragen geklärt, bereit für M0

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
13. [Datenbanken & Datenmodell](#13-datenbanken--datenmodell)
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
| ML-Rechenleistung | **Cloud-APIs mit Open-Source-Modellen**: **Scaleway Generative APIs** (bestätigt), Cloudflare Workers AI als Alternative. **Sensible Ordner bleiben lokal** und werden mit einem kleinen Embedding-Modell auf dem NAS durchsuchbar (siehe 7.5). Bei Bedarf kommt mehr lokale Rechenleistung über einen Rechner im Heimnetz (7.6). |
| Datenklassen | Jeder Ordner ist **„Cloud erlaubt“** oder **„Nur lokal“** (vererbt, **frei konfigurierbar**, keine feste Vorbelegung). „Nur lokal“ ist **rechtlich begründet**, z.B. bei Gesundheitsdaten oder Daten Dritter. Diese Daten werden nie in der Cloud verarbeitet, auch nicht auf einer eigenen Cloud-VM. Sie landen in keinem S3-Cache und gehen nur clientseitig verschlüsselt ins Backup (siehe 7.4, 15.1). |
| Rolle der Cloud | **Das NAS bleibt die Zentrale.** Object Storage (S3) dient als **verschlüsseltes externes Backup** und als **Beschleuniger für den Zugriff von außen** (Freigabe-Links, Downloads unterwegs). Eine Speicher-Schnittstelle im Server hält einen späteren Umzug in die Cloud offen (4.6). **Herstellerunabhängig** über die S3-API, Start mit Hetzner. |
| Nutzer | Familie/Team (bis ~20 Konten), Zugriff von unterwegs, **öffentliche Freigabe-Links**. |
| Hardware | **DS918+**: Celeron J3455, 4 Kerne, **kein AVX**, DSM-Kernel 4.4, 16–20 GB RAM, **SSD-Lese-/Schreib-Cache**. Besonderheiten siehe 3.1. |
| Bestandsdaten | `homes/<user>/Drive` (Synology Drive) wird direkt als „Meine Ablage“ eingebunden, die **Synology-Drive-Team-Ordner als „Geteilte Ablagen“** (4.1). |
| Zugriff von außen | Eigene Domain, Portweiterleitung. Der heutige DSM-Reverse-Proxy wird durch einen **Caddy-Container mit eigener IP** (macvlan) ersetzt, der HTTP/3 kann (siehe 5.9, 15). |
| Heimanschluss | Upload heute **50 Mbit/s**, ab **Januar 2027 400 Mbit/s**. |
| Transport | **Alles über HTTPS auf Port 443**, bevorzugt **HTTP/3 (QUIC)**, automatischer Fallback auf HTTP/2 über TCP (siehe 5.9). |
| Apple | Apple Developer Account ist vorhanden. |
| Anmeldung | **Eigene Konten. Jede Anmeldung mit zwei Faktoren: Passwort + TOTP-Einmalcode oder Passkey.** Wiederherstellungscodes, Step-up für sensible Aktionen, widerrufbare Gerätetokens (16.1). Kein LDAP/SSO. |
| Plattformen v1 | Web, macOS, iOS. **Windows/Android vorerst nicht.** Der Rust-Kern hält sie für später offen. |
| Office | **Nur Vorschau** (lokal per LibreOffice → PDF). **Keine Microsoft-Office-Web-Integration** (kein Office Online/Microsoft 365/WOPI zu Microsoft). Bearbeiten lokal in den Desktop-Apps über den Sync. |
| Backup | Hyper Backup nach S3 ist **eingerichtet** (Stand 2026-10). |
| Datenmenge | 300k – 3 Mio Dateien, mehrere TB. |
| Tech-Stack | **Rust** (Server + gemeinsame Sync-Engine), **Swift/SwiftUI** (Mac/iOS, Engine via UniFFI), **Nuxt/Vue/TypeScript** (Web). |

### Annahmen & Nicht-Ziele

- **Nicht in v1:** Bearbeiten im Browser (falls je, dann nur selbst gehostet, z.B. Collabora; **nie Microsoft**), Gesichtserkennung,
  Ende-zu-Ende-Verschlüsselung, Kommentare an Dateien, LDAP/SSO.
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
7. **Herstellerunabhängig.** Nach außen gibt es nur offene Standards: die S3-API für Speicher (beliebiger Anbieter), OpenAI-kompatible APIs für KI,
   Docker Compose, Postgres und normale Dateien. Ein Anbieterwechsel ist Konfiguration, keine Entwicklung.

---

## 3. Systemüberblick

```
                         Internet / LAN
                               │  TCP 443 (HTTP/2) + UDP 443 (HTTP/3/QUIC)
   Caddy-Container, eigene IP per macvlan (TLS, Let's Encrypt, HTTP/3)
                               │
┌──────────────────────────────┼───────────────────────────────────────────┐
│ Container Manager – Projekt "xlrx" (docker compose, amd64)                │
│                                                                           │
│  xlrx-server  (Rust, axum, tokio)                                         │
│   ├─ REST/JSON-API + Server-Sent Events, liefert Web-UI (statisch) aus    │
│   ├─ Sync: Journal, Uploads/Downloads (Chunks), Versionen, Papierkorb     │
│   ├─ Watcher: inotify + periodische Abgleich-Scans (externe Änderungen)   │
│   ├─ Suche: Tantivy (lexikalisch) + pgvector (semantisch), hybrid         │
│   ├─ Startseite / Aktivität / Benachrichtigungen (SSE, APNs)              │
│   └─ Auth, Freigaben, Links, Admin                                        │
│          │                            │                                   │
│  postgres 18 + pgvector 0.8     xlrx-worker  (Rust, gleiche Codebasis)    │
│   Metadaten, Journal, Rechte,    ├─ Thumbnails/Vorschau (libvips, ffmpeg, │
│   Job-Queue, Texte, Vektoren     │   LibreOffice → PDF)                   │
│                                  ├─ Text-Extraktion (→ tika), EXIF, Geo   │
│  tika  (Apache Tika Server)      ├─ OCR lokal (Tesseract, Fallback)       │
│                                  └─ KI-Provider ──────────────────────────┼──► Scaleway (Paris)
│  embed-local (OpenAI-kompatibel,    (OpenAI-kompatibel)                   │    Cloudflare Workers AI
│   kleines Modell, ohne AVX) ◄────── lokal für sensible Ordner             │
│                                                                           │
│  xlrx-server ── Außen-Cache (nur „Cloud erlaubt“) ────────────────────────┼──► S3 Object Storage
│  Hyper Backup (DSM) ── verschlüsseltes Backup ────────────────────────────┼──► S3 Object Storage
└───────────────────────────────────────────────────────────────────────────┘
  /volume1/homes/<user>/Drive  Btrfs, normale Dateien    /volume1/xlrx-state/…  DB, Index, Thumbs
                                                          (profitiert vom SSD-Cache)
  optional: KI-Rechner im Heimnetz (Mac mini / Mini-PC) für „Nur lokal“-Daten, siehe 7.6

Clients
  Mac-App  ── xlrx-core (Rust via UniFFI) ── Spiegel-Modus (FSEvents) │ File-Provider-Modus
              Transport: URLSession (HTTP/3)  FinderSync-Badges, Menüleiste, LaunchAgent
  iOS-App  ── xlrx-core ── File-Provider-Extension (Dateien-App), Share-Extension, Push
  Browser  ── Web-UI (Nuxt, PWA)
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
| embed-local | 0,5–1 GB (Modell + Laufzeit) |
| Rest | Page-Cache für Index & Dateien |

### Plattenbedarf für abgeleitete Daten (bei ~1,5 Mio Dateien, grob)
Postgres 10–30 GB (inkl. extrahierter Texte und Vektoren), Tantivy 5–15 GB, Thumbnails/Vorschauen 50–100 GB.
Alles liegt auf dem normalen Volume. Der vorhandene **SSD-Lese-/Schreib-Cache** fängt die zufälligen Zugriffe
von Postgres und Tantivy ab. Ein separates SSD-Volume ist nicht nötig.

### 3.1 Besonderheiten des DS918+
Der DS918+ ist für dieses Vorhaben gut nutzbar, setzt aber Grenzen, die der Plan von Anfang an berücksichtigt:

| Eigenschaft | Folge für den Plan |
|---|---|
| **Celeron J3455 ohne AVX/AVX2** (nur bis SSE4.2, „x86-64-v2“) | Viele ML-Laufzeiten (ONNX Runtime, PyTorch, Standard-Builds von llama.cpp) stürzen mit *Illegal Instruction* ab. **Alle Images werden für x86-64-v2 gebaut** (kein `target-cpu=native`, keine AVX-Annahmen, auch bei pgvector). Die CI führt die Binaries unter `qemu-x86_64 -cpu Denverton` aus, einer CPU ohne AVX. So fallen AVX-Abhängigkeiten auf, bevor sie auf dem NAS crashen. |
| **4 schwache Kerne** | Der Sync-Server muss immer reaktionsfähig bleiben. Darum haben schwere Jobs (OCR, LibreOffice, lokale Embeddings) feste Kern-Limits: Der Worker bekommt max. 2 Kerne per `cpus:` in compose, mit niedriger Priorität. Vorschauen von Office-Dateien entstehen bei Bedarf und werden gecacht. |
| **DSM-Kernel 4.4** | Kein dateisystemweites fanotify → inotify + Abgleich-Scans (4.4). Kein UDP-GSO → QUIC kostet hier spürbar mehr CPU als TCP (siehe 5.9). Btrfs-Reflinks funktionieren (`BTRFS_IOC_CLONE`). |
| **Intel HD 500 (Quick Sync)** | Optional später: Video-Thumbnails per VAAPI (`/dev/dri` in den Worker durchreichen). |
| **SSD-Cache** | Beschleunigt Postgres/Tantivy automatisch. DSM verlangt für einen Lese-/Schreib-Cache ohnehin zwei SSDs im RAID 1, ein einzelner SSD-Ausfall kostet also keine Daten. Postgres-WAL und Index-Merges erhöhen die Schreiblast. Die SSD-Lebensdauer sollte deshalb im DSM-Speicher-Manager im Blick bleiben. |

Ein **Hardware-Spike in M0** misst die echten Werte auf deinem Gerät, bevor Feature-Entscheidungen davon abhängen:
- Durchsatz des lokalen Embedding-Modells
- QUIC- und HTTP/2-Durchsatz
- Hash-Geschwindigkeit
- Subvolume-übergreifende Reflinks im `/volume1`-Mount
- Rechte-Durchsetzung für den Benutzer `xlrx` und Vererbung der Synology-ACLs
- Verhalten von Synology Drive bei kurzlebigen Temp-Dateien
- Copy-on-Write für die DB-Freigabe abschalten (13.2)

---

## 4. Speicherung auf dem NAS

### 4.1 Layout

```
/volume1/                                 ← das ganze Volume, EIN Mount, im Container: /mnt/volume1
  homes/klaus/Drive/…                     ← "Meine Ablage" (bestehender Synology-Drive-Ordner)
  homes/anna/Drive/…
  Familie/…                               ← bestehender Synology-Drive-Team-Ordner → "Geteilte Ablage"
  Buero/…                                 ← dito
  xlrx-state/                             ← eigene Freigabe: nur Benutzer xlrx, im Netzwerk verborgen
    store/versions/ab/cd/<blake3>         ← alte Versionen (Reflink-Klone, inhaltsadressiert)
    store/trash/<node-id>/…               ← Papierkorb (30 Tage)
    store/staging/<upload-id>             ← Uploads im Aufbau
    dumps/                                ← nächtlicher pg_dump (→ Hyper Backup)
  xlrx-db/                                ← eigene Freigabe für DB und abgeleitete Daten (13.2)
    postgres/  index/  thumbs/            ← Postgres, Tantivy, Vorschauen (nicht im Backup, nur die Dumps)
```

- **Warum das ganze Volume:** Jede Synology-Freigabe (`homes`, jeder Team-Ordner, `xlrx-state`) ist ein eigenes Btrfs-Subvolume.
  Reflinks funktionieren zwischen Subvolumes desselben Dateisystems, aber **nur innerhalb desselben Mounts**.
  Ein einziger Mount von `/volume1` erlaubt deshalb einen **zentralen** Bereich für Versionen und Papierkorb in `xlrx-state`.
  In den Freigaben der Nutzer liegen damit keine versteckten xlrx-Ordner. Synology Drive, SMB und File Station sehen davon nichts.
- **Roots sind konfigurierbar:**
  - `homes/<user>/Drive` → „Meine Ablage“ der Person
  - **bestehende Synology-Drive-Team-Ordner → „Geteilte Ablagen“**. Mitglieder und Rollen werden in xlrx gepflegt (eigene Konten, 9.1).
  - neue geteilte Ablagen → neue Freigabe oder Unterordner einer bestehenden

  Migration bedeutet Einbinden statt Kopieren.
- **Rechte eng trotz breitem Mount:** Der Container läuft **nie als root**, sondern als DSM-Benutzer `xlrx`. Dieser hat DSM-Rechte nur auf `homes`,
  die eingebundenen Team-Ordner, `xlrx-state` und `xlrx-db`. Die Rechte werden direkt dem Benutzer gegeben, nicht nur über Gruppen.
  Der Kernel setzt die Synology-ACLs auch im Container durch, alles andere auf dem Volume bleibt unzugänglich. Das wird im M0-Spike nachgewiesen.
  Ebenso wird geprüft, ob neue Dateien die ACLs des Elternordners erben, damit Nutzer sie per SMB weiter bearbeiten können.
  Falls nicht, setzt der Server die Rechte nach dem Schreiben explizit.
- **Verschlüsselte Freigaben** (eCryptfs) sind eigene Mounts. Dort gibt es keine Reflinks, Versionen werden normal kopiert.
- **Parallelphase mit Synology Drive:** Was xlrx in `Drive`- und Team-Ordner schreibt, synchronisiert Synology Drive weiter auf
  die Macs, und umgekehrt sieht xlrx Änderungen von Synology Drive als externe Änderungen (4.4). Einzige Regel:
  Auf einem Mac nie Synology-Drive-Client und xlrx-Client auf **denselben** lokalen Ordner loslassen.

### 4.2 Versionen ohne Platzkosten (Btrfs-Reflinks)
Bevor eine Datei überschrieben wird, legt der Server einen **Reflink-Klon** der alten Version unter `xlrx-state/store/versions/` an
(`FICLONE`/`BTRFS_IOC_CLONE`, Subvolume-übergreifend im selben Mount). Das kostet keine Zeit und keinen Platz, solange sich die Blöcke nicht unterscheiden.
Fallback ohne Reflink: normale Kopie. Aufbewahrung konfigurierbar, z.B. alle Versionen 30 Tage, danach täglich/wöchentlich, max. N.

### 4.3 Atomare Schreibvorgänge & Absturzsicherheit
`rename` funktioniert nur innerhalb eines Subvolumes. Der Upload wird deshalb zentral aufgebaut und erst im letzten Schritt
per Reflink in den Zielordner geholt.

Commit eines Uploads:
1. **Intent** in Postgres schreiben (`pending_ops`: was soll passieren, mit welcher Basis-Version).
2. Datei in `xlrx-state/store/staging/` zusammensetzen, `fsync`, BLAKE3 der ganzen Datei prüfen.
3. Prüfen, ob die Zieldatei noch der erwarteten Version entspricht (inode/size/mtime/ctime + ggf. Hash).
4. Alte Version per Reflink nach `store/versions/`.
5. Staging-Datei per Reflink in eine **kurzlebige Temp-Datei im Zielordner** klonen (`.xlrx-tmp-<id>`), dann `fsync`.
   Das sind nur Metadaten und dauert auch bei großen Dateien Millisekunden.
6. `rename(temp → ziel)`, `fsync` des Verzeichnisses.
7. DB-Transaktion: Knoten, Version, Journal-Eintrag, Intent erledigt.

Löschen funktioniert analog: Reflink in `store/trash/`, dann `unlink`. Wiederherstellen geht den umgekehrten Weg.
Nach einem Absturz arbeitet der Server beim Start alle offenen Intents **idempotent** ab (vorwärts oder zurück) und räumt
verwaiste Temp-Dateien weg. Die mtime des Clients wird übernommen (`utimensat`), damit SMB-Nutzer echte Änderungsdaten sehen.
Der M0-Spike prüft, dass Synology Drive die Temp-Dateien in der Parallelphase nicht mitsynchronisiert, weil sie nur Millisekunden existieren.

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
  `Thumbs.db`, `~$*`-Lock-Dateien, eigene Temp-Dateien `.xlrx-tmp-*`. Die genauen Hilfsdateien von Synology Drive in `homes/<user>/Drive`
  werden im M0-Spike erfasst und ergänzt.
- Hinweis: Der DSM-Kernel ist je nach Modell 4.4 oder 5.10. Darum setzt der Plan kein dateisystemweites fanotify voraus.

### 4.5 Namen & Plattform-Eigenheiten
- Namen werden serverseitig in **NFC** gespeichert. macOS liefert oft NFD („Ä“ zerlegt).
- **Case-Insensitivity:** APFS ist meist case-insensitiv, Btrfs nicht. Der Server erzwingt Eindeutigkeit **case-insensitiv pro Ordner**
  für Änderungen über die API. Entstehen über SMB trotzdem „Foo.txt“ und „foo.txt“, synchronisiert der Mac-Client
  eine davon als `foo (Groß-/Kleinschreibungskonflikt).txt`.
- Unzulässige Zeichen und zu lange Pfade bekommen eine Warnung in der UI und werden nie still verworfen.

### 4.6 Speicher-Schnittstelle (Cloud-Umzug offenhalten)
Der Server greift auf Inhalte nur über eine Schnittstelle `ContentStore` zu: Inhalt nach Hash lesen (auch Byte-Bereiche),
Version schreiben, Version aufbewahren, Papierkorb. Sync-Protokoll, Journal und Suche kennen nur Node-IDs und Hashes, keine Pfade auf der Platte.

| Implementierung | Wann | Eigenschaften |
|---|---|---|
| `PlainFsStore` | **ab M1** | normale Dateien auf dem NAS, Watcher, Reflink-Versionen (dieses Kapitel) |
| `S3CacheStore` | ab M3b | Spiegel einzelner Inhalte in S3 für den Außenzugriff (Kapitel 15.2), kein Primärspeicher |
| `S3PackStore` | **nur bei Bedarf** | inhaltsadressierter Primärspeicher in S3: Chunks gebündelt in 16–64-MB-Objekten wie bei restic, mit Speicherbereinigung |

Damit bleibt ein späterer Umzug machbar, etwa wenn der DS918+ ersetzt wird: auf ein neues NAS mit derselben Implementierung oder in die Cloud
mit `S3PackStore`. Dann würde das NAS zum Spiegel-Client per Linux-Client ohne Oberfläche, damit SMB und Fotos weiter funktionieren.
Heute wird davon nur die Schnittstelle gebaut, keine S3-Primärspeicherung.

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
- **Push statt Polling:** Ein langlebiger HTTP-Stream (`/api/sync/notify`, Server-Sent Events) sendet `{seq}` bei neuen
  Änderungen. Der Client holt dann gezielt ab. SSE statt WebSocket, weil SSE ein normaler HTTP-Request ist: Es funktioniert
  über HTTP/2 **und** HTTP/3 und braucht keinen Upgrade-Mechanismus, den Proxies und Firewalls blockieren könnten.
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
   damit kein Body-Limit eines Proxys greift.
4. `POST /api/uploads/{id}/commit`: Der Server setzt aus vorhandenen Chunks (Hash wird beim Lesen verifiziert) und neuen Chunks zusammen
   und schreibt atomar (siehe 4.3).
   - `base_version` passt nicht zur aktuellen Version → **Konflikt**. Der Server legt die eingehende Version als
     `Name (Konflikt – Klaus' MacBook 2026-10-02 14.03).ext` daneben an. Beide bleiben erhalten.
     Diese Auflösung findet serverseitig statt, damit sich alle Clients gleich verhalten.
- **Viele kleine Dateien:** Batch-Endpunkte (mehrere Dateien pro Request, gestreamt) vermeiden ein Request-Gewitter, z.B. bei git-Checkouts.
- **Umgesetzt (M1):** Upload-Sitzungen (`POST /api/uploads` mit Ziel „neue Datei“, „neuer Inhalt“ oder „Inhalt für den Sync“,
  `PUT /api/uploads/{id}/parts?offset=…` mit höchstens 8 MiB und optionaler SHA-256-Prüfsumme, `GET` für den Stand, `…/commit` mit
  optionalem Inhalts-Hash). Ein Teil wird erst bestätigt, wenn er auf der Platte ist (fsync). Schon Empfangenes wird nie überschrieben.
  Nach einem Abbruch schickt der Client nur die fehlenden Bereiche. Ein Absturz mitten im Speichern wird beim Neustart sicher aufgelöst.
  Unbenutzte Sitzungen verschwinden nach 24 h. Die Web-App lädt Dateien über 8 MiB so hoch, mit automatischer Wiederholung und „Fortsetzen“.
  **Delta-Uploads** (nur fehlende Chunks, Chunk-Index) folgen mit dem Mac-Client (M5); sie passen als „Bereiche, die der Server schon hat“ hinein.
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
- LAN-Direktverbindung: Der Client erkennt, wenn der Server lokal erreichbar ist. Bevorzugt geht das per Split-DNS, dann gelten dieselbe Domain und
  dasselbe Zertifikat. Alternativ per LAN-URL mit gepinntem Server-Zertifikat (siehe 5.9).
- Parallele Transfers mit adaptiver Parallelität, Bandbreitenlimits und Zeitplänen.
- FSEvents mit persistierter Event-ID (`sinceWhen`). Nach Neustart oder Schlaf werden nur die Änderungen seitdem verarbeitet, nicht alles gescannt.
- Hintergrund-QoS (`utility`), Pausieren im Akkubetrieb oder Stromsparmodus optional.
- Zielwerte: Leerlauf ~0 % CPU, Client-RAM < 200 MB bei 1 Mio Dateien, BLAKE3 > 1 GB/s auf Apple Silicon.

### 5.9 Transport: HTTPS, bevorzugt HTTP/3 (QUIC)
**Grundsatz:** Sync, Web-UI, Benachrichtigungen und Links laufen **ausschließlich über HTTPS auf Port 443**. Es gibt keine
Sonderports und kein eigenes Protokoll. Durch jede Firewall, die Web-Surfen erlaubt, kommt also mindestens HTTP/2 über TCP.

**HTTP/3 (QUIC)** ist der bevorzugte Weg, mit automatischem Fallback:
- Der Proxy kündigt `Alt-Svc: h3=":443"` an. Clients versuchen QUIC über **UDP 443**. Ist UDP blockiert, bleibt die Verbindung
  ohne Unterbrechung auf HTTP/2/TCP.
- Ehrlicher Hinweis: QUIC kommt **nicht leichter** durch Firewalls. UDP 443 ist in Firmen- und Hotelnetzen sogar öfter gesperrt als TCP 443.
  Die Firewall-Freundlichkeit kommt vom Port 443, der Fallback sorgt dafür, dass das immer funktioniert. QUIC lohnt sich trotzdem:
  - **Verbindungsmigration:** Wechsel WLAN ↔ Mobilfunk ohne Abbruch laufender Uploads (ideal für iPhone und MacBook)
  - schnellerer Verbindungsaufbau (1-RTT/0-RTT)
  - **kein Head-of-Line-Blocking:** viele parallele Chunk-Transfers bleiben bei Paketverlust flüssig
- **Apple-Clients nutzen `URLSession` als Transport.** URLSession spricht HTTP/3 nativ (`assumesHTTP3Capable`) und
  beherrscht iOS-Hintergrund-Transfers. Es respektiert System-VPN und Proxy-Einstellungen und ist energieeffizient.
  Der Rust-Kern enthält die Protokoll-Logik. Den eigentlichen Transport implementiert die Plattform über eine UniFFI-Schnittstelle.
  Spätere Clients (Windows/Linux/Android) nutzen eine Rust-Implementierung (`quinn`/`h3` bzw. `reqwest`).
- **QUIC-Terminierung in einem Caddy-Container**, der den heutigen DSM-Reverse-Proxy ersetzt. Der DSM-Proxy kann **kein** HTTP/3.
  Caddy spricht HTTP/3 ab Werk, holt Let's-Encrypt-Zertifikate automatisch, hat kein Body-Limit und streamt SSE ohne Puffern.
  Vom Proxy zum `xlrx-server` reicht HTTP/1.1 oder h2c im internen Docker-Netz. Einrichtung mit eigener IP siehe 15.3.
- **Router:** TCP 80/443 und **UDP 443** an die IP des Caddy-Containers weiterleiten.
- **DS918+-Besonderheit:** QUIC läuft im Userspace. Ohne UDP-GSO (Kernel 4.4) braucht es auf dem J3455 deutlich mehr CPU als TCP.
  Bei heute 50 Mbit/s Upload reicht das sicher. Bei 400 Mbit/s ab Januar 2027 kann die CPU zum Engpass werden; der Spike misst das.
  Gegenmittel: Große Downloads von außen übernimmt der S3-Cache (15.2), und Caddy kann HTTP/3 notfalls abschalten. Im LAN kann QUIC Gigabit ausbremsen. Falls der M0-Spike das bestätigt,
  bekommen die Clients für das LAN einen eigenen Endpunkt nur mit HTTP/2, z.B. `drive-lan.<domain>` per Split-DNS mit gültigem Let's-Encrypt-Zertifikat (DNS-Challenge oder Wildcard).
  Die Clients schalten automatisch um, sobald er erreichbar ist. Die UDP-Puffer (`net.core.rmem_max`/`wmem_max`) werden per Boot-Aufgabe vergrößert.
- **Split-DNS** (Router, Pi-hole oder DSM-DNS-Server): Im LAN zeigt die Domain auf die IP des Caddy-Containers. So gelten dasselbe Zertifikat
  und dieselbe URL überall, und der Client muss nichts umschalten.

---

## 6. Suche

### 6.1 Was indexiert wird
| Quelle | Wie |
|---|---|
| Dateiname, Pfad | Tantivy, Edge-N-Gramme für Suche während der Eingabe |
| Text aus PDF/Office/Pages/Numbers/Keynote/EML/MSG/TXT/MD/Code | Text-Dateien direkt, PDFs mit `pdftotext` (Poppler) im Server, Office/iWork/Mails mit Apache Tika (Container im internen Netz). PDFs ohne Textebene gehen zur OCR. |
| Gescannte PDFs & Bilder mit Text | Tesseract `deu+eng` lokal (seit M2: gescannte PDFs bis 30 Seiten, TIFF-Scans); ab M4 für Fotos zusätzlich OCR per Vision-Modell (nur „Cloud erlaubt“) |
| Bildinhalt | „Cloud erlaubt“: Vision-Modell erzeugt Beschreibung, Tags, erkannten Text und Dokumenttyp (siehe 7.2). „Nur lokal“: **CLIP-Bildvektoren** auf dem NAS (siehe 7.5) |
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
- **Drei Vektor-Indizes:**
  - `cloud` (z.B. `qwen3-embedding-8b`, 1024 Dim.) für normale Ordner
  - `lokal` (kleines Modell auf dem NAS, siehe 7.5) für sensible Ordner
  - `clip` (Bildvektoren für Fotos in sensiblen Ordnern)

  Jede Datei liegt je nach Datenklasse ihres Ordners in `cloud` **oder** in `lokal`/`clip`.
  Vektoren verschiedener Modelle sind nicht vergleichbar. Darum wird die Anfrage für jeden Index mit dem passenden Modell embedded,
  und die Ergebnislisten werden erst über die Rangfolge zusammengeführt (6.4).

### 6.4 Hybride Rangfolge
1. Bis zu vier Listen parallel: lexikalisch Top-100 ‖ `cloud` Top-100 ‖ `lokal` Top-100 ‖ `clip` Top-100. Die Anfrage wird pro
   Index mit dem passenden Modell embedded, für `clip` mit dem CLIP-Text-Encoder. Das Query-Embedding hat ein Timeout von ~400 ms;
   fällt eine Quelle aus, fehlt nur ihre Liste.
2. **Reciprocal Rank Fusion** arbeitet nur mit Rängen, nicht mit Scores. Darum lassen sich die Listen zweier Embedding-Modelle sauber mischen. Danach Boosts: Treffer im Dateinamen, Aktualität, eigene Nutzungshäufigkeit, „bei mir freigegeben“.
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

**Umgesetzt in M2:** Der Suchindex folgt dem Journal und den gelesenen Texten (Cursor in seinen eigenen Commits) und
legt für jeden Inhalt ohne Text einen Auftrag `extract` an. Ein Worker liest eine gegen den Hash geprüfte Kopie der
Datei, so landet ein Text nie beim falschen Inhalt. Fehlt ein Programm (Tika, Tesseract), wartet der Auftrag
(`waiting`) und läuft beim nächsten Start mit dem Programm. Unlesbare Dateien (verschlüsselt, kaputt) bekommen einen
leeren Text und werden nicht wiederholt. Alles läuft auf dem NAS.

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
Derselbe Mechanismus bindet den lokalen Embedding-Dienst `embed-local` auf dem NAS an (7.5).

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
- **Pro Ordner/Ablage eine Datenklasse** (vererbt): **„Cloud erlaubt“** oder **„Nur lokal“**. Sie gilt für **alle** Cloud-Wege:
  KI-APIs, eigene Cloud-VMs, den S3-Außen-Cache (15.2) und das Backup (15.1).
  - **Cloud erlaubt:** volle Funktion wie oben beschrieben.
  - **Nur lokal** (rechtlich begründet, z.B. Gesundheitsdaten, Daten Dritter, Verträge): **Verarbeitung ausschließlich im Heimnetz.**
    Auch eine eigene, gemietete Cloud-VM ist ausgeschlossen, denn sie wäre ebenfalls Auftragsverarbeitung.
    - Extraktion per Tika, OCR per Tesseract
    - **semantische Textsuche** über das lokale Embedding-Modell
    - **Bildsuche nach Inhalt** über lokale CLIP-Vektoren (7.5)
    - Freigabe-Links sind möglich, werden aber immer direkt vom NAS ausgeliefert, nie über den S3-Cache.
    - Ins Backup gelangen die Daten nur **clientseitig verschlüsselt** (15.1).
  - Wechselt ein Ordner von „Cloud erlaubt“ auf „Nur lokal“, werden seine Cloud-Ergebnisse (Vektoren, Bildbeschreibungen) **und seine
    S3-Cache-Objekte** gelöscht, und alles wird lokal neu erzeugt. Die Löschung wird protokolliert.
  - Voreinstellung für neue Ablagen ist einstellbar. Sicherer Standard: „Nur lokal“, bis aktiv freigegeben.
  - Ein Verzeichnis der Verarbeitungstätigkeiten zeigt pro Ablage, welche Daten wohin gehen. Im Admin-Bereich ist es als Export verfügbar.
- Es werden nur minimierte Daten gesendet: Text-Chunks oder Bilder verkleinert auf ≤ 1024 px, **ohne EXIF/GPS**.
  Dateinamen und Pfade werden standardmäßig nicht mitgeschickt.
- Bei geteilten Ablagen entscheidet der Besitzer bzw. Verwalter über die Datenklasse.
- Vor dem Go-live: AVV mit dem Anbieter abschließen und die Bedingungen zu Speicherung und Training prüfen. Laut Anbieterangaben wird nicht gespeichert und nicht trainiert.

### 7.5 Lokales Embedding-Modell auf dem NAS
**Ja, das geht, mit Abstrichen bei der Geschwindigkeit.**

- Ein eigener Container **`embed-local`** bietet einen OpenAI-kompatiblen Endpunkt `/v1/embeddings`. Für den Worker ist das einfach ein
  weiterer Provider (7.1). Es braucht keine Sonderlogik, und das Modell ist austauschbar.
- **Modellkandidaten** (mehrsprachig, gut für Deutsch):

  | Modell | Größe | Dimensionen | Lizenz | Einschätzung auf dem J3455 |
  |---|---|---|---|---|
  | `multilingual-e5-small` | 118 Mio Parameter | 384 | MIT | **Standard-Empfehlung.** Schnell genug, solide Qualität |
  | `embeddinggemma-300m` | 300 Mio Parameter | 768 (Matryoshka: 512/256/128) | Gemma-Lizenz | bessere Qualität, aber ~3–5× langsamer; nur wenn der Spike das zulässt |

  Beide laufen int8-quantisiert. Zum Vergleich: Das Cloud-Modell `qwen3-embedding-8b` ist deutlich stärker. Lokal ist ein Kompromiss für die sensiblen Ordner.
- **Laufzeit ohne AVX:** ONNX Runtime aus dem Quellcode ohne AVX gebaut **oder** llama.cpp mit GGUF-Modell
  (`GGML_NATIVE=OFF`, AVX-Optionen aus). Welche Variante schneller und stabil ist, entscheidet ein Benchmark im M0-Spike direkt auf dem DS918+.
- **Erwartete Geschwindigkeit** (Überschlag, wird gemessen):
  - `multilingual-e5-small` mit 2 Kernen: ~0,5–2 Chunks/s (512 Tokens). Nachts dürfen alle 4 Kerne rechnen.
  - Ein Query-Embedding dauert ~50–100 ms, ist also unproblematisch für die Suche.
- **Damit das reicht:** Im lokalen Index werden pro Dokument nur die ersten ~3 Chunks und der OCR-Text embedded. Den vollständigen Text
  deckt die lexikalische Suche ab. Rechenbeispiel: sensible Ordner mit 20k Dokumenten → ~60k Chunks → **Erst-Indexierung ca. 1 Tag**,
  danach laufend in Echtzeit.
- **Alles lokal statt Cloud** wäre technisch möglich (ein einziger Vektorraum, keine Cloud-Abhängigkeit für Text). Bei ~1 Mio Chunks
  läge die Erst-Indexierung auf dem J3455 aber bei **etwa 1–3 Wochen**, und die Qualität wäre geringer. Daher die Empfehlung: Cloud für
  normale Ordner, lokal für sensible.
- **Bildsuche ohne Cloud: CLIP.** Statt Bildbeschreibungen, für die der J3455 zu schwach ist, berechnet `embed-local` für Fotos in
  „Nur lokal“-Ordnern **CLIP-Bildvektoren**, z.B. `clip-ViT-B-32-multilingual-v1`. Der Bild-Encoder ist klein (~4,4 GFLOP pro Bild),
  nach Überschlag ~0,5 s pro Bild auf 2 Kernen. 10.000 Fotos sind damit in wenigen Stunden fertig. Der mehrsprachige Text-Encoder
  macht deutsche Anfragen wie „Hund im Schnee“ direkt mit den Bildern vergleichbar. Die Qualität liegt unter echten Bildbeschreibungen,
  reicht aber für Motive gut aus. Die Vektoren kommen in einen eigenen Index `clip` (siehe 6.3).

### 7.6 Mehr Rechenleistung für „Nur lokal“-Daten
Für „Cloud erlaubt“ liefern die APIs die großen Modelle. Für „Nur lokal“ kommt zusätzliche Rechenleistung **nur aus dem Heimnetz**.
`embed-local` ist ein OpenAI-kompatibler Endpunkt. Ein zusätzlicher Rechner ist deshalb eine Konfigurationsänderung ohne Code.

| Option | Kosten | Was zusätzlich möglich wird |
|---|---|---|
| **Nur DS918+** (Start) | – | `e5-small`, CLIP, Tesseract-OCR, alles langsam |
| **Mini-PC mit Intel N100/N150**, 16 GB | ~150–250 € einmalig, wenige Watt | AVX2: `bge-m3` bzw. `qwen3-embedding-0.6b`, schnelleres CLIP und OCR, kleine Vision-Modelle (langsam) |
| **Mac mini (Apple Silicon)**, 16 GB+ | ab ~700 € einmalig | Embedding-Modelle bis ~4B Parameter, **echte Bildbeschreibungen** mit 7–12B-Vision-Modellen in wenigen Sekunden pro Bild, Apple-Vision-OCR (sehr gut auf Deutsch). Etwa 20–50× so schnell wie das NAS |

Entscheidung **nach dem M0-Spike**: Reicht der DS918+ für die Menge an „Nur lokal“-Daten, bleibt es dabei. Ein Wechsel des
lokalen Embedding-Modells löst ein Neu-Embedding der lokalen Indizes aus. Das läuft im Hintergrund, während der alte Index weiter antwortet.
Gemietete Cloud-VMs (Scaleway DEV1-M, Hetzner CX43 o.ä.) kommen für „Nur lokal“ nicht in Frage. Für „Cloud erlaubt“ bringen sie
gegenüber den APIs keinen Vorteil.

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
Glocke im Web (live per SSE), Push auf iOS und Mac (APNs, direkt vom Server mit `.p8`-Schlüssel), optional E-Mail (SMTP)
bei neuen Freigaben. Pro Person einstellbar.

---

## 9. Teilen & Berechtigungen

### 9.1 Modell
- **Meine Ablage** pro Person und **Geteilte Ablagen** (Team-Ordner mit Mitgliedern und Rollen, gehören keiner Einzelperson).
  Die bestehenden **Synology-Drive-Team-Ordner** werden als Geteilte Ablagen eingebunden. Ihre Mitglieder legt ein Admin beim Einbinden in xlrx fest,
  weil xlrx eigene Konten nutzt. Die DSM-Rechte für SMB bleiben unabhängig davon bestehen.
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

**Nuxt 4** (Vue 3, Single-Page-App als statischer Build, vom Server ausgeliefert), TypeScript. Der API-Client wird aus OpenAPI generiert (utoipa).
Live-Updates kommen per Server-Sent Events. PWA-fähig, Deutsch/Englisch, Dark Mode.
Die Web-App folgt dem App-Entwurf (12): Papierton und Tinte (wählbar unter „Darstellung“, dazu ein dunkler Modus), Schrift Geist
(mit der App ausgeliefert, kein Schriftendienst), gestrichelte Hilfslinien, Sheets für Aktionen und Rückfragen, auf dem Handy die
schwebende Leiste mit „+“. Startseite und Anmeldung zeigen die radierte Berglandschaft aus dem App-Entwurf, passend zur Tageszeit
(Morgen 5–10, Tag 10–17, Abend 17–21, Nacht 21–5 Uhr) und ohne Bewegung, wenn das System „Bewegung reduzieren“ verlangt.

Bereiche wie bei Google Drive: **Startseite**, **Meine Ablage**, **Geteilte Ablagen**, **Für mich freigegeben**, **Zuletzt verwendet**,
**Markiert**, **Papierkorb**, **Aktivität**, **Admin**.

Funktionen:
- **Virtualisierte Listen/Raster:** flüssig auch mit 100k Einträgen in einem Ordner. Mehrfachauswahl, Drag & Drop, Tastaturkürzel.
- **Upload von Dateien und ganzen Ordnern** per Drag & Drop, chunked und wiederaufnehmbar. BLAKE3 läuft als WASM im Web Worker,
  damit Dedup und Delta auch im Browser funktionieren.
- **Vorschau:**
  - Bilder inkl. HEIC/RAW (serverseitig konvertiert) und PDF (pdf.js)
  - Office/Pages (LibreOffice → PDF im Worker, rein lokal; **keine Microsoft-Office-Web-Integration**)
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
| **xlrx-core** | Rust-Bibliothek als XCFramework (UniFFI): Protokoll-Logik, Chunking, Sync-Engine, lokale SQLite. Den Netzwerk-Transport liefert die App über `URLSession` (HTTP/3, siehe 5.9). |

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
- **Gestaltung** nach dem Entwurf in Claude Design (Canvas „xlrx-drive iOS“): Papierton und Tinte, Schrift Geist, auf der Startseite
  die radierte Berglandschaft nach Tageszeit. Ihr Generator steckt schon in der Web-App (`web/app/utils/landscape.ts`) und wird
  für iOS nach Swift portiert. Bei 390 pt Breite muss er dieselben Pfade liefern wie der Entwurf.

---

## 13. Datenbanken & Datenmodell

### 13.1 Welche Datenbank wofür
| Einsatz | Wahl | Warum |
|---|---|---|
| **Server: Metadaten, Journal, Rechte, Jobs, Aktivität, Vektoren** | **PostgreSQL 18 + pgvector 0.8** (ein Container) | Siehe unten. Eine einzige Datenbank für alles Transaktionale. |
| **Volltextsuche** | **Tantivy** (Rust-Bibliothek im Server, keine eigene DB) | BM25, deutsche/englische Stemmer, Fuzzy, Snippets. Abgeleitet: jederzeit aus den Texten in Postgres neu aufbaubar. |
| **Clients (Mac/iOS): Sync-Zustand** | **SQLite** im Rust-Kern (WAL-Modus) | Drei Bäume, Hash-Cache, Chunk-Listen. Lokal, eine Datei, absturzsicher, ohne Server-Prozess. |

**Warum PostgreSQL:**
- **Transaktionen für den Zuverlässigkeits-Kern:** Journal, Intent-Log (4.3) und Knoten werden gemeinsam atomar geändert.
- **Mehrere gleichzeitige Schreiber:** Server, Worker und Watcher schreiben parallel. Bei SQLite gäbe es nur einen Schreiber pro Datenbank, über Container-Grenzen hinweg.
- **Job-Queue ohne Zusatzdienst:** `FOR UPDATE SKIP LOCKED` für Jobs und `LISTEN/NOTIFY` für Live-Updates (SSE). Redis oder RabbitMQ sind nicht nötig.
- **Rechte-Filter:** Arrays mit GIN-Index für `ancestor_ids` (9.3).
- **Vektoren im selben System:** Rechte-Filter und Vektorsuche laufen in einer Abfrage. HNSW über binär quantisierte Vektoren, gefilterte iterative Suche (pgvector ≥ 0.8). Eine separate Vektor-DB mit doppelt gepflegten Rechten entfällt.
- **Partitionierung** für die großen Ereignistabellen (`access_events` pro Monat).
- **Ausgereift:** Ein `pg_dump` ist ein vollständiges, portables Backup. Major-Upgrades laufen über `pg_upgrade`.

**Bewusst nicht gewählt:**

| Alternative | Grund |
|---|---|
| SQLite auf dem Server | Leichter, aber nur ein Schreiber, kein `SKIP LOCKED`/`NOTIFY`, Vektorsuche (`sqlite-vec`) weniger ausgereift. Bei 3 Mio Dateien und parallelen Jobs unnötig riskant. |
| OpenSearch/Elasticsearch | JVM mit 2–4 GB RAM, zusätzliche Datenhaltung mit eigenen Rechten. Auf dem DS918+ zu schwer. |
| Meilisearch | Schnell, aber schwache deutsche Wortstämme und RAM-hungrige Indexierung. Tantivy passt besser in den Rust-Server. |
| Qdrant (eigene Vektor-DB) | Gut, aber zusätzlicher Dienst, Rechte doppelt pflegen. pgvector reicht für 3–4 Mio Vektoren. |
| ParadeDB (`pg_search`, BM25 in Postgres) | Elegant, weil dann auch die Volltextsuche in Postgres läuft. Aber jüngere Erweiterung und AGPL. Option für später, die Suche ist austauschbar gekapselt. |
| MariaDB/MySQL, MongoDB | keine Vorteile für dieses Datenmodell |

### 13.2 Betrieb auf dem DS918+
- **Version:** **PostgreSQL 18** (aktuelle stabile Hauptversion). PostgreSQL 19 ist gerade im Release-Candidate-Stadium, GA wird für Oktober 2026 erwartet.
  Der Wechsel auf 19 erfolgt später per `pg_upgrade`, sobald ein pgvector-Image dafür existiert und 19.1/19.2 erschienen ist.
- **Image:** `pgvector/pgvector:pg18`. Es wird mit `OPTFLAGS=""` gebaut, also ohne `-march=native` und **ohne AVX-Annahme**.
  Damit läuft es auf dem J3455. Der QEMU-Check in der CI (17) stellt das trotzdem sicher.
- **Ablage:** eigene Freigabe **`xlrx-db`**, getrennt von `xlrx-state`:
  - **Grund:** Datenbanken schreiben zufällig in große Dateien. Auf Btrfs mit Copy-on-Write fragmentiert das stark. Für `xlrx-db` wird deshalb
    Copy-on-Write abgeschaltet (NOCOW bzw. Synology-Datenprüfsumme aus; das genaue Vorgehen klärt der M0-Spike).
  - Die Integrität sichern Postgres' eigene Datenprüfsummen, ab PG 18 standardmäßig aktiv.
  - Der Versionsspeicher in `xlrx-state` **muss** Copy-on-Write behalten. Reflinks zwischen NOCOW- und CoW-Dateien lehnt Btrfs ab.
    Deshalb sind es zwei getrennte Freigaben.
  - Der SSD-Cache fängt die zufälligen Lesezugriffe ab.
- **Startwerte** (16–20 GB RAM, werden im Betrieb nachjustiert):

  | Parameter | Wert | Zweck |
  |---|---|---|
  | `shared_buffers` | 3 GB | Arbeitsspeicher-Cache |
  | `effective_cache_size` | 8 GB | Planer-Hinweis auf den Page-Cache |
  | `work_mem` / `maintenance_work_mem` | 16 MB / 512 MB | Sortierungen / Index-Aufbau (HNSW) |
  | `max_connections` | 40 | Connection-Pool liegt in der App (`sqlx`) |
  | `synchronous_commit` | `on` | **kein Datenverlust** bei Stromausfall, nicht verhandelbar |
  | `checkpoint_timeout` / `max_wal_size` | 15 min / 4 GB | weniger Schreiblast auf SSD und HDD |
  | `wal_compression` | `zstd` | weniger WAL-Volumen |
  | `io_method` | `worker` (Standard) | `io_uring` braucht einen neueren Kernel als 4.4 |

- **Größe:** 10–30 GB bei ~1,5 Mio Dateien, inklusive extrahierter Texte und Vektoren.
- **Backup:** nächtlicher `pg_dump` (Custom-Format, komprimiert) nach `xlrx-state/dumps` → Hyper Backup.
  Point-in-Time-Recovery per WAL-Archivierung ist vorerst nicht nötig, kann aber nachgerüstet werden.
- **IDs:** 64-Bit-Sequenzen für Knoten und Journal (kompakt, schnell, sortierbar). Für extern sichtbare IDs, etwa Link-Tokens, werden Zufallswerte verwendet.

### 13.3 Schema-Skizze

Die wichtigsten Tabellen:

```
users(id, username, display_name, email, password_hash, totp_secret_enc, totp_last_step, is_admin, quota_bytes, …)
groups(id, name) · group_members(group_id, user_id)
passkeys(id, user_id, name, credential_id, public_key, sign_count, created_at, last_used)
recovery_codes(user_id, code_hash, used_at) · sessions(id, user_id, created_at, last_seen, step_up_at)
devices(id, user_id, name, platform, refresh_token_hash, token_family, last_otp_at, last_seen)
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

content_text(hash, lang, source[plain|pdf|ocr|tika|vision], text, truncated, seq)  -- komprimiert (lz4)
content_vision(hash, model, caption, tags, doc_type, detected_date, raw jsonb)
content_embeddings(hash, chunk_no, model, vec halfvec(1024))
search_vectors_cloud(node_id, chunk_no, model, ancestor_ids, vec halfvec(1024))
      -- HNSW auf binary_quantize(vec), Filter auf ancestor_ids
search_vectors_local(node_id, chunk_no, model, ancestor_ids, vec halfvec(384))
      -- lokales Modell (7.5), Dimension je nach Modell
search_vectors_clip(node_id, model, ancestor_ids, vec halfvec(512))
      -- CLIP-Bildvektoren für „Nur lokal“-Fotos
data_class(node_id, class[cloud|local])                     -- Datenklasse pro Ordner, vererbt
s3_cache(content_hash, bucket_key, size, reason[link|remote], expires_at, last_hit)
      -- Außen-Cache (15.2), nur für Inhalte der Klasse „cloud“
jobs(id, kind, key, priority, state[queued|waiting|failed], attempts, run_after, last_error, …)
ai_usage(day, provider, model, tokens_in, tokens_out, cost)
```

Migrationen mit `sqlx migrate`. Vor jeder Migration erstellt der Server automatisch einen `pg_dump`.

---

## 14. API-Überblick

REST/JSON unter `/api`, OpenAPI-Spezifikation generiert. Auth per Session-Cookie (Web) oder Bearer-Token (Geräte).
Geräte melden sich per Browser-Login mit PKCE an. Pro Gerät gibt es einen widerrufbaren Refresh-Token.

```
Auth      POST /auth/login (Passwort) → POST /auth/otp (TOTP oder Wiederherstellungscode) · POST /auth/otp/setup
          POST /auth/passkey/options → POST /auth/passkey/verify · /me/passkeys (registrieren, umbenennen, löschen)
          POST /auth/step-up · /auth/device/* (Browser-Flow mit PKCE) · GET/DELETE /me/devices · GET /me/sessions
Nodes     GET /nodes/{id} · /nodes/{id}/children · POST /nodes (Ordner) · PATCH /nodes/{id} (rename/move)
          DELETE /nodes/{id} (→ Papierkorb) · POST /nodes/{id}/restore · GET /nodes/{id}/versions
Inhalt    GET /content/{version} (Range; von außen ggf. 307 → signierte S3-URL, 15.2) · GET /thumb/{hash}/{size} · GET /preview/{version}
Upload    POST /uploads · PUT /uploads/{id}/chunks/{hash} · POST /uploads/{id}/commit · POST /uploads/batch
Sync      GET /sync/changes?cursor&roots · GET /sync/snapshot?page · GET /sync/notify (SSE)
Suche     GET /search?q&filter… · GET /search/suggest?q
Start     GET /home · GET /activity · GET /notifications · GET /events/stream (SSE für die Web-UI)
Teilen    /shares · /links · öffentlich: /s/{token}
Admin     /admin/users · /admin/groups · /admin/roots · /admin/ai · /admin/jobs · /admin/health
Metriken  /metrics (Prometheus) · /healthz
```

---

## 15. Betrieb auf der Synology

- **Container Manager → Projekt** mit `docker-compose.yml` (liegt unter `deploy/`). Dienste: `xlrx-server`, `xlrx-worker`, `postgres`,
  `tika`, `embed-local`, `caddy` (eigene IP, siehe 15.3). Images werden per GitHub Actions für amd64 **mit x86-64-v2 als Basis** gebaut (kein AVX, siehe 3.1)
  und in GHCR veröffentlicht.
- Ein eigener DSM-Benutzer `xlrx` (PUID/PGID, nie root) mit Lese-/Schreibrechten nur auf `homes`, die eingebundenen Team-Ordner, `xlrx-state` und `xlrx-db`.
  `/volume1` wird als Ganzes gemountet (4.1). Der Worker mountet es **read-only**.
  CPU-Limits in compose: Worker und `embed-local` zusammen max. 2 Kerne tagsüber.
- **Boot-Aufgabe** (Aufgabenplaner, root):
  `sysctl -w fs.inotify.max_user_watches=1048576 net.core.rmem_max=7500000 net.core.wmem_max=7500000`
  (inotify für viele Verzeichnisse, UDP-Puffer für QUIC).
- **Erreichbarkeit von außen:** eigene Domain, Portweiterleitung **TCP 80/443 + UDP 443** auf den Caddy-Container (15.3). Split-DNS für das LAN.
- **Monitoring:** `/healthz`, Prometheus-Metriken, strukturierte Logs, Admin-Dashboard (Job-Queue, Index-Status, Fehler).
- **Updates:** neues Image ziehen, automatische Migration mit vorherigem Dump, Rollback-Anleitung.
- **Object Storage, herstellerunabhängig:**
  - xlrx nutzt nur den **Kern der S3-API**: PUT/GET mit Range, HEAD, DELETE, Multipart-Upload, ListObjectsV2 und signierte URLs (SigV4).
    Endpunkt, Region, Bucket, Schlüssel und Adressierungsart (Path- oder Virtual-Host-Style) sind frei konfigurierbar.
  - **Bewusst nicht genutzt:** Lifecycle-Regeln, Object Lock, Bucket-Benachrichtigungen, Versionierung und anbieterspezifische Speicherklassen.
    Ablauf und Aufräumen erledigt xlrx selbst. Diese Funktionen unterscheiden sich zwischen Anbietern am stärksten.
  - **Kompatibilitäts-Suite:** läuft in der CI gegen MinIO und nachts gegen die echten Anbieter (Hetzner, Scaleway). Jeder neue Anbieter muss sie bestehen.
  - **Start mit Hetzner Object Storage** (Deutschland/Finnland). Basispaket ~6,50 €/Monat inkl. ~1 TB Speicher und ~1 TB Datenverkehr nach außen,
    danach ~6,50 €/TB Speicher und ~1 €/TB Verkehr. Zum Vergleich Scaleway: ~8 €/TB in einer Zone bzw. ~16 €/TB über mehrere Zonen, Verkehr ~10 €/TB nach 75 GB.
    Die Verfügbarkeit bei Hetzner vor der Bestellung prüfen, 2026 gab es Engpässe.
  - **Anbieterwechsel:**
    - Cache: neuen Bucket eintragen. Der Cache ist wegwerfbar und füllt sich neu.
    - Backup: verschlüsselte Backup-Daten per `rclone` 1:1 kopieren und die Hyper-Backup-Aufgabe neu verknüpfen. Alternativ eine neue Aufgabe parallel aufbauen und die alte nach Ablauf der Aufbewahrung löschen.
  - Die KI-APIs bleiben bei Scaleway. Das sind zwei Anbieter mit jeweils eigenem AVV.

### 15.1 Backup (3-2-1)
| Ebene | Was | Schützt vor |
|---|---|---|
| Btrfs-Snapshots (Snapshot Replication), stündlich/täglich | `homes`, Team-Ordner | Versehentliches Löschen, Ransomware über SMB (Snapshots sind schreibgeschützt, optional unveränderlich) |
| xlrx-Versionen + Papierkorb | jede Datei | Überschreiben, Sync-Fehler |
| **Hyper Backup → S3**, nächtlich, **clientseitig verschlüsselt** (eingerichtet) | `homes`, Team-Ordner; sobald xlrx läuft zusätzlich `xlrx-state/dumps` und `xlrx-state/store/versions` | Ausfall, Diebstahl, Brand des NAS |

- **Verschlüsselung:** Hyper-Backup-Verschlüsselung ist Pflicht. Passwort und Schlüsseldatei liegen offline (Passwortmanager + Papier im Haus).
  Ohne sie gibt es keine Wiederherstellung. Der Anbieter sieht nur verschlüsselte Blöcke.
- **„Nur lokal“-Ordner:** Sie gehen **verschlüsselt nach S3** (geklärt: zulässig mit AVV beim Speicheranbieter und Schlüssel nur bei dir).
  Dafür gibt es eine **eigene Aufgabe mit eigenem Schlüssel in einem eigenen Bucket**. Das ist sauber dokumentierbar
  (AVV, Verzeichnis der Verarbeitungstätigkeiten) und getrennt aufbewahr- und löschbar.
  Der Admin-Bereich zeigt die Pfadliste aller „Nur lokal“-Ordner zum Abgleich mit den Hyper-Backup-Aufgaben.
- **Nicht gesichert:** Suchindex, Vektoren, Thumbnails und `staging`. Sie werden neu aufgebaut. Extrahierte Texte und KI-Ergebnisse liegen in Postgres
  und sind über den Dump gesichert. Ein Neuaufbau kostet also kein zweites Mal API-Geld.
- **Aufbewahrung:** Hyper Backup „Smart Recycle“, z.B. 30 tägliche, 12 wöchentliche, 12 monatliche Stände.
- **Erst-Upload:**
  - Bei **50 Mbit/s** brauchen 3 TB rein rechnerisch ~6 Tage Volllast. Mit Drosselung tagsüber (z.B. 20 Mbit/s) und Vollgas nachts
    sind es **etwa 1,5–2 Wochen**.
  - **Trotzdem jetzt starten** statt auf Januar zu warten, und zwar nach Priorität: zuerst „Nur lokal“-Ordner und Dokumente (klein,
    unersetzlich), dann Fotos, dann der Rest. Lange vor Januar ist alles oben.
  - Bei **400 Mbit/s** ab Januar würde derselbe Erst-Upload **unter einem Tag** dauern. Danach überträgt das Backup nur noch die Änderungen.
- **Hyper Backup mit Hetzner** (S3-kompatibel, eigener Endpunkt) wird in M0 getestet. Falls es hakt, gibt es einen gleichwertigen
  Fallback: **restic** im Container. Es ist ebenfalls verschlüsselt, dedupliziert und arbeitet herstellerunabhängig mit S3.
- **Kosten** für 3 TB bei Hetzner: ~19 €/Monat. Glacier-Klassen sind billiger, passen aber nicht, weil Hyper Backup direkten Lesezugriff braucht.
- **Wiederherstellung üben:** Vierteljährlich einen zufälligen Ordner und den `pg_dump` in eine Testumgebung zurückspielen. Der Admin-Bereich erinnert daran.
- **Status:** Das Backup ist eingerichtet. Offen bleiben eine erste Wiederherstellungsprobe (falls noch nicht gemacht) und die Ergänzung um
  `xlrx-state/dumps` und `store/versions`, sobald xlrx produktiv läuft. Die Freigabe `xlrx-db` (Postgres-Dateien, Index, Thumbnails) bleibt ausgenommen.

### 15.2 Außen-Beschleuniger (S3-Cache)
**Problem:** Jeder Download von außen, über Freigabe-Links oder vom Handy unterwegs, läuft durch den Upload des Heimanschlusses
und durch die schwache NAS-CPU.
- **Heute (50 Mbit/s ≈ 6 MB/s):** Ein 2-GB-Video über einen Link braucht ~6 Minuten, und mehrere gleichzeitige Downloads teilen sich das.
  Der Cache bringt hier am meisten.
- **Ab Januar (400 Mbit/s ≈ 50 MB/s):** Die Leitung reicht meist. Dann wird die NAS-CPU (TLS/QUIC) zum Engpass. Der Cache
  entlastet sie und hilft bei vielen gleichzeitigen Downloads. M3b wird voraussichtlich erst nach Januar fertig, die Priorisierung
  bleibt trotzdem sinnvoll.

**Lösung:** Ausgewählte Inhalte aus **„Cloud erlaubt“-Ordnern** werden in einen privaten S3-Bucket gespiegelt und von dort direkt ausgeliefert.
- **Was wird gespiegelt:**
  1. Dateien hinter **Freigabe-Links**: Das Spiegeln startet beim Anlegen des Links im Hintergrund. Bis es fertig ist, liefert das NAS selbst aus.
  2. **Vorausladen** für Personen, die oft unterwegs sind: neue und geänderte Dateien in ihren Ordnern sowie ihre „Vorgeschlagenen“ Dateien
     von der Startseite (8.2). Das passiert nachts bzw. wenn die Leitung frei ist.
  3. Dateien, die von außen **mehrfach** abgerufen werden, z.B. neue Familienfotos, die mehrere Personen unterwegs öffnen.
- **Auslieferung:** Fragt ein Client von außen eine gespiegelte Datei an, antwortet der Server mit einer Weiterleitung (`307`) auf eine
  **vorab signierte S3-URL**, die nur wenige Minuten gültig ist. Browser und `URLSession` folgen automatisch, Byte-Ranges funktionieren.
  Im LAN wird nie umgeleitet.
- **Aktualität:** Der Cache ist inhaltsadressiert (Hash). Eine neue Version erzeugt ein neues Objekt, das alte wird entfernt.
  Rechteprüfung und Link-Passwort laufen weiterhin auf dem NAS, S3 sieht nur anonyme Hash-Namen.
- **Aufräumen:** Wird ein Link widerrufen oder läuft er ab, wird das Objekt sofort gelöscht. Sonst gilt LRU mit Größenbudget
  (z.B. 200 GB) und maximal 30 Tagen Lebensdauer. Wird ein Ordner auf „Nur lokal“ umgestellt, werden alle seine Objekte sofort gelöscht (7.4).
- **Nie im Cache:** Inhalte aus „Nur lokal“-Ordnern. Die Prüfung erfolgt beim Hochladen, nicht nur beim Einplanen, und ist durch einen Test abgesichert wie bei der KI.
- **Grenzen:** Linkseite, Anmeldung und Rechteprüfung kommen weiter vom NAS. Ist das NAS oder der Heimanschluss offline, funktionieren
  auch gespiegelte Links nicht. Uploads von unterwegs gehen weiter direkt ans NAS; das ist unkritisch, weil der Download des
  Heimanschlusses meist schnell ist.
- **Kosten:** Ein Cache bis ~200 GB und bis 1 TB Abrufe pro Monat passen bei Hetzner ins Basispaket. Teilt er sich das Paket
  mit dem Backup, kommen nur einige Euro pro Monat dazu.
- **Zugangsdaten getrennt:** eigener Bucket und eigener API-Schlüssel nur für den Cache. Hyper Backup hat einen anderen Schlüssel für den Backup-Bucket.

### 15.3 Caddy statt DSM-Reverse-Proxy
Router, DNS und die Übernahme bestehender DSM-Proxy-Regeln erledigst du selbst. xlrx liefert dafür `Caddyfile`, compose-Konfiguration und diese Anleitung.

**Warum eine eigene IP:** Das nginx von DSM belegt auf dem NAS selbst die Ports 80 und 443 (Web Station, DSM-Reverse-Proxy). Es lässt sich
nicht dauerhaft davon lösen, denn manuelle Änderungen überschreibt das nächste DSM-Update. Deshalb bekommt Caddy per **macvlan** eine
**eigene IP im Heimnetz**, z.B. `192.168.1.20`, und lauscht dort ungestört auf TCP 80/443 und UDP 443.

- **Netzwerk:** Der Caddy-Container hängt in zwei Netzen.
  - Im macvlan-Netz (Parent-Interface `eth0`, bzw. `ovs_eth0`, wenn Open vSwitch aktiv ist) hat er seine eigene IP.
  - Im internen Docker-Netz erreicht er `xlrx-server`.

  Vom NAS selbst ist die macvlan-IP ohne Zusatz-Interface nicht erreichbar. Für xlrx spielt das keine Rolle.
- **Zertifikate:** Caddy holt Let's-Encrypt-Zertifikate selbst über die HTTP- oder TLS-ALPN-Challenge. Für einen eigenen LAN-Endpunkt
  (`drive-lan.<domain>`, 5.9) oder ein Wildcard-Zertifikat wird die DNS-Challenge genutzt. Dafür braucht Caddy das Plugin deines DNS-Anbieters.
- **Umzug ohne Ausfall:**
  1. Bestehende Regeln aus dem DSM-Reverse-Proxy (andere Dienste) in die `Caddyfile` übernehmen.
  2. Caddy parallel starten und über die neue IP testen.
  3. Portweiterleitung im Router auf die Caddy-IP umstellen und Split-DNS anpassen.

  Der Rückweg ist jederzeit möglich: Portweiterleitung zurück auf die NAS-IP.
- **Alternative ohne macvlan:** Caddy auf hohen Ports, z.B. 8443/TCP+UDP, und der Router übersetzt 443 → 8443. Das ist einfacher, im LAN aber
  fummeliger, weil Split-DNS dann auf einen anderen Port zeigen müsste. Deshalb nur der Plan B.

---

## 16. Sicherheit

- TLS überall, HSTS, strikte CSP, `SameSite`-Cookies, CSRF-Schutz.
- Anmeldung immer mit zwei Faktoren: Passwort + TOTP oder Passkey, Details siehe 16.1.
- Postgres, Tika und `embed-local` nur im internen Docker-Netz. Der Worker darf nur zum KI-Anbieter. `embed-local` hat gar keinen Internetzugang.
- Jobs aus „Nur lokal“-Ordnern werden vom Worker technisch nie an einen Cloud-Provider geroutet. Die Regel wird beim Versand
  geprüft, nicht nur beim Einplanen. Ein Test stellt sicher, dass kein Request dieser Ordner das Haus verlässt.
- Parser-Isolation im Worker (read-only, Ressourcenlimits, Timeouts pro Datei).
- Nutzerinhalte von einer separaten Origin bzw. als Attachment mit CSP `sandbox` ausliefern.
- S3: private Buckets, getrennte API-Schlüssel für Cache (Server) und Backup (Hyper Backup), signierte URLs mit wenigen Minuten Gültigkeit.
  Objektnamen sind Hashes, sie enthalten keine Datei- oder Pfadnamen.
- Audit-Log für Admin-Aktionen, Freigaben und Link-Zugriffe.
- `cargo audit`/`cargo deny` und `npm audit` in der CI. Security-Review vor dem Öffnen nach außen.


### 16.1 Anmeldung & Konten
Ziel: eigene Konten, die so sicher sind wie bei einem guten Cloud-Dienst. Jede Anmeldung braucht **zwei Faktoren**, wahlweise auf zwei Wegen:

| Weg | Ablauf | Eigenschaften |
|---|---|---|
| **Passwort + TOTP** | Passwort, dann 6-stelliger Code aus einer Authenticator-App | funktioniert überall, auch auf fremden Geräten |
| **Passkey** | ein Klick, entsperrt per Face ID/Touch ID/PIN (z.B. iCloud-Schlüsselbund, YubiKey) | **phishing-resistent**, kein Passwort nötig; zählt als beide Faktoren, weil die Nutzerprüfung (`userVerification = required`) Pflicht ist |

- **Konten:** Es gibt keine Selbstregistrierung. Ein Admin legt Konten an, die Person bekommt einen zeitlich begrenzten Einladungslink.
- **Einrichtung beim ersten Login erzwungen:** mindestens **ein** zweiter Weg, also TOTP oder ein Passkey. Empfohlen werden zwei,
  z.B. Passkey + TOTP oder zwei Passkeys, damit ein verlorenes Gerät nicht aussperrt.
- **TOTP:**
  - RFC 6238 (6 Ziffern, 30 s, ±1 Zeitschritt Toleranz), kompatibel mit jeder Authenticator-App (Aegis, 2FAS, 1Password, Apple Passwörter).
  - **Replay-Schutz:** Der zuletzt benutzte Zeitschritt wird pro Konto gespeichert, derselbe Code gilt nur einmal.
  - Die TOTP-Geheimnisse liegen **verschlüsselt** in Postgres. Der Schlüssel kommt aus einem Docker-Secret, nicht aus der DB und nicht aus dem Backup.
- **Passkeys (WebAuthn):**
  - Umgesetzt mit `webauthn-rs`. Mehrere Passkeys pro Konto mit Namen („iPhone“, „YubiKey“), Zähler-Prüfung gegen geklonte Schlüssel.
  - Die Relying-Party-ID ist die Domain (`<domain>`). Dadurch funktionieren Passkeys auch auf Subdomains wie `drive-lan.<domain>`.
  - Einen neuen Passkey hinzufügen oder einen entfernen ist eine Step-up-Aktion.
- **Wiederherstellungscodes:** 10 Einmalcodes bei der Einrichtung, nur als Hash gespeichert. Ein Admin kann die zweiten Faktoren
  zurücksetzen. Das wird protokolliert und der Person gemeldet.
- **Passwörter:** argon2id, Parameter auf dem J3455 kalibriert (~250 ms pro Prüfung). Mindestens 12 Zeichen. Abgleich gegen eine **lokale**
  Liste häufiger und geleakter Passwörter, ohne Anfrage nach außen. Wer nur Passkeys nutzt, braucht trotzdem ein Passwort für den Notfall mit TOTP bzw. Wiederherstellungscode.
- **Schutz vor Durchprobieren:** Rate-Limit pro IP und pro Konto, exponentielles Backoff und temporäre Sperre mit Benachrichtigung.
  Unbekannte Konten bekommen dieselbe Antwort und Antwortzeit wie falsche Passwörter.
- **Web-Sitzungen:** Cookie mit `HttpOnly`, `Secure` und `SameSite=Strict`. Leerlauf-Timeout ~8 h, maximale Laufzeit ~7 Tage, danach erneute Anmeldung.
  **Step-up:** Sensible Aktionen verlangen erneut einen **OTP oder Passkey**:
  - Passwort, TOTP oder Passkeys ändern
  - öffentlichen Link anlegen
  - Datenklasse ändern
  - Gerät hinzufügen
  - Admin-Aktionen
- **Geräte (Mac/iOS):**
  - Anmeldung über den Browser-Flow (`ASWebAuthenticationSession`, PKCE) mit Passwort + OTP **oder Passkey**. Auf dem Mac und iPhone ist das meist
    ein Touch-ID- bzw. Face-ID-Klick. Danach gibt es ein **gerätegebundenes, rotierendes Refresh-Token** im Schlüsselbund.
    Wird ein altes Token erneut benutzt, widerruft das sofort die ganze Kette.
  - Eine **Passwortänderung** beendet alle anderen Web-Sitzungen und meldet **alle Geräte** ab; sie melden sich danach über den Browser neu an.
  - Zugriffstokens sind kurzlebig (~15 min). Pro Gerät ist alle N Tage eine erneute Bestätigung per OTP oder Passkey nötig (konfigurierbar, z.B. 30).
  - Geräteliste mit Widerruf, Benachrichtigung bei Anmeldung eines neuen Geräts.
  - **Umgesetzt (M1):** Die App öffnet `/device?challenge=…&redirect_uri=xlrx://auth&name=…&platform=…`. Nach Anmeldung (oder frischem
    zweitem Faktor) und „Verbinden“ geht der Browser mit einem Einmalcode (2 min) zurück zur App; die App tauscht ihn mit dem PKCE-Verifier
    gegen ein Tokenpaar (`POST /api/devices/token`). Jede Erneuerung ersetzt beide Tokens. Ausnahme für verlorene Antworten: Solange das neue
    Paar nie benutzt wurde, gilt das vorige Refresh-Token noch einmal. Jede andere Wiederverwendung meldet das Gerät ab und landet im Audit-Log.
    Gerätetokens gelten nur für Dateien und Sync, nie für das Konto selbst oder die Verwaltung. Zurücksetzen der Faktoren und Sperren des Kontos
    melden alle Geräte ab. Frist für die erneute Bestätigung: `XLRX_DEVICE_CONFIRM_DAYS` (Standard 30). Benachrichtigungen folgen mit M3.
- **Freigabe-Links** brauchen kein Konto. Sie haben ein optionales Passwort und eigene Rate-Limits (9.2).

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
6. **CPU-Kompatibilität:** Alle Server-Binaries und Images laufen in der CI einmal unter `qemu-x86_64 -cpu Denverton` (kein AVX).
   Ein *Illegal Instruction* bricht den Build.
7. **Transport:** Tests mit gesperrtem UDP (Fallback auf HTTP/2 ohne Abbruch), Netzwechsel während eines Uploads (QUIC-Migration),
   SSE durch den Reverse Proxy (keine Pufferung, Reconnect mit Cursor).
8. **Datenschutz-Regel:** Integrationstest mit einem Fake-Cloud-Provider und einem Fake-S3. Aus „Nur lokal“-Ordnern darf dort kein
   einziger Request ankommen, weder KI noch Cache. Nach einem Klassenwechsel auf „Nur lokal“ müssen alle Cache-Objekte gelöscht sein.
9. **Anmeldung:** TOTP-Replay, Zeitfenster-Grenzen, Passkey-Registrierung und -Anmeldung (inkl. Zähler-Prüfung und `userVerification`), Sperren und Backoff, Step-up, Token-Rotation und Widerruf der Kette bei Wiederverwendung,
   gleiche Antwortzeiten bei unbekannten Konten.
10. **S3-Kompatibilität:** Suite gegen MinIO in der CI und nachts gegen Hetzner und Scaleway. Geprüft werden Multipart, Range, signierte URLs, Löschen und Listen (15).
11. **Last/Skalierung:** synthetischer Baum mit 3 Mio Dateien. Gemessen werden Listing, Sync, Suche und Erst-Indexierung gegen die Zielwerte.
12. **Dogfooding:** mehrere Wochen produktiver Eigenbetrieb **bevor** Synology Drive abgelöst wird.

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
├─ web/                          Nuxt-App
├─ apple/
│  ├─ Project.yml                (XcodeGen/Tuist)
│  ├─ Packages/XlrxUI/           gemeinsame SwiftUI-Komponenten
│  ├─ macOS/                     App, SyncAgent, FinderSync, FileProvider
│  └─ iOS/                       App, FileProvider, ShareExtension
├─ deploy/
│  ├─ docker-compose.yml
│  ├─ Dockerfile.server · Dockerfile.worker · Dockerfile.embed-local
│  ├─ Caddyfile                  Caddy-Konfiguration (HTTP/3, xlrx + übernommene DSM-Regeln)
│  └─ synology.md                Einrichtungsanleitung (Freigaben, Rechte, Boot-Aufgabe, Router)
├─ spikes/
│  └─ ds918/                     Hardware-Messungen (Embedding, QUIC, Hashing, Reflink, ACL)
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
| **M0** | Fundament + DS918-Spike | Workspace, CI (Rust/Web/Apple, x86-64-v2 + QEMU-Check), Docker-Images, compose, Caddy-Beispiel, Postgres-Schema, **Auth (Passwort + TOTP oder Passkey, Wiederherstellungscodes, Sitzungen, Step-up)**, Admin-Grundgerüst. **Spike auf dem DS918+:** lokale Embedding-Laufzeit ohne AVX (ONNX vs. llama.cpp, e5-small vs. embeddinggemma), QUIC- vs. HTTP/2-Durchsatz, BLAKE3-Geschwindigkeit, **Subvolume-übergreifende Reflinks im `/volume1`-Mount**, Rechte-Durchsetzung für Benutzer `xlrx`, ACL-Vererbung, Temp-Dateien vs. Synology Drive, NOCOW für `xlrx-db`, Hilfsdateien von Synology Drive. `Caddyfile` + compose für den Caddy-Container mit eigener IP (15.3). Router und DNS stellst du selbst um. | `docker compose up` auf der Synology zeigt den Login über die eigene Domain per HTTP/3. Messwerte stehen in `spikes/ds918/`. Anmeldung nur mit Passwort + OTP oder Passkey möglich | M |
| **M1** | Server-Kern + Web-Basis | Speicher-Schnittstelle `ContentStore` mit `PlainFsStore` (4.6), Roots (bestehende Drive-Ordner einbinden), Watcher + Abgleich-Scan, Journal, Upload/Download (Chunks), Versionen (Reflink), Papierkorb, Web: Durchsuchen, Upload, Vorschau, Thumbnails | Web-UI zeigt die echten Daten aus Synology Drive, Änderungen per SMB erscheinen in Sekunden | L |
| **B1** | Sync-Kern in Simulation | xlrx-chunk, xlrx-sync (sans-IO), xlrx-sim, Konfliktregeln, Invarianten | 1 Mio Seeds ohne Verletzung | L |
| **M2** | Volltextsuche I | Tika-Extraktion, Tesseract-OCR, Tantivy (de/en), Filter/Syntax, Snippets, Rechte-Filter, Such-UI | Suche nach Inhalt in PDF/Office/Scans, p95 < 300 ms bei Bestandsgröße | M |
| **M3** | Teilen, Aktivität, Startseite | Ablagen (inkl. Einbinden der Synology-Drive-Team-Ordner), Gruppen, **Datenklassen pro Ordner (7.4)**, Freigaben, Links, Aktivitätsstream, Vorschläge, Benachrichtigungen | Familie nutzt Web-UI für Teilen und findet Dinge über die Startseite | L |
| **M3b** | Außen-Beschleuniger | `S3CacheStore`, Spiegeln von Link-Dateien, Vorausladen für Personen unterwegs, 307 auf signierte URLs, Aufräumen, Datenklassen-Prüfung (15.2) | Ein Link auf ein 2-GB-Video lädt extern mit voller Geschwindigkeit, ohne den Heimanschluss zu belasten. „Nur lokal“-Inhalte nachweislich nie im Bucket | M |
| **M4** | KI-Suche | Provider-Abstraktion (Scaleway/Cloudflare/lokal), Vision-Analyse, Embeddings, **lokales Embedding-Modell + CLIP für „Nur lokal“-Ordner**, drei Vektor-Indizes, hybride Rangfolge, Budget, Kostenschätzung | „Rechnung Heizung 2025“ und „Hund am Strand“ liefern sinnvolle Treffer. Sensible Ordner sind semantisch und nach Bildinhalt durchsuchbar, ohne dass ein Byte das Heimnetz verlässt. Kosten im Rahmen | M–L |
| **M5** | Mac-App (Spiegel-Modus) | xlrx-client + FFI, SyncAgent, Menüleisten-App, Selective Sync, FinderSync, LAN-Direktverbindung | 4 Wochen Dogfooding ohne Datenverlust → **Synology-Drive-Client abschalten** | XL |
| **M6** | iOS-App | Startseite, Suche, Durchsuchen, Vorschau, File Provider (Dateien-App), Share-Extension, Push | Alltagstauglich auf iPhone/iPad, über TestFlight verteilt | L |
| **M7** | Mac File-Provider-Modus | FP-Extension, Offline-Pinning, Moduswechsel pro Ordner | Große Ablagen auf Abruf, stabil im Dogfooding | L |
| **M8** | Härtung & Feinschliff | Lasttest mit 3 Mio Dateien, Security-Review, Backup/Restore-Probe, Dokumentation, Tuning der Vorschläge | Go-live für alle, Synology Drive deinstalliert | M |

Größen: S ≈ Tage, M ≈ 1–3 Wochen, L ≈ 3–6 Wochen, XL ≈ 6+ Wochen fokussierte Arbeit. Das sind grobe Richtwerte.
Am meisten Zeit kostet erfahrungsgemäß die Härtung des Syncs (M5).

**Stand 2026-10-03:**
- **B1 erledigt:** Chunker, Sync-Engine (inkrementell), Simulator; nach einem adversarialen Review 1 Mio. Seeds ohne Befund (ADR 0001).
- **M0 im Code erledigt:** Server mit Konten und Anmeldung (Passwort + TOTP oder Passkey, Wiederherstellungscodes, Step-up, Verwaltung, Audit-Log), Web-App dazu, Docker-Image, compose mit Caddy (macvlan, HTTP/3), CI inkl. Browser-Test und Prüfung ohne AVX.
- **Offen für M0:** Inbetriebnahme auf dem DS918+ (`deploy/synology.md`) und die Messungen aus `spikes/ds918/`.
- **M1 erledigt (lokal getestet, ohne NAS):**
  - Ablagen („Meine Ablage“ = `homes/NAME/Drive`) mit Knoten und lückenlosem Journal.
  - Abgleich-Scan (Identität über die Inode, Schutz vor wiederverwendeten Inodes und leeren/fehlenden Freigaben, „racy“-Zeitstempel) und Überwachung per inotify mit Teil-Abgleich: Änderungen von außen erscheinen in Sekunden.
  - Ändern über die API: Ordner, Hochladen, Ersetzen mit Versionen (inhaltsadressiert, Reflink), Umbenennen/Verschieben, Papierkorb – jeweils mit Absichtsprotokoll, sodass ein Absturz an jeder Stelle nichts verliert (in Tests für jede Stelle nachgestellt).
  - Änderungs-Feed im Format der Sync-Engine und Live-Ereignisse (SSE); Schreib-Operationen der Sync-Engine über das API (Vorbedingungen, idempotente Op-IDs).
  - Anmeldung von Geräten (Mac/iPhone) über den Browser mit PKCE, rotierende Refresh-Tokens mit Erkennung von Wiederverwendung, Geräteliste mit Abmelden (16.1).
  - Uploads in Teilen (≤ 8 MiB), wiederaufnehmbar, jeder Teil geprüft und vor der Bestätigung auf der Platte (5.3).
  - Vorschaubilder für Fotos und Bilder (JPEG, PNG, GIF, WebP, BMP, TIFF) beim ersten Abruf, aufrecht gedreht, abgelegt unter dem Hash genau der
    gelesenen Bytes (nie unter dem Hash aus der Datenbank, damit nie ein fremdes Bild erscheint); höchstens zwei gleichzeitig, Schutz vor
    „Dekompressionsbomben“. PDF-, HEIC- und Video-Vorschauen kommen mit dem Worker (M2).
  - Web-App mit Durchsuchen, Vorschau, Hochladen, Versionen, Papierkorb und Live-Aktualisierung; Startseite mit Begrüßung und Berglandschaft nach Tageszeit aus dem App-Entwurf.
- **M1 im Code fertig.** Offen bleibt der Nachweis auf dem NAS (echte Daten aus Synology Drive, Änderungen per SMB).
- **M2 im Code erledigt (lokal getestet, ohne NAS):**
  - Suchindex (Tantivy) im Zustandsverzeichnis, gespeist aus dem Journal: Umbenennen, Verschieben, Löschen und Wiederherstellen kommen sofort an;
    beim Verschieben eines Ordners in einen anderen wird der ganze Teilbaum neu eingetragen (das Journal merkt sich, ob sich der Elternordner geändert hat).
    Der Index speichert seinen Stand in den eigenen Commits, setzt nach einem Neustart dort fort und baut sich aus der Datenbank neu auf, wenn er fehlt oder kaputt ist.
  - Deutsch und Englisch mit Wortstamm („Rechnungen“ findet „Rechnung“), Umlaute und ß gefaltet, Wortanfänge in Namen beim Tippen, ähnliche Schreibweisen,
    wenn nichts genau passt. Syntax: Wörter, „Phrasen“, `-ausschluss`, `typ:`, `in:`, `nach:`, `vor:`; Treffer je Dateityp; Textausschnitte mit Markierung.
  - Rechte doppelt geprüft: der Index sieht nur lesbare Ablagen, jeder Treffer wird in Postgres noch einmal geprüft. Eine Suche wartet kurz, bis eigene Änderungen im Index sind.
  - Textextraktion auf dem NAS (siehe 6.6) mit Fortschritt in der Verwaltung.
  - Web: Suche mit Vorschlägen (Dateinamen, Suchfilter, zuletzt gesucht – nur im Browser gespeichert), Ergebnisse mit Bildern als Kacheln und Textausschnitten, Suche in einem Ordner.
  - Offen für später: `von:`, `ist:`, `dokument:`, `ort:` und die Facetten Besitzer und Ort (M3/M4); Messung p95 mit echtem Bestand auf dem DS918+.
- **M3 erledigt (lokal getestet) – Teilen, Datenklassen, öffentliche Links, Aktivität, Vorschläge, Glocke:**
  - Eine Rechteprüfung für alles (Durchsuchen, Inhalte, Vorschaubilder, Versionen, Uploads, Änderungen, Papierkorb, Sync, Suche, Live-Ereignisse).
    Rolle = höchste aus Besitz der eigenen Ablage, Mitgliedschaft in einer Geteilten Ablage, Freigabe auf dem Element oder einem Ordner darüber
    (an die Person oder eine ihrer Gruppen, nicht abgelaufen). Ohne Rolle gibt es das Element nicht – auch für Admins.
  - Rollen Ansehen, Bearbeiten (innen ändern), Verwalten (weiter teilen), optional mit Ablaufdatum. Umbenennen, Verschieben, Löschen verlangen
    Bearbeiten-Rechte am Ordner darüber: Der geteilte Ordner selbst bleibt beim Besitzer. Wer nur eine Freigabe hat, kann in den Papierkorb legen
    und zurückholen, aber nie endgültig löschen.
  - Wer nur eine Freigabe hat, sieht ab dem geteilten Ordner: Pfade, Suchtreffer, Zugriffsliste und Live-Ereignisse verraten nichts darüber.
    Die Suche filtert über die Vorfahren im Index; Teilen und Entziehen brauchen keine Neu-Indexierung.
  - Gruppen und Geteilte Ablagen (bestehende Ordner des NAS, z. B. Synology-Drive-Teamordner, mit Mitgliedern) in der Verwaltung;
    der Pfad muss ein echter Ordner unter dem Datenverzeichnis sein und darf keine andere Ablage überschneiden. Alles im Audit-Log.
  - Web: Bereich „Geteilt“, „Teilen“ in den Aktionen, Tab „Zugriff“, „geteilt mit …“ im Ordner; wer nur ansehen darf, sieht keine Änderungs-Aktionen.
  - Noch nicht: Verknüpfung geteilter Elemente in „Meine Ablage“ für den Mac-Client (kommt mit M5).
  - Datenklassen (7.4): jeder Ordner „Nur lokal“ oder „Cloud erlaubt“, vererbt bis zur nächsten eigenen Einstellung; auch für eine ganze
    Ablage. Ohne Einstellung gilt `XLRX_DEFAULT_DATA_CLASS` (Standard „Nur lokal“). Ändern nur mit Verwalten-Recht, erneuter Bestätigung
    und Eintrag im Audit-Log (vorher, nachher). Wer nur eine Freigabe hat, erfährt nicht, aus welchem Ordner darüber die Einstellung kommt.
    `data_class::allows_cloud` ist die eine Prüfung, die jeder spätere Cloud-Weg (M3b, M4, Backup) vor dem Versand aufrufen muss.
  - Web: Datenklasse im Ordner und in den Aktionen, Kennzeichnung in der Liste, „Cloud-Analyse“ in den Details einer Datei,
    Verzeichnis aller Einstellungen in der Verwaltung (als Tabelle zu sichern).
  - Öffentliche Links (9.2): Ansehen, Herunterladen, Nur hochladen (Dateianfrage, sieht nichts vom Ordner), Bearbeiten (herunterladen,
    Dateien hinzufügen, neue Fassungen – die alten bleiben als Versionen). Kein Link kann löschen, umbenennen, verschieben oder überschreiben:
    ein belegter Name bekommt einen freien. Token mit 128 Bit; in der DB nur sein SHA-256 und das Token mit dem Serverschlüssel versiegelt
    (zum erneuten Kopieren). Optional Passwort (argon2id; nach 5 Fehlversuchen Sperre je Link, nach 20 je Adresse; Freischaltung als
    Cookie nur für diesen Link, an das aktuelle Passwort gebunden), Ablaufdatum, Höchstzahl Downloads (gezählt werden begonnene Downloads).
    Anlegen nur mit Verwalten-Recht und erneuter Bestätigung; Beenden sofort. Ein Link gilt nie mehr als die Rechte seines Erstellers:
    verliert der sie, ist gesperrt oder liegt das Element im Papierkorb, geht der Link nicht mehr. Unbekannte Tokens zählen als
    Fehlversuch der Adresse; dazu höchstens 600 Anfragen je Adresse und Minute. Inhalte mit `Content-Disposition` und CSP `sandbox` wie
    bisher. Uploads über Links höchstens `XLRX_LINK_UPLOAD_MAX_MB` (Standard 10 240). Audit-Log: angelegt, beendet, entsperrt,
    Fehlversuch, heruntergeladen, hochgeladen, ersetzt.
  - Web: Abschnitt „Link“ im Teilen-Dialog (Art, Passwort, Ablauf, Downloads; Kopieren, Beenden) und die Linkseite `/s/…` ohne Konto:
    Passwort, Ordner mit Vorschaubildern, Vorschau, Herunterladen, Hochladen per Ziehen oder Auswahl.
  - Noch nicht: Ordner als ZIP herunterladen.
  - Aktivität (8.3): direkt aus dem Journal (wer, was, wann; „auf dem NAS“ für Änderungen, die ein Abgleich findet), dazu `events` für
    Teilen und Links. Gruppiert nach Aktion, Person, Ordner und ~30 Minuten („Du hast 4 Fotos hinzugefügt“ mit Bilderleiste). Was das
    Löschen oder Wiederherstellen eines Ordners mitnimmt (gleiche Transaktion), erscheint als der Ordner allein; der erste Import einer
    Ablage ist keine Neuigkeit; Uploads über einen Link erscheinen als solche, nicht als Upload des Link-Erstellers. Umbenennen nennt den
    alten Namen. Jede Person sieht nur, was zu Elementen geschah, die sie jetzt sehen darf (gleiche Rechteprüfung), die Nutzung von Links
    nur, wer das Element verwaltet; Ordner darüber bleiben verborgen.
  - Web: Seite „Aktivität“ (Alle / Von anderen / Freigaben, ältere nachladen), Abschnitt auf der Startseite, Tab „Aktivität“ einer Datei.
  - Startseite „Vorgeschlagen“ (8.2): `access_events` (Öffnen im Web, Downloads; Geräte melden später auch lokale Öffnungen, bis 30 Tage
    rückwirkend), privat und je 10 Minuten nur einmal. Punkte aus Frecency (Halbwertszeit 7 Tage), Wochenmuster (gleicher Wochentag ±2 h
    in mindestens 2 früheren Wochen, Zeitzone `XLRX_TIMEZONE`), Ko-Nutzung mit dem zuletzt Geöffneten, Änderungen anderer (oder auf dem
    NAS) seit dem eigenen letzten Öffnen, neu Freigegebenes; feste Gewichte, die Begründung ist der stärkste Anteil („Anna hat das vor
    2 Std. geändert“, „Öffnest du meist montags“). Nur was noch da ist und gesehen werden darf. Angezeigte und geöffnete Vorschläge
    werden protokolliert (`suggestion_log`), um die Gewichte später anzupassen.
  - Glocke (8.4): Benachrichtigung, wenn jemand etwas mit dir (oder deiner Gruppe) teilt und wenn Dateien über deine Dateianfrage
    ankommen (je Link und Stunde zu einer Meldung gesammelt). Live über den bestehenden Ereignisstrom (`event: notification` mit der
    Zahl ungelesener), auch in anderen Fenstern; Öffnen der Liste markiert gelesen. Nur Meldungen zu Elementen, die man noch sehen darf.
  - Bilder, deren Vorschaubild nicht geht (beschädigt), zeigen überall das Dateisymbol statt eines kaputten Bildes.
  - Noch nicht: E-Mail bei neuen Freigaben (SMTP, einstellbar je Person) und Push auf iOS/Mac (APNs, mit den Apps ab M5).

**Nächster Schritt:** Inbetriebnahme auf dem DS918+ mit den Spike-Messungen (M0) und dem M1-Nachweis mit den echten Daten.

---

## 20. Risiken

| Risiko | Gegenmaßnahme |
|---|---|
| Datenverlust durch Sync-Fehler | sans-IO + Simulation, Invarianten, Massenlösch-Schutz, Server-Versionen und Papierkorb, Btrfs-Snapshots, Synology Drive bis M5 als Rückfallebene |
| Eigenheiten der File-Provider-API | Spiegel-Modus ist der Standard, FP kommt erst in M7. Beide nutzen denselben Kern. |
| Datenschutz bei Cloud-KI / S3 | Datenklasse pro Ordner gilt für alle Cloud-Wege, Prüfung beim Versand, Tests mit Fake-Provider, Datenminimierung, AVV je Anbieter, „Nur lokal“ strikt im Heimnetz |
| Backup existiert, Wiederherstellung klappt aber nicht | clientseitige Verschlüsselung mit offline verwahrtem Schlüssel, vierteljährliche Wiederherstellungsprobe, Erinnerung im Admin-Bereich |
| Cloud-Umzug später nötig (DS918+ altert) | Speicher-Schnittstelle ab M1 (4.6), `S3PackStore` nur bei Bedarf |
| KI-Kosten laufen davon | Kostenschätzung vor Start, Batch-API, Dedup per Hash, Budget-Limit mit Auto-Pause |
| NAS-Ressourcen (J3455, 4 schwache Kerne) | schwere Arbeit in der Cloud, CPU-Limits für Worker/embed-local, Nachtfenster, RAM-sparende Vektor-Quantisierung, SSD-Cache |
| J3455 ohne AVX → ML-Laufzeiten stürzen ab | x86-64-v2-Builds, QEMU-Check in der CI, Laufzeit-Auswahl per Spike in M0 |
| Lokale Embeddings zu langsam | kleines Modell (e5-small), nur erste Chunks embedden, Nachtfenster. Bei Bedarf KI-Rechner im Heimnetz (7.6) |
| UDP 443 blockiert / QUIC auf dem J3455 zu CPU-hungrig | automatischer Fallback auf HTTP/2, eigener LAN-Endpunkt nur mit HTTP/2, Messung im Spike |
| inotify-Limits / alter Kernel | Limit anheben, Abgleich-Scans als Sicherheitsnetz |
| Externe Änderungen kollidieren mit Uploads | Intent-Log, Prüfung vor dem Ersetzen, Konfliktkopie statt Überschreiben |
| Breiter Mount von `/volume1` | Container nie als root, DSM-Rechte nur auf die nötigen Freigaben, Nachweis im Spike, Worker nur read-only |
| Kontoübernahme | zwei Faktoren Pflicht (Passwort + TOTP oder Passkey), Replay-Schutz, Rate-Limits, Step-up, rotierende Gerätetokens, Benachrichtigungen (16.1) |
| Proxy-Umzug stört andere Dienste | Regeln vorher in die Caddyfile übernehmen, paralleler Test über eigene IP, Umschalten per Portweiterleitung, Rückweg jederzeit |
| Bindung an einen Speicher-Anbieter | nur Kern-API von S3, Kompatibilitäts-Suite, Wechsel = Konfiguration bzw. `rclone`-Kopie (15) |
| Projektumfang | strikte Meilensteine, Nicht-Ziele für v1 (siehe 1), früher Nutzen durch Strang A |

---

## 21. Offene Fragen

Alle Grundsatzfragen sind geklärt (Stand 2026-10-02):

| Thema | Entscheidung |
|---|---|
| Hardware | DS918+ mit SSD-Cache, 16–20 GB RAM |
| Bestandsdaten | `homes/<user>/Drive` → Meine Ablage; Synology-Drive-Team-Ordner → Geteilte Ablagen |
| Zugriff | eigene Domain; Caddy-Container mit eigener IP ersetzt den DSM-Proxy |
| Bandbreite | Upload 50 Mbit/s, ab Januar 2027 400 Mbit/s |
| Apple | Developer Account vorhanden |
| Sensible Ordner | rechtlich begründet „Nur lokal“, pro Ordner frei konfigurierbar; Backup verschlüsselt nach S3 zulässig |
| Cloud-Rolle | NAS bleibt Zentrale; S3 herstellerunabhängig (Start Hetzner) für Backup und Außen-Beschleuniger |
| KI | Scaleway Generative APIs für „Cloud erlaubt“ |
| Anmeldung | eigene Konten, Passwort + TOTP **oder** Passkey |
| Plattformen | Web, macOS, iOS; Windows/Android vorerst nicht |
| Office | nur Vorschau, keine Microsoft-Office-Web-Integration |
| Backup | Hyper Backup → S3 eingerichtet |

**In eigener Verantwortung:** Portweiterleitung im Router (TCP 80/443, UDP 443 auf die Caddy-IP), DNS und Split-DNS, Übernahme
bestehender DSM-Proxy-Regeln in die `Caddyfile`.

Weitere Fragen klären sich in den Meilensteinen, vor allem durch die Messwerte des M0-Spikes.
