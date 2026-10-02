# DS918+-Spike (M0)

Der Plan stützt sich an einigen Stellen auf Annahmen über den DS918+ (Celeron J3455, kein AVX, Kernel 4.4, Btrfs).
Diese Skripte prüfen sie auf dem echten Gerät. Sie ändern keine Nutzerdaten und räumen ihre Testdateien wieder weg.

| Skript | Wo | Prüft |
|---|---|---|
| `run.sh [Ordner]` | NAS, `sudo` | CPU-Merkmale, Reflink **zwischen Subvolumes** im gemeinsamen `/volume1`-Mount (und Gegenprobe), Rechte-Durchsetzung für `xlrx` im Container, ACL-Vererbung neuer Dateien, NOCOW auf `xlrx-db`, Hilfsdateien von Synology Drive, argon2-Dauer, optional Scan-/Hash-Geschwindigkeit auf echten Daten |
| `transfer.sh HOST` | Mac | HTTP/3 gegen HTTP/2: Durchsatz und Antwortzeit, im LAN und von unterwegs |
| `embedding.sh` | NAS, `sudo` | Lokale Embeddings ohne AVX: ONNX Runtime (e5-small) gegen llama.cpp (EmbeddingGemma) |

Voraussetzungen: Schritte 1–3 aus `deploy/synology.md` (Benutzer `xlrx`, Freigaben, Image).

```sh
sudo sh spikes/ds918/run.sh /volume1/homes/klaus/Drive/Dokumente
sudo sh spikes/ds918/embedding.sh
```

Die Ergebnisse landen als `results-*.md` in diesem Ordner. Daraus folgen die Entscheidungen für M1 und M4:

- Reflink über Subvolumes: zentraler Versionsspeicher in `xlrx-state`, sonst Versionen pro Freigabe.
- ACL-Vererbung: ob der Server Rechte nach dem Schreiben explizit setzen muss.
- argon2-Parameter für `deploy/.env`.
- Embedding-Laufzeit und Modell für „Nur lokal“-Ordner.
- Ignorierliste für Hilfsdateien.
