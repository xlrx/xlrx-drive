# ADR 0002 – Mac-Client im Spiegel-Modus: Rust-Kern mit sans-IO-Treiber, dünne Apple-Hülle

Status: angenommen (bei offenen Entscheidungen des Eigentümers gilt bis dahin der Vorschlag, siehe Offene Punkte) · Stand: 2026-10-04 · Meilenstein: M5 (PLAN 19) · baut auf ADR 0001 auf · Code (neu): `crates/xlrx-wire`, `crates/xlrx-driver`, `crates/xlrx-fs`, `crates/xlrx-client`, `crates/xlrx-cli`, `crates/xlrx-ffi`, `crates/xlrx-e2e`, `crates/xlrx-testkit`, `apple/`

## Ziel

Der Mac-Client ersetzt den Synology-Drive-Client. Das Abnahmekriterium steht in PLAN 19: „4 Wochen Dogfooding ohne Datenverlust“. Dafür muss der Client drei Dinge leisten:

1. **Die Engine aus ADR 0001 sicher betreiben.** Dazu gehören dauerhafter Zustand, Idempotenz, der Ausführungsvertrag (ADR 0001 §4.1) und der Scan-Vertrag (ADR 0001 §5). Die Garantien aus 1 Mio. Simulationsläufen dürfen dabei nicht verloren gehen.
2. **Die Lücken schließen, die die Engine bewusst dem Client überlässt.**
   - Persistenz.
   - Schutz vor Massenlöschungen und vor massenhaftem Ersetzen.
   - Objekte, die nicht synchronisiert werden können: symbolische Links, harte Links, Mount-Punkte, unlesbare Einträge, Namen, die der Server nicht halten kann (kein UTF-8, nach NFC länger als 255 Bytes).
   - Schutz der Wurzel.
   - Eindeutige OpIds, auch bei Neuinstallation, Klon oder Wiederherstellung.
   - Verhalten nach einer Rücksetzung der Server-Datenbank.
   - Selective Sync und Ignorier-Regeln.
3. **Fast vollständig ohne Mac und ohne NAS gebaut und geprüft werden können.**
   - Hier steht ein Linux-Container zur Verfügung: Rust 1.97, PostgreSQL, Swift 6.4 nur mit Foundation.
   - Apple-Code und APFS prüfen GitHub-Runner `macos-*`.
   - Ein echter Mac wird nur für die Apple-Frameworks gebraucht, das NAS nur für die Schlussabnahme.

**Nicht Teil von M5:**
- File-Provider-Modus (M7) und iOS (M6).
- Delta-Übertragung auf Chunk-Ebene (PLAN 5.3; siehe Offene Punkte).
- Verknüpfen einzeln geteilter Elemente in „Meine Ablage“ (PLAN 9.1).
- APNs-Push auf dem Mac (PLAN 8.4).
- Pause im Akku- oder Stromsparbetrieb und Bandbreiten-Zeitpläne (PLAN 5.8).
- Pakete wie `.rtfd` als Einheit (PLAN 11.2).
- Transport-Tests mit gesperrtem UDP und SSE durch Caddy (PLAN 17.7).

Delta-Übertragung, geteilte Elemente und APNs sieht PLAN für M5 vor (5.3, 9.1, 19). PLAN wird im Commit dieses ADR angepasst. Alle Abweichungen von PLAN stehen unter „Offene Punkte“.

## Entscheidung

### 1. Leitplanken

1. **Ein Prozess besitzt alles, was Zustand hat.** Der Sync-Agent (LaunchAgent) hält den Rust-Kern, die SQLite-Dateien, die Tokens und die Dateisystem-Ausführung.
   - App und FinderSync sprechen nur XPC und linken kein Rust.
   - Es gibt also genau einen Token-Erneuerer, was die Wiederverwendungs-Erkennung des Servers verlangt (`auth/device.rs:375-392`).
   - Es gibt genau einen Schreiber.
   - **Das wird erzwungen.** `xlrx_client::Agent::open` (gemeinsam für CLI und FFI) nimmt für seine ganze Lebensdauer `flock(LOCK_EX|LOCK_NB)` auf `<Ablage>/agent.lock` (Zustandsverzeichnis, §6). Ist die Sperre vergeben, scheitert `open` mit `AgentError::AlreadyRunning`.
   - Jeder `RootActor` sperrt zusätzlich `<root>/.xlrx-client/lock`. Ist diese Sperre belegt, pausiert der Ordner mit einem Problem.
   - CLI: `xlrx run` ist der Agent. Bis M5.6 öffnen die übrigen Befehle selbst einen Agent (gleiche Sperre) oder brechen mit einem Hinweis ab. Ab M5.6 (`xlrx run` als Dienst) sprechen sie über `$XDG_RUNTIME_DIR/xlrx-drive/agent.sock` (Verzeichnis 0700) mit dem laufenden Agent. `status` darf `state.sqlite` nur lesend öffnen.
   - Der `TokenManager` erneuert nur unter dieser Sperre. Vor jeder Erneuerung liest er den Refresh-Token neu aus dem `SecretStore`.
2. **Logik in Rust, Swift nur als Adapter.**
   - Swift liefert: HTTP über URLSession, Keychain, FSEvents (Ereignisse und `flush`), Thread-QoS und die Oberfläche.
   - In Rust und damit auf Linux testbar liegen: Redirect-Politik, Anmeldung, SSE-Parser, Namensregeln, Hashing und alle Vorbedingungsprüfungen.
3. **Zwei sans-IO-Zustandsautomaten.**
   - Die Engine (`xlrx-sync`) entscheidet, *was* geschieht.
   - Der neue Ordner-Treiber `FolderSync` (`xlrx-driver`) entscheidet, *wann*: Abruf, Scan, Planung, Commit-Barriere, Wiederholungen, Massenlösch-Schutz, Pausen.
   - Zeit ist für beide eine Eingabe. Der Simulator betreibt den echten Treiber.
4. **„Fehlt im Scan“ heißt „ist wirklich weg“.**
   - Der Scanner lässt außerhalb von `CLIENT_DIR` kein Objekt weg, das verknüpft ist oder sein könnte. `CLIENT_DIR` fehlt samt Inhalt im Scan: Was der Nutzer dorthin verschiebt, gilt als gelöscht (§7.2). Sonst werden nur unverknüpfte Junk-Dateien weggelassen. Junk-Dateien sind Dateien mit Namen aus `ignored()` außer `.xlrx-dl-*`, deren `LocalId` in S nicht verknüpft ist.
   - Nicht synchronisierbare Objekte meldet er als *opak*. Ordner mit ignoriertem Namen und lesbare opake Ordner durchläuft er, und alle Nachkommen meldet er ebenfalls als opak (§7.3).
   - Nicht durchlaufbare Ordner stehen in `blind`. Solange `blind` nicht leer ist, hält der Treiber jede Server-Löschung an.
   - Ein voller Scan wird nur geliefert, wenn seine Nachprüfung stabil war oder seine fehlenden Identitäten schon im vorigen vollen Scan fehlten. Nach drei instabilen Scans wird geliefert, aber der `DeleteGuard` hält alle daraus folgenden Server-Löschungen an (§7.4).
   - Restrisiko: eine Verschiebung im letzten, als stabil gewerteten Durchgang, die weder ein Ereignis noch einen geänderten Ordner-Stempel hinterlässt (Risiken).
   - Teil-Rescans melden nie Entfernungen. Eine verschwundene Identität führt zu einem vollständigen Scan.
   - Grund: `plan_local_gone` plant eine Server-Löschung, sobald ein verknüpftes Objekt in L fehlt (`engine.rs:1391-1454`). Eine Server-Löschung kostet mehr als Durchsatz: Versionen, Freigaben, Links und KI-Daten hängen an der `NodeId`.
5. **Commit vor Wirkung, Fail-Stop bei Commit-Fehler.**
   - Keine Operation aus `plan()` läuft, bevor die Transaktion mit ihrer Outbox-Zeile, `local_temps` und `next_op` dauerhaft ist.
   - Schlägt ein Commit fehl, wird die Engine im Speicher verworfen und aus SQLite neu geladen.
6. **Nachweis vor jeder unumkehrbaren lokalen Aktion.**
   - Vor einem Tausch und vor dem Verschieben in den Papierkorb wird ein Intent mit Inode-Nachweis synchron festgeschrieben.
   - Gelöscht wird nur, was nachweislich vom Client selbst stammt.
7. **Im Zweifel anhalten oder neu aufbauen, nie raten.**
   - Ein frischer Zustand mit leerem S plant keine Löschung. Ohne S laufen nur `plan_remote_new` und `plan_local_new` (`engine.rs:1717-1879`). Sie legen an, laden herunter, verknüpfen, erzeugen Konfliktkopien und benennen liegengebliebene Ausweichnamen zurück (`cleanup_temp_name`, `:1816-1821`).
   - Ein Neuaufbau ist trotzdem nicht kostenlos, denn er ist ein Abgleich mit leerem S. Noch nicht übertragene Löschungen kommen zurück. Noch nicht übertragene Verschiebungen werden zu Kopien an beiden Orten. Ab der Wächter-Schwelle fragt die Oberfläche vorher (§6).
   - Neu aufgebaut wird deshalb nur in drei Fällen: bei nachgewiesener Korruption, bei 409 `cursor_invalid` (Server-Rücksetzung) und bei abweichendem Op-Body (409 `op_mismatch`, etwa nach Klon oder zurückgespieltem Zustand, §9). Transiente Fehler pausieren den Ordner mit Backoff.
8. **Was hier prüfbar ist, wird zuerst hier gebaut und geprüft.** Die ersten Teilschritte sind vollständig auf Linux baubar und testbar.

### 2. Crates und Abhängigkeiten

```
xlrx-proto    (+ name::{ignored, valid_name mit NameRefused, syncable, valid_sync_name},
                 Präfix-Konstanten, CLIENT_DIR, Name::local_fold_key, ContentHash::from_hex)
xlrx-chunk    → proto (unverändert)
xlrx-sync     → proto  (+ StateDelta, decline, LocalEntry.opaque, local_fold_key im lokalen Baum,
                          delta::local_changes, später Filter; − Config::max_unconfirmed_deletes)
xlrx-wire     → proto, sync    DTOs der Sync-, Upload- und Token-API; nur der Client nutzt sie
xlrx-driver   → proto, sync    rein: FolderSync, Store-Trait, DeleteGuard
xlrx-fs       → rustix         nur rustix; Linux- und Apple-Zweige, keine C-Abhängigkeit
xlrx-client   → driver, fs, wire, chunk
                 rusqlite =0.39.0 [bundled], crossbeam-channel, rayon, unicode-normalization, serde_json, sha2,
                 base64, rand, url, time, tracing, thiserror;
                 Linux: notify 8 (default-features = false → nur inotify); Feature `ureq`: ureq 3
xlrx-cli      → client         Binärdatei `xlrx`
xlrx-ffi      → client         uniffi =0.32.2; cdylib + staticlib; Binärdatei uniffi-bindgen-swift
                                 ──► XlrxFFI.xcframework ──► apple/
xlrx-sim      → proto, sync, driver (Treiber-Modus, Orakel)
xlrx-server   → proto, chunk, sync (wie heute); Dev-Abhängigkeit: xlrx-testkit
xlrx-testkit  → server         nur als Dev-Abhängigkeit eingebunden
xlrx-e2e      → server, testkit, client[ureq], tokio (nur Tests)
```

| Crate | Aufgabe | Begründung der Grenze |
|---|---|---|
| `xlrx-wire` (neu) | Gemeinsame DTOs: `ChangesQuery{…, check}`, `Changes{changes, cursor, more, cursor_tag}`, `Change`, SSE-Nutzlast `Changed{root, seq}`, Sync-`ContentQuery{size}`, `Available`, `OpRequest`, `OpLookup{result, op}`, `OpResultQuery{with_op}`, `UploadTarget`, `CreateUpload`, `UploadInfo`, `PartQuery`, `CommitUpload`, Commit-Antwort `ContentCommitted{hash}`, `TokenRequest`, `Tokens` (aus `auth/device.rs`), `RootInfo` (aus `api/files.rs`, mit eigenem `Role`), `ErrorBody{error, reason}` (Form aus `error.rs`). Abhängigkeiten: `time` mit `serde`, `formatting` und `parsing`; `uuid` mit `serde`. | Der Server behält seine eigenen Structs. Golden-JSON hält beide Seiten gleich: Es wird aus den Server-Structs erzeugt (Server-Test `wire_formen`), und `xlrx-wire` prüft es im Rundlauf (`crates/xlrx-wire/tests/golden.rs`). HTTP-Formen gehören nicht in die sans-IO-Engine. |
| `xlrx-driver` (neu) | `FolderSync`, `Store`, `MemoryRows`, `DeleteGuard`, Zeitsteuerung | Rein (proto, sync, serde), damit `xlrx-sim` die Produktionslogik unter seinem Fehlermodell betreibt |
| `xlrx-fs` (neu) | Dünne, typisierte Hülle um rustix-Systemaufrufe (§7.1) | Nur reines Rust, ohne Crates aus dem Workspace. Der Apple-Zweig ist hier per `cargo check --target aarch64-apple-darwin` prüfbar (das Target ist installiert). C-Crates wie blake3 oder das gebündelte SQLite lassen sich nicht kreuzübersetzen. |
| `xlrx-client` (neu) | Speicher, Scanner und Ausführung, Watcher, Netz, Anmeldung, Agent-Laufzeit mit Prozess-Sperre (§1) | Plattformneutraler Kern für CLI und FFI |
| `xlrx-cli` (neu, Binärdatei `xlrx`) | `login`, `roots`, `add`, `sync --once`, `run`, `status`, `decide`, ab M5.10 `exclude`/`include` (clap 4.6, schon im Lock) | Dogfooding auf Linux ohne Mac. Treiber für Tests auf macOS-CI. `run` ist der Agent, die übrigen Befehle nutzen dieselbe Sperre bzw. ab M5.6 seinen Socket (§1). |
| `xlrx-ffi` (neu) | UniFFI-Fassade ohne eigene Logik | Erbt `[lints] workspace = true`. Erzeugter Code verletzt `forbid(unsafe_code)` nicht (geprüft). |
| `xlrx-e2e` (neu) | Echter Server über TCP plus mehrere echte Clients | Läuft im CI-Job `rust` auf Linux. Eine Teilmenge läuft nächtlich im Job `apple-e2e` auf APFS (§14). Dafür baut der Server ab M5.8 auch auf macOS (S9). Bekannte Hindernisse heute: `ioctl_ficlone` wird ohne cfg-Schutz aufgerufen (`files/store.rs:49`). `notify` 8.2.0 importiert ohne Feature `macos_kqueue` auf macOS `fsevent_sys` (`notify-8.2.0/src/lib.rs:204-205`, `fsevent.rs:21`), und der Server bindet es heute mit `default-features = false` ein. |
| `xlrx-testkit` (neu, ab M5.3a) | Test-Gerüst des Servers aus `tests/common`: `TestDb`, `Env`, `Client`, Browser-Hälfte der Geräte-Anmeldung, `FakeS3` | Eigenes Crate, nur unter `[dev-dependencies]` von `xlrx-server` und `xlrx-e2e`. Der Zyklus läuft nur über Dev-Abhängigkeiten. Es gibt kein Feature und keine Selbst-Dev-Abhängigkeit, denn ein Feature gelangte bei jedem Bau mit `--examples` in die Release-Binärdatei des Servers (`deploy/Dockerfile.server:21`). |

Weitere Festlegungen:
- **rusqlite** wird auf `=0.39.0` mit `bundled` gepinnt. Version 0.40 scheitert in diesem Workspace am `links = "sqlite3"`-Konflikt mit `sqlx-sqlite 0.9.0`, das `libsqlite3-sys <0.38` pinnt (`Cargo.lock:2386`, `:4024`). 0.39.0 nutzt das bereits gesperrte `libsqlite3-sys 0.37.0`.
- **uniffi** wird auf `=0.32.2` gepinnt.
- Der FSEvents-Zweig von **notify** wird nicht verwendet (§7.5). Der Server nutzt auf macOS das Feature `macos_kqueue` (S9).
- **PLAN 18** wird um `xlrx-wire`, `xlrx-driver`, `xlrx-fs`, `xlrx-cli`, `xlrx-e2e` und `xlrx-testkit` ergänzt. `xlrx-client` und `xlrx-ffi` stehen dort schon.

### 3. Sicherheitsinvarianten des Clients

| # | Invariante | Durchgesetzt durch | Bewiesen durch |
|---|---|---|---|
| I1 | Kein Objekt mit synchronisierbarem Namen fehlt in einem gelieferten Scan. Weggelassen werden nur `CLIENT_DIR` und unverknüpfte Junk-Dateien (Dateien mit Namen aus `proto::name::ignored()` außer `.xlrx-dl-*`, deren `LocalId` in S nicht verknüpft ist). Der Inhalt nicht durchlaufbarer Ordner fehlt. Diese Ordner stehen in `blind`, und solange `blind` nicht leer ist, gibt der Treiber keine Server-Löschung frei. | Scanner (§7.3): Er meldet verknüpfte Dateien mit ignoriertem Namen als opak, ebenso ignorierte und lesbare opake Ordner samt allen Nachkommen. Bei nicht leerem `blind` hält der Treiber jedes `RemoteOp::Delete*` an. Nach dem vollen Scan folgt die Nachprüfung (§7.4). | Sim-Varianten „opake Inodes“ und „nicht-atomarer voller Scan“. Dateisystem-Tests: verknüpfte Datei bzw. verknüpfter Ordner in einen opaken oder ignorierten Ordner verschoben; verknüpfter Ordner in `#recycle`/`@tmp` umbenannt; Elternordner auf dem Server gelöscht, ignorierter Unterordner lokal; `verschieben_waehrend_vollem_scan`. Orakel: keine Server-Löschung, solange die Identität unter der Wurzel außerhalb von `CLIENT_DIR` existiert; nie ein Nutzerverzeichnis als `junk` im Papierkorb. |
| I2 | Keine Server- oder lokale Operation für eingefrorene Schlüssel (opak, ausgeschlossen), auch nicht als Nachbar (Ausweichname, Breaker) | Einfrierregeln in der Engine (E6 mit Kinder- und Nachbarregeln, E7) | Sim-Orakel: keine Op auf eingefrorenen Knoten und keine lokale Op auf opaken L-Objekten, auch nicht `LocalOp::Move` mit `synced_to: None`. Dazu die Sim-Variante „opak und verknüpft“. |
| I3 | Kein Teil-Scan mit Entfernungen; eine verschwundene Identität führt zum vollen Scan | `delta::local_changes` (E3) und Treiber | Sim-Teil-Rescan-Modus nutzt dieselbe Funktion |
| I4 | Nie ein Teil-Schnappschuss. Wurzel fehlt, ersetzt oder unlesbar: Pause. Jeder Lesefehler außer `EACCES`/`EPERM` und einem bestätigten `ENOENT`-Wettlauf bricht den vollen Scan ab, und nichts wird geliefert. | `ScanAbort`, auch `ScanAbort::Io` (§7.4), Marker und Inode (§7.2) | Tests: Wurzel entfernt, ersetzt oder ausgehängt |
| I5 | Commit vor Freigabe. Bei Commit-Fehler gilt Fail-Stop, das Delta geht nicht verloren. Je Zustandsverzeichnis und je Ordner schreibt genau ein Prozess. | `FolderSync::poll`, Peek-then-Clear; `agent.lock` und Ordner-Sperre (§1) | Schatten-Orakel in `persist()`, ENOSPC-Test, `zweiter_agent_startet_nicht`, `zwei_ablagen_gleicher_ordner_pausiert` |
| I6 | Unter einer `(device, op_id)` führt der Server nie zwei verschiedene Operationen aus. Er liefert nie das Ergebnis einer anderen Operation. | Zufälliges `op_base` für jeden frischen Zustand; Body-Vergleich auf dem Server über die kanonische Form (S3); 409 `op_mismatch` oder eine Abfrage mit anderem `op` führt zum Neuaufbau (§9) | Sim: Neuinstallation und Torn-Tail mit beiden Zweigen (gleicher Body liefert das gespeicherte Ergebnis, 409 führt zum Neuaufbau); Server-Test `op_body_abweichung_409`; E2E-Wiederherstellung |
| I7 | Ein Original wird nie ohne bestandene Prüfung ersetzt oder entfernt. Was der Client nicht nachweislich selbst angelegt hat, entfernt er nie. | Intent-Protokoll, Nachprüfung, Papierkorb | Hooks für gleichzeitige Schreiber, Abbruch an jedem Hook |
| I8 | Massenlöschungen werden in beiden Richtungen angehalten, massenhafte Ersetzungen lokaler Dateien ebenso. „Wiederherstellen“ umfasst auch die im Fenster schon ausgeführten Löschungen. Die Entscheidung überlebt einen Neustart. | `DeleteGuard`: Zählseiten Server, lokal und Ersetzen; Bewertung je `plan()`-Ergebnis als Ganzes; 24-h-Summe; `delete_window_log`. Dazu `Engine::decline` (E5, auch für `Replace`). | Sim mit zufälligen Schwellen und Entscheidungen, auch Ablehnen angehaltener Replace-Ops; E2E `rm_rf_1000_angehalten_ablehnen_stellt_wieder_her` (alle 1000 zurück, auch bei geteilter Löschung) |
| I9 | Bei 404 auf die Wurzel, Herabstufung auf „Ansehen“, Widerruf, unbekanntem Refresh-Token oder Abmeldung wird nichts in die Engine gespeist und keine Op gesendet. Der Ordner pausiert. | Treiber, `TokenManager` (§10) | E2E: Gerät widerrufen, Ablage entzogen, `herabstufung_auf_ansehen_pausiert`; `geraet_anmelden` (unbekannter Refresh-Token) |
| I10 | Rücksetzung der Server-DB führt zum Neuaufbau mit frischem Zustand, nie zum Behalten von S. Nach dem Öffnen gibt es keine Remote-Op, keinen Lookup und kein `decline`, bevor ein Abruf mit `check` gelungen ist. | `cursor_tag` bei jedem Abruf und jeder Op (S4); erstes `plan()` erst nach erfolgreichem Abruf (§5) | Server-Tests `cursor_tag_erkennt_ruecksetzung` und `op_mit_veraltetem_cursor_tag_409`; E2E `server_ruecksetzung_baut_neu_auf` |
| I11 | Bleibende HTTP-Fehler (400, 403, 404, 415, 422) werden geparkt. Sie werden nie als `Transient` und nicht in jeder Runde als `Rejected` gemeldet. `Rejected` folgt erst, wenn sich Quelle oder Knoten geändert haben und die Abfrage 404 bestätigt. | Ergebnis-Abbildung (§8.4) | E2E: Park bei 403 |
| I12 | Ein Token-Erneuerer. Neuer Refresh-Token wird gespeichert, bevor er benutzt wird. 429 ist keine Abmeldung. `token_invalid` vom Erneuerungs-Endpunkt meldet sofort ab (`SignedOut`) und wird nie wiederholt. | `TokenManager`: erneuert nur unter `agent.lock` und liest den Refresh-Token vorher neu aus dem `SecretStore` (§1, §10) | Fake-Transport und echter Server (`geraet_anmelden`); `cli_waehrend_run_nutzt_socket` |
| I13 | Transporte folgen keinem Redirect, speichern keine Cookies, cachen nicht. `Authorization` geht nur an `server_url`, nie bei einem 307. `server_url` ist `https://`, `http://` nur für Loopback in Tests. | Transport-Vertrag, 307-Logik in Rust, Prüfung von `server_url` | FakeS3-Test (`redirect_ohne_authorization`); nächtlich `apple-e2e` mit Apple-URLSession |
| I14 | Neu-Planung im Takt, solange nicht konvergiert. Nach leerem `plan()` wird einmal erneut geplant. | `FolderSync` | Treiber-Seeds, Unit-Test mit Fake-Uhr |

### 4. Engine-Erweiterungen

Alle Erweiterungen sind additiv und werden zuerst im Simulator bewiesen. **Maßstab je Teilschritt:** 1 Mio. strenge Seeds über alle neuen Varianten zusammen, mindestens 200 000 je Variante. Die Altvarianten bleiben byte-gleich (Trace-Hash-Tor). Derselbe Maßstab gilt für den Treiber-Modus.

Neue Regel für die CI ist ein **Trace-Hash-Tor**. M5.1 führt es in einem eigenen Commit auf dem sonst unveränderten Stand ein.
- `xlrx-sim` schreibt je Seed einen Hash über die Trace-Zeilen fort: FNV-1a 64. Er wird in `Sim::log` mit jeder Zeile gespeist, nicht aus dem Ringpuffer (`TRACE_LEN`). `debug_summary` zählt nicht mit, und der Hash zieht nichts aus dem RNG.
- Je Variante liegt eine Golden-Datei im Repo: `crates/xlrx-sim/tests/golden/<variante>.txt`, je Zeile `seed hash`, 1000 Seeds wie in der CI.
- Verglichen wird im selben Lauf wie `tests/seeds.rs`, und zwar für die gelaufenen Seeds. `XLRX_SIM_BLESS=1` schreibt die Dateien neu.
- Eine Engine-Änderung, die das Verhalten bestehender Varianten nicht ändern soll, muss die Golden-Dateien byte-gleich lassen. Jede gewollte Änderung schreibt sie ausdrücklich neu, mit Begründung im Commit.

| # | Änderung | Umfang | Simulator-Nachweis | Teilschritt |
|---|---|---|---|---|
| E0 | `Config::max_unconfirmed_deletes` entfällt. Das Feld wird nirgends gelesen. Die einzige Schwelle für Massenlöschungen ist `GuardConfig` im Treiber (§5). Alte Konfigurations-JSON lädt weiter, weil serde das unbekannte Feld ignoriert. ADR 0001, Offene Punkte („Massenlösch-Schutz“), wird im Commit von M5.1 angepasst. | Feld plus vier Setzstellen (`sim.rs`, `scenario.rs`, `examples/scale.rs`, Server-Test `tests/sync.rs`) | Golden-Hashes unverändert | M5.1 |
| E1 | `StateDelta` mit `delta()` und `clear_delta()`. `PartialEq` per derive auf `State`, `Config` und `Tree`. Für `Synced` gibt es ein handgeschriebenes `PartialEq`, das nur `entries` vergleicht (wie `Synced::same_entries`). Der Zähler `mutations` bleibt bewusst außen vor: Er zählt seit Prozessstart und weicht nach dem Laden immer ab. | ca. 120 Zeilen, rein buchhalterisch | Schatten-Zustand in `persist()` (`sim.rs:527-533`): Delta auf Zeilen anwenden, daraus `State` bauen, `== engine.state()`. Gilt für jeden Seed und jede Variante. | M5.4 |
| E2 | *Nicht in M5:* `serde` für `LocalObservation`, `RemoteChange` und `Op`, Eingabe-Journal und `xlrx-sim replay`. Das ist eine Diagnosehilfe; Größe und Datenschutz des Journals sind ungeklärt. | — | — | nach M5 |
| E3 | `delta::local_changes` aus `sim.rs:576-646` nach `xlrx-sync` verlegt | ca. 150 Zeilen | Der Teil-Rescan-Modus der Sim ruft genau diese Funktion. Die Golden-Dateien ändern sich dabei einmal gewollt, weil verschwundene Identitäten jetzt den vollen Scan auslösen. | M5.6 |
| E4 | *Entfällt:* `ensure_next_op`. Eine OpId-Kollision führt zum Neuaufbau (§9). | — | — | — |
| E5 | `decline(ops) -> Vec<NodeId>` für angehaltene Löschungen und Ersetzungen | ca. 60 Zeilen | Zufällige Ablehnung angehaltener Löschungen und `Replace`-Ops. Orakel: Konvergenz, kein Verlust. Nach abgelehntem Ersetzen liegt die lokale Fassung auf dem Server und die verworfene Server-Fassung als Version. | M5.5 |
| E6 | `LocalEntry.opaque` und Einfrierregeln | ca. 200 Zeilen | Opake Inodes, unlesbare Ordner, symbolischer Link ersetzt verknüpfte Datei. Im strengen Modus außerdem: Ein verknüpftes Objekt wird opak, dann löscht der Server den Elternordner (Spiegelfall: lokale Löschung). „Opak und verknüpft“ mit Tausch und Umbenennung auf dem Server sowie lokaler Verschiebung. Verknüpftes Objekt in einen opaken bzw. ignorierten Ordner verschoben. Orakel: keine Op auf eingefrorenen Schlüsseln (I2). | M5.5 |
| E7 | `Filter` (Selective Sync per NodeId, Ignorier-Regeln mit gitignore-Semantik) | ca. 300 Zeilen | Umschalten von Ausschlüssen, ignorierte Namen im Namensvorrat. Bleibt hinter einem Schalter, bis der Maßstab oben erfüllt ist. | M5.10 |
| E8 | Lokale Namensfaltung `Name::local_fold_key()` | Faltungstabelle (C+F) und lokaler Faltungsmodus in `Tree` | Für die heutigen Namensvorräte (`NAMES_CI` in `sim.rs`: ASCII sowie Ä/ä) liefert `local_fold_key` dieselben Schlüssel wie `fold_key`; die Golden-Dateien der Altvarianten bleiben byte-gleich. Seeds mit ß/ss, ς/σ und NFD-Paaren laufen in einer neuen strengen Variante mit eigener Golden-Datei. | M5.2 (vor dem ersten echten APFS-Lauf) |

```rust
// E1: wird in den bestehenden Hilfsfunktionen gesetzt; die Planung liest es nie.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub remote_replaced: bool,          // on_remote_snapshot (engine.rs:271)
    pub remote: BTreeSet<NodeId>,       // r_changed (:1220) ← r_insert/r_remove (:1235-1250)
    pub synced: BTreeSet<NodeId>,       // s_changed (:1126) ← s_insert/s_remove/s_update (:1145-1171)
    pub local_replaced: bool,           // on_local_snapshot (:304); L wird in M5 nicht persistiert
    pub local: BTreeSet<LocalId>,       // l_changed (:1173)
    pub outbox: BTreeSet<OpId>,         // emit_remote (:2129), on_remote_result (:647), decline
    pub pending: BTreeSet<NodeId>,      // finish_remote_update (:286), mark_pending (:838)
    pub local_temps: BTreeSet<LocalId>, // retain (:319, :382: entfernte Schlüssel in der Closure), :586, :1819, :1983
    pub scalars: bool,                  // cursor (:277), next_op (:2122), name_counter (:2265, :2283),
                                        // breakers (:926-927), filter (E7)
}
impl Engine {
    pub fn delta(&self) -> &StateDelta;          // Peek: ein fehlgeschlagener Commit verliert nichts
    pub fn clear_delta(&mut self);               // erst nach erfolgreichem Commit
    pub fn decline(&mut self, ops: &[OpId]) -> Vec<NodeId>;   // DeleteFile, DeleteDir, Replace (E5)
}
```

Direkte Baum-Mutationen außerhalb der Hilfsfunktionen gibt es nur bei `:271` (R wird ersetzt) und `:304` (L wird ersetzt). Beide sind als `*_replaced` erfasst. Geprüft per grep: `synced` und `remote` werden sonst nur in `:1147/1156/1165/1240/1245` verändert.

**E5 `decline`.** Die Funktion nimmt `DeleteFile`, `DeleteDir` und `LocalOp::Replace`.
- **Löschungen:** Sie entfernt die Op aus `outbox` und `sent` (Server-Seite) bzw. aus `inflight_local` (lokale Seite) und ruft `s_remove(node)` auf. Das entspricht dem bestehenden `Breaker::Unlink` (`engine.rs:931-934`). Danach stellen die vorhandenen Regeln wieder her:
  - Server-Seite: Der Knoten ist nur noch in R und wird heruntergeladen. Lokal gelöschte Elternordner kommen über „keep“ zurück.
  - Lokale Seite: Das Objekt ist nur noch in L und wird neu hochgeladen.
- **`Replace`** (Ablehnen beim Ersetzen-Wächter, §5): Sie entfernt die Op aus `inflight_local` und setzt S auf den Server-Stand (`content` und `rev` aus R). L bleibt unverändert.
  - Die bestehende Regel für „nur lokal geändert“ plant dann `RemoteOp::Upload{base_rev}`.
  - Der Server schreibt die lokale Fassung in denselben Knoten und bewahrt die überschriebene Fassung als Version auf. Es entstehen keine Konfliktkopien.
  - Hat sich der Knoten inzwischen weiter geändert, macht der Server aus dem Upload mit veraltetem `base_rev` eine Konfliktkopie.

Angehaltene und geparkte Ops bleiben „in flight“. Damit bleibt `idle` für den ganzen Ordner falsch (`engine.rs:910-916`). Das Sicherheitsnetz ruht also, solange eine solche Op besteht, und zwar im ganzen Ordner, nicht nur für die Eltern angehaltener Kinder.
- Das ist hinnehmbar, denn das Sicherheitsnetz fängt nur Regellücken ab. Der strenge Sim-Modus wertet jeden Einsatz als Fehler (`sim.rs:271-273`), und 1 Mio. strenge Seeds liefen ohne Einsatz (ADR 0001, Ergebnisse).
- Warteketten auf eine angehaltene oder geparkte Op betreffen nur die davon abhängigen Knoten (`busy` → warten, kein Breaker).
- Optional: eine strenge Sim-Variante mit dauerhaft angehaltener Op und dem Orakel „alle nicht abhängigen Knoten konvergieren“.

**E6 Einfrierregeln.** `LocalEntry { …, #[serde(default)] pub opaque: bool }`. Das betrifft 7 Konstruktorstellen: `engine.rs:488/525/559`, `xlrx-sim/src/fs.rs:138/266` und `examples/scale.rs:50/72`. In `:559` (Replace, `..e`) wird `opaque: false` ausdrücklich gesetzt. Sonst erbte die neue Datei das Flag.
- **Opakes L-Objekt:**
  - In `plan_local_new` (`:1809`) übersprungen und als erledigt gezählt.
  - Nie Ziel von `rebind` (`:398-445`).
  - Belegt ein unverknüpftes opakes Objekt den Zielnamen eines neuen R-Knotens, plant `plan_remote_new` (`:1717`) schon heute nichts (`:1751-1753`). Die Konfliktkopie entstünde nur in `plan_local_new` (`:1874`), und das wird übersprungen. Neu ist nur die Problem-Meldung.
- **Ein S-Knoten `n` ist eingefroren** (`frozen(n)`), wenn für `n` oder einen S-Vorfahren gilt:
  - (a) Sein L-Objekt existiert und ist opak. Beispiele: Eine Datei oder ein Ordner wurde unlesbar; eine Datei bekam einen zweiten harten Link. Die Inode bleibt, und damit bleibt auch die Verknüpfung. Ebenso gilt (a) für Nachkommen eines lesbaren opaken oder ignorierten Ordners, die der Scanner opak meldet (§7.3).
  - (b) Sein L-Objekt fehlt, und an seinem S-Ort liegt ein opakes Objekt. Beispiele: Ein symbolischer Link ersetzt eine Datei oder einen Ordner; eine Platte wurde über einem Ordner eingehängt.
- **Ein L-Objekt `o` ist eingefroren**, wenn `frozen_l(o) := o opak ∨ frozen(node_of(o))`.
- **Wirkung des Einfrierens:**
  - `evaluate` (`:941`) überspringt den Knoten.
  - `is_settled` (`:967`) liefert `true`, damit der Knoten nicht in `waiting` bleibt. Das Sicherheitsnetz (`:917-936`) hängt davon nicht ab: Es wird über `cx.stuck` aus Kinder- und Nachbarregeln gespeist. Dass es wegen eingefrorener Knoten nie anspringt, sichern die beiden folgenden Regeln.
  - **Kinderregeln:**
    - In `plan_remote_gone` (`:1352-1364`) **und** `plan_local_gone` (`:1413-1434`) gelten eingefrorene Kinder als `keep`.
    - Die Prüfung steht vor der Auswertung von `eff_remote_loc`. So setzt ein eingefrorenes Kind nie `deleting` oder `leaving`.
    - Fall (b) ist in `plan_remote_gone` schon heute `keep`, weil das opake Objekt am S-Ort unverknüpft ist (`:1354`).
    - Für Fall (a) gilt das nicht. Ein verknüpftes opakes Kind zählt heute als `deleting`, sobald der Server den Elternordner löscht. Der Elternordner meldet dann in jedem Durchlauf `Breaker::Unlink`, und nach `STALL_LIMIT` Durchläufen greift das Sicherheitsnetz.
    - Mit `keep` wird der Elternordner wie bei anderen bleibenden Kindern entknüpft und auf dem Server neu angelegt (`:1372-1374`). Das ist gewollt. Unverändert bleibt der eingefrorene Knoten selbst.
  - **Nachbarregeln:**
    - `yield_local_temp(o)` (`:1978`) prüft `frozen_l(o)` am Anfang, also vor dem Eintrag in `local_temps`.
    - `yield_remote_temp(m)` (`:1998`) prüft `frozen(m)` am Anfang.
    - Für eingefrorene Objekte entsteht keine Op.
    - Die Breaker `TempLocal`/`TempRemote` werden für eingefrorene Objekte nie in `cx.stuck` gelegt (`:1646`, `:1709`, `:1786`). Sonst würde `cx.stuck.first()` das Sicherheitsnetz dauerhaft auf einen wirkungslosen Breaker lenken.
    - Die wartende Regel (`local_move_to`, `remote_move_to_local`, `plan_remote_new`, `plan_local_new`) meldet stattdessen ein Problem („blockiert durch eingefrorenes X“) und bleibt in `waiting`.
- Eingefroren heißt: Am eingefrorenen Objekt wird auf keiner Seite etwas verändert. Das ist absichtlich nicht konvergent. Die Oberfläche zeigt das Problem an, zum Beispiel: „Symbolischer Link wird nicht synchronisiert, ‚Bericht.pdf‘ bleibt auf dem Server erhalten.“
- Das Sim-Orakel für I2 prüft auch lokale Ops ohne Knoten: `LocalOp::Move{local, synced_to: None}` mit `frozen_l(local)` ist ein Fehler.

**E7 Filter** (M5.10, gitignore-Semantik):
- `excluded: BTreeSet<NodeId>` wird an Nachkommen vererbt und überlebt Umbenennungen (PLAN 5.7).
- Ignorier-Regeln wirken **nur auf unverknüpfte Objekte**:
  - Ein passender neuer lokaler Eintrag wird nicht hochgeladen.
  - Ein passender neuer Server-Knoten wird nicht heruntergeladen.
  - Bereits verknüpfte Objekte bleiben synchronisiert.
  - Eine neue Regel löst nie eine Löschung oder Räumung aus.
- Ausgeschlossene Teilbäume:
  - Unverknüpfte R-Knoten werden nicht heruntergeladen.
  - Verknüpftes S ohne L führt nur zu `s_remove`, nie zu einer Server-Löschung. So zählen ausgeschlossene Teilbäume nicht als „lokal gelöscht“ (ADR 0001, Offene Punkte).
  - Lokale Neuzugänge darin werden eingefroren.
  - Räumen („Speicher freigeben“, PLAN 5.7) geschieht nur ausdrücklich, über `forget_subtree` plus Lösch-Protokoll des Executors, und nur für vollständig synchronisierte Objekte. Alles mit ungesicherten Änderungen bleibt und wird zur Rückfrage.
- Der Scanner kennt keine Nutzerregeln. Scanner und Engine-Regeln können deshalb nicht auseinanderlaufen.
- Maßgeblich ist der Filter in `State`. Er wird im selben Commit wie der restliche Zustand gespeichert.
- Zusätzlich hält `account.sqlite` die Nutzerwahl als Vorlage für einen Neuaufbau: `folder_filter(folder, kind exclude|ignore, node, path, pattern)`.
  - Die Tabelle wird bei jeder Änderung durch den Nutzer mitgeschrieben und nur beim Neuaufbau gelesen (§6).
  - Ein Auseinanderlaufen der beiden Kopien ist harmlos. Ein fehlender Ausschluss führt nur zu Downloads, ein zusätzlicher nur dazu, dass etwas nicht übertragen wird. Keine Regel löst eine Löschung aus.

**E8 Lokale Faltung** (M5.2):
- `Name::fold_key` (Kleinschreibung plus NFC) hält ß/ss und ς/σ absichtlich auseinander. APFS und casefold-ext4 legen solche Paare zusammen.
  - Folge ohne E8: Ein neuer Server-Knoten „Masse.pdf“ neben lokalem „Maße.pdf“ hat für die Engine keinen lokalen Beleger.
  - Sie plant einen Download, der an `EEXIST` scheitert, und plant ihn in jeder Runde neu.
- `Name::local_fold_key()` ist die volle Unicode-Faltung auf dem NFC-Namen: Status C und F aus `CaseFolding.txt`, z. B. ß/ẞ→ss, ς→σ, ﬁ→fi. Sie ist eine Obermenge dessen, was APFS und casefold-ext4 zusammenlegen.
- `Tree` bekommt einen eigenen lokalen Faltungsmodus statt `fold: bool`. Nur der lokale Baum nutzt ihn (`Tree::new(local_case_insensitive)` in `from_state` und `on_local_snapshot`); R (`Tree::new(true)`) faltet weiter mit `fold_key`, ebenso der Server. Im Simulator nutzt `local_fold_key` auch `SimFs`.
- Wirkung:
  - „Masse.pdf“ hat jetzt „Maße.pdf“ als lokalen Beleger.
  - Die bestehende Regel in `plan_remote_new` („die Namen kollidieren nur lokal“) benennt den Ankömmling auf dem Server um.
  - Zu viel Faltung kostet höchstens eine unnötige Umbenennung auf dem Server.
- Die Probe (§7.2) lehnt ein Volume ab, das ein Paar zusammenlegt, das `local_fold_key` nicht zusammenlegt.

### 5. Ordner-Treiber `xlrx-driver`

```rust
pub type Millis = u64;                                   // monoton, nur für Zeitgeber im Prozess
pub struct Now { pub mono: Millis, pub unix_ms: i64 }    // jede Eingabe trägt beide Uhren
pub struct FolderConfig { pub engine: xlrx_sync::Config, pub api_root: i64, pub guard: GuardConfig, pub timing: Timing }
pub struct GuardConfig { pub delete: Limit, pub replace: Limit /* Vorschlag: wie delete */,
                         pub quiet_reset_ms: Millis /*10 min*/, pub day_factor: u32 /*4: gleitende 24-h-Summe*/ }
pub struct Limit { pub max_files: u32 /*200*/, pub max_permille: u32 /*100 = 10 %*/, pub floor: u32 /*20*/ }
pub trait Store {
    type Error: std::error::Error + Send + Sync + 'static;
    fn load(&mut self) -> Result<Option<Loaded>, Self::Error>;
    /// Eine atomare, dauerhafte Transaktion: Engine-Delta plus Treiber-Zeilen. Bei Err bleibt delta() unverändert.
    fn commit(&mut self, eng: &xlrx_sync::Engine, rows: &DriverRows) -> Result<(), Self::Error>;
}
pub struct DriverRows { pub meta: FolderMeta /* instance, op_base, cursor_tag, device_id, … */,
                        pub window: DeleteWindow /* je Seite: Fenster und 24-h-Summe */,
                        pub window_log: Vec<WindowEntry> /* delete_window_log */,
                        pub held: BTreeMap<(Side, NodeId), Held> /* Side: Remote | Local | Replace */,
                        pub failing: BTreeMap<FailKey, Failing> /* gleiche lokale Fehlschläge */,
                        pub parked: BTreeMap<OpId, Parked>, pub issues: Vec<Issue> }
impl<S: Store> FolderSync<S> {
    pub fn open(cfg: FolderConfig, store: S, now: Now) -> Result<Self, OpenError<S::Error>>; // lädt auch `held`
    pub fn poll(&mut self, now: Now) -> Result<Vec<Job>, Failed<S::Error>>;  // committet VOR der Rückgabe
    pub fn complete(&mut self, done: Done, now: Now);
    pub fn hint_remote(&mut self, seq: Seq, now: Now);                        // SSE
    pub fn hint_local(&mut self, dirs: Vec<LocalId>, rescan_all: bool, now: Now);
    pub fn decide(&mut self, d: Decision, now: Now);                         // Löschungen, Ersetzen, Neuaufbau, Trennen
    pub fn set_paused(&mut self, why: Option<PauseReason>);
    pub fn next_deadline(&self) -> Option<Millis>;
    pub fn status(&self) -> FolderStatus;
    pub fn engine(&self) -> &xlrx_sync::Engine;
}
pub enum Job {
    Fetch { cursor: Option<Seq>, check: Option<String> },
    Scan(ScanRequest),                                   // Full | Dirs(Vec<LocalId>); dazu die in S verknüpften LocalIds (§7.3)
    Local { id: OpId, op: LocalOp },
    Remote { id: OpId, op: RemoteOp, attempt: Attempt, cursor: Seq, check: Option<String> },  // cursor/check: S4
    Lookup { id: OpId },                                 // GET /sync/ops …?with_op=true (vor decline)
}
pub enum Attempt { First, MaybeSent }                    // MaybeSent ⇔ id < boot_next_op oder Wiederholung
pub enum Done {
    Fetched { changes: Vec<RemoteChange>, cursor: Seq, cursor_tag: Option<String> },  // ALLE Seiten
    FetchFailed(FetchFailure),                           // RootGone(404) | CursorInvalid | Auth | Net
    Scanned(ScanOutcome), ScanFailed(ScanAbort),
    Local { id: OpId, result: LocalResult },
    Remote { id: OpId, outcome: RemoteOutcome },
    Looked { id: OpId, found: Option<RemoteOutcome> },   // None: 404; Final: gleiches op; Rebuild: anderes op
}
pub enum ScanOutcome { Snapshot { root: LocalId, obs: Vec<LocalObservation>, blind: Vec<LocalId>, stable: bool },
                       Dirs { listings: Vec<DirListing>, ancestors: Vec<LocalObservation> } }
pub enum RemoteOutcome { Final(RemoteResult), Retry { why: String }, Park { status: u16, why: String },
                         Rebuild { why: String } }      // 409 op_mismatch oder cursor_invalid, Abfrage mit anderem op
```

**Zeit.**
- `Millis` ist monoton und gilt nur für Zeitgeber im Prozess.
- Alles Persistierte (`held`, `parked`, `issues`, Lösch-Fenster) speichert der Treiber als Wanduhr `unix_ms`.
- Beim Laden rechnet er diese Werte in die monotone Zeit um. Werte in der Zukunft kappt er auf jetzt.
- Alle Zeitrechnungen sind sättigend.

**Ablauf von `poll(now)`:**

1. **Eingaben anwenden.**
   - `Fetched` führt zu `on_remote_changes(alle Seiten, cursor)` (`engine.rs:222`) und speichert `cursor_tag`.
   - `Snapshot` führt zu `on_local_snapshot` (`:296`). `blind` und `stable` gehen an den `DeleteGuard`.
   - `Dirs` wird erst bei Ankunft gegen `engine.local_tree()` ausgewertet, mit `local_changes`.
     - Ergebnis `Changes` wird mit `on_local_changes(upserts, vec![])` eingespeist.
     - Ergebnis `NeedFullScan` wird **verworfen**, und ein voller Scan wird eingeplant. Nur-Upserts dürfen nie eingespeist werden, solange eine verschwundene Identität offen ist: `rebind` verknüpft nur neu, wenn die alte ID in L fehlt (`:398-445`). Sonst würde ein Atomic Save eine Konfliktkopie erzeugen (`:1838-1844`).
   - `Rebuild` aus einem Ergebnis oder einer Abfrage und `FetchFailed(CursorInvalid)` werden nie eingespeist. Sie führen zum Neuaufbau (§6, §9).
2. **Abruf** wird angestoßen, wenn eine dieser Bedingungen gilt: `wants_fetch()`, eine SSE-`seq` größer als der Cursor, `pending` ist nicht leer, oder der Takt ist fällig (60 s, wenn SSE fehlt).
3. **Scan.**
   - `wants_full_scan()` oder eine Eskalation führt zu `Scan(Full)`.
   - Entprellte schmutzige Ordner führen zu `Scan(Dirs)`.
   - Ergebnisse `Precondition`, `Error` und `SourceChanged` markieren die Ordner der Op als schmutzig.
   - **Gleiche lokale Fehlschläge** zählt der Treiber je Schlüssel: Knoten bzw. `LocalId`, Op-Art, Ziel-Elternordner und Zielname. Das betrifft Fehlschläge, die sich ohne Änderung wiederholen.
     - Jede Änderung an R oder L an diesem Schlüssel setzt den Zähler zurück.
     - Ab dem dritten Fehlschlag wird die neu geplante Op angehalten. Sie bleibt in `inflight_local` und wird nach einem Neustart am Schlüssel wieder angehalten.
     - Backoff 1 min bis 6 h; die Oberfläche zeigt ein Problem.
     - Die Tabelle `parked` passt dafür nicht, weil lokale Ops in jeder Runde eine neue OpId bekommen (§7.6).
4. **Planen.**
   - Nach `open()` ruft der Treiber `plan()` erst auf, wenn ein Abruf erfolgreich war (mit `check`, sobald ein Cursor gespeichert ist). Vorher gibt es keine Remote-Op, keinen `Lookup` und kein `decline`, auch keine Wiederholung aus der Outbox.
   - Ist das Ergebnis leer und hat sich `synced.mutations()` geändert, wird einmal erneut geplant.
   - Solange der Ordner nicht ruhig ist, wird alle 2 s neu geplant, damit `STALL_LIMIT = 3` (`:148`) seine Durchläufe bekommt.
5. **`DeleteGuard`** sortiert Lösch- und Ersetzen-Ops aus (siehe unten).
6. **Commit** von Delta und Treiber-Zeilen.
   - Erst bei `Ok` folgen `clear_delta()` und die Freigabe der Jobs.
   - Bei `Err` wechselt `FolderSync` in den Zustand `Failed`. Der Aufrufer verwirft das Objekt, setzt den Ordner auf „Fehler“ und öffnet mit Backoff neu (`from_state(load())`).
   - Ein Commit-Fehler gilt als transient und führt nie zum Neuaufbau (§6).
7. **Gruppen-Commit.** Ergebnisse und Eingaben ohne neue Ops werden höchstens 1 s gesammelt. Ein Absturz verliert dann nur den Rest seit dem letzten Commit. Das entspricht dem Absturzmodell der Sim: Ergebnis verloren nach der Wirkung (`sim.rs:739-759`).

`Transient` wird nie in die Engine gespeist. Wiederholungen bleiben im Treiber und nutzen dieselbe OpId. Pro OpId läuft nie mehr als ein Versuch zugleich.

**`DeleteGuard`** zählt drei Seiten getrennt, alle im Treiber und persistent:
- **Server-Seite:** lokal gelöscht, `RemoteOp::Delete*`.
- **Lokale Seite:** auf dem Server gelöscht, `LocalOp::Delete*`.
- **Ersetzen:** auf dem Server verändert, `LocalOp::Replace`. Diese Seite hat eine eigene Schwelle (`replace`, Vorschlag: gleiche Formel). Grund: Wird eine Freigabe per SMB auf dem NAS überschrieben, etwa durch Ransomware, entsteht keine Server-Version. Der Client würde dann jedes lokale Original ersetzen.

Ablauf:
- **Auslösen.**
  - Der Wächter bewertet die Ops einer Seite aus einem `plan()`-Ergebnis als Ganzes. Die Schwelle ist `max(floor, min(max_files, max_permille·|S|/1000)) + allowance`.
  - Übersteigen *im Fenster freigegebene plus neue* Ops die Schwelle, wird die gesamte neue Menge angehalten, nicht nur der Überschuss.
  - Lösch-Ops werden erst freigegeben, wenn ihre Quelle ruhig ist. Bei lokalen Löschungen heißt das: Entprellung abgelaufen, kein Scan offen. So zerfällt ein laufendes `rm -rf` nicht in Teilmengen unter der Schwelle. Bis dahin bleiben die Ops unversandt in Outbox bzw. `inflight_local`.
  - Das Fenster wird erst nach 10 min ohne Löschung und bei ruhigem Ordner zurückgesetzt.
  - Zusätzlich gilt je Seite eine gleitende 24-h-Summe mit der Schwelle mal `day_factor` (4), gespeichert in `delete_day` (§6). So entkommt auch ein Tröpfeln mit Pausen über 10 min nicht.
  - Jede freigegebene Löschung wird im selben Commit in `delete_window_log(side, node, kind, parent, name, op_id, seq, trash_id)` protokolliert.
- **Anhalten ohne Schwelle.** Dafür ist keine neue Engine-Regel nötig. Erlauben und Wiederherstellen funktionieren wie bei der Schwelle.
  - Solange der letzte volle Scan nicht durchlaufbare Ordner meldet (`blind`, §7.3), hält der Wächter jede `RemoteOp::Delete*` an. Problem: „Ordner X nicht lesbar, N Löschungen angehalten“.
  - Ein Schnappschuss kann nach drei instabilen vollen Scans geliefert werden (`stable = false`, §7.4). Dann hält der Wächter alle daraus folgenden Server-Löschungen an und fragt nach.
- **Halten.**
  - Angehaltene Server-Ops bleiben in Outbox und `sent`.
  - Angehaltene lokale Ops (Löschen, Ersetzen) bleiben in `inflight_local`.
  - Die Tabelle `held(side, node)` wird im selben Commit gespeichert, auch für die Seite `replace`.
  - `open()` lädt die `held`-Zeilen vor dem ersten `poll`. Nach einem Neustart liefert erst das erste `plan()` die Outbox erneut (`engine.rs:850-860`). `from_state` sendet nichts; es beginnt mit leerem `sent` und leerem `inflight_local`, die Outbox bleibt im `State`.
  - Lokale Löschungen und Ersetzungen werden nach einem Neustart mit neuen IDs neu geplant. Der Wächter erkennt alle Ops am Knoten und hält sie wieder an.
  - Solange Ersetzungen angehalten sind, läuft der Papierkorb-Ablauf (Grund `replaced`) nicht.
- **Erlauben** gibt frei und setzt `allowance += n`.
- **Wiederherstellen** umfasst die angehaltenen Ops *und* die bereits ausgeführten Löschungen des Fensters. Bereits ausgeführt sind die Zeilen aus `delete_window_log` und angehaltene Ops, die `Lookup` als dem Server bekannt meldet. Die Oberfläche nennt beide Zahlen.
  - **Server-Seite:**
    - Für jede angehaltene Op folgt zuerst `Lookup`. Dem Server unbekannte Ops (404) nimmt `decline` zurück.
    - Ausgeführte Löschungen holt `POST /api/trash/{id}/restore` zurück (`id` ist die `NodeId`; sie bleibt erhalten). Der Endpunkt besteht bereits und nimmt Geräte-Tokens an (`api/mod.rs`, Router für Browser und Geräte).
    - Ordner werden vor ihren Kindern wiederhergestellt: `DeleteDir` erzeugt eigene Papierkorb-Einträge, und `ops::restore` legt ein Kind ohne lebenden Elternordner an die Wurzel.
    - Danach lädt `plan_remote_new` die Knoten erneut herunter.
  - **Lokale Seite:**
    - Bevorzugt wird aus dem Server-Papierkorb wiederhergestellt, für angehaltene und für bereits lokal angewendete Löschungen. Das erhält Identität und Versionen.
    - Angehaltene Ops nimmt danach `decline` zurück. Die Engine verknüpft sie per gleichem Namen und Inhalt; bereits gelöschte Objekte lädt sie erneut herunter.
    - Ohne Server-Kopie (Löschung per SMB, §7.6) gilt: `decline` für die angehaltenen Ops; die angewendeten Löschungen werden aus dem Client-Papierkorb (`trash.node`) zurückgeholt und als neu hochgeladen.
  - **Ersetzen:**
    - Der Prompt lautet „Viele Dateien wurden auf dem Server verändert“.
    - Ablehnen ruft `decline` (E5) für die angehaltenen `Replace`-Ops auf. Die lokale Fassung gewinnt und geht als neue Version in denselben Knoten.

### 6. Persistenz

**Dateien.** Ablage auf macOS unter `~/Library/Application Support/de.xlrx.drive/` (gehört dem Agent), auf Linux unter `$XDG_STATE_HOME/xlrx-drive/`.

| Datei | Inhalt | Schreiber |
|---|---|---|
| `account.sqlite` | Konto, Ordnerliste, Einstellungen, Aktivität, Vorlage der Auswahl (`folder_filter`) | Agent |
| `roots/<uuid>/state.sqlite` | Engine-`State` und Treiber-Zeilen | genau ein `RootActor`-Thread |
| `roots/<uuid>/local.sqlite` | Hash-Cache, Intents, Papierkorb-Buch, Transfers | Executor und Scanner über eine `Mutex<Connection>`, synchron |

- Die Trennung von `state.sqlite` und `local.sqlite` gibt dem Executor einen eigenen synchronen Commit-Pfad. Ein Intent wartet so nie auf einen großen Engine-Commit, etwa die ersten 1 Mio. R-Zeilen.
- Bei einem Neuaufbau bleibt `local.sqlite` erhalten: Hash-Cache, Intents und Papierkorb bleiben gültig. Auch `account.sqlite` bleibt erhalten. Aus `folder_filter` übernimmt der Neuaufbau ab M5.10 die Auswahl (siehe Laden).
- Tokens liegen nie in SQLite.
- **Ein Prozess je Ablage.**
  - `xlrx_client::Agent::open` hält für seine ganze Lebensdauer `flock(LOCK_EX|LOCK_NB)` auf `<Ablage>/agent.lock`. Ist die Sperre belegt, scheitert der Aufruf mit `AgentError::AlreadyRunning`.
  - Jeder `RootActor` sperrt zusätzlich `<root>/.xlrx-client/lock`. Ist diese Sperre belegt, etwa weil eine zweite Ablage denselben Ordner nutzt, pausiert der Ordner mit einem Problem.

**Pragmas:**
- `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`, `busy_timeout=5000`.
- Auf Apple zusätzlich `fullfsync=ON` und `checkpoint_fullfsync=ON`, weil `fsync` dort den Laufwerkscache nicht leert.
- `PRAGMA quick_check` beim Start.
- Alle `u64` (`NodeId`, `LocalId`, `OpId`, `Seq`, `Rev`) laufen über `struct SqlU64(u64)` mit Bit-Cast nach `i64`. Grund: rusqlite kann `u64` nur mit `fallible_uint` und scheitert oberhalb von `i64::MAX` (`to_sql.rs:272-274`, `from_sql.rs:136-137`). `LocalId::GONE = u64::MAX` wird so zu `-1`.

```sql
-- state.sqlite (Schema v1)
CREATE TABLE root_meta (id INTEGER PRIMARY KEY CHECK (id = 1),
  schema_version INTEGER NOT NULL, instance TEXT NOT NULL,      -- neu bei jedem Neuaufbau
  device_id INTEGER NOT NULL,                                    -- nur zur Information; ein Wechsel baut nicht neu auf (§9)
  api_root INTEGER NOT NULL, op_base INTEGER NOT NULL,           -- rand31 << 32
  config TEXT NOT NULL,                                          -- serde_json xlrx_sync::Config
  cursor INTEGER, cursor_tag TEXT, next_op INTEGER NOT NULL, name_counter INTEGER NOT NULL,
  breakers_used INTEGER NOT NULL, last_breaker TEXT, filter TEXT, -- filter ab M5.10
  root_ino INTEGER NOT NULL, marker TEXT NOT NULL, caps TEXT NOT NULL, commit_seq INTEGER NOT NULL) STRICT;
CREATE TABLE remote (node INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL,
  kind INTEGER NOT NULL, hash BLOB, size INTEGER, rev INTEGER NOT NULL) STRICT;
CREATE TABLE synced (node INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL,
  kind INTEGER NOT NULL, hash BLOB, size INTEGER, rev INTEGER NOT NULL, local INTEGER NOT NULL,
  fp_size INTEGER, fp_mtime_ns INTEGER, fp_ctime_ns INTEGER) STRICT;  -- kein UNIQUE-Index auf local (siehe Laden)
CREATE TABLE outbox (op_id INTEGER PRIMARY KEY, op TEXT NOT NULL) STRICT;        -- RemoteOp-JSON = Wire-Form
CREATE TABLE pending (node INTEGER PRIMARY KEY, seq INTEGER NOT NULL) STRICT;
CREATE TABLE local_temps (local INTEGER PRIMARY KEY, dir INTEGER NOT NULL, name TEXT NOT NULL) STRICT;
CREATE TABLE delete_window (id INTEGER PRIMARY KEY CHECK (id = 1), started_ms INTEGER, last_ms INTEGER,
  released_remote INTEGER NOT NULL, released_local INTEGER NOT NULL, released_replace INTEGER NOT NULL,
  allowance_remote INTEGER NOT NULL, allowance_local INTEGER NOT NULL, allowance_replace INTEGER NOT NULL) STRICT;
CREATE TABLE delete_window_log (side TEXT NOT NULL CHECK (side IN ('remote','local')), node INTEGER NOT NULL,
  kind INTEGER NOT NULL, parent INTEGER NOT NULL, name TEXT NOT NULL, op_id INTEGER NOT NULL,
  seq INTEGER, trash_id INTEGER) STRICT;  -- jede im Fenster freigegebene Löschung, im selben Commit wie die Freigabe (§5)
CREATE TABLE delete_day (side TEXT NOT NULL CHECK (side IN ('remote','local','replace')), released_ms INTEGER NOT NULL,
  n INTEGER NOT NULL) STRICT;  -- freigegebene Löschungen bzw. Ersetzungen je Seite für die gleitende 24-h-Summe (§5);
                               -- Zeilen älter als 24 h entfallen beim nächsten Commit
CREATE TABLE held (side TEXT NOT NULL CHECK (side IN ('remote','local','replace')), node INTEGER NOT NULL,
  op_id INTEGER, since_ms INTEGER NOT NULL, PRIMARY KEY (side, node)) STRICT;
CREATE TABLE failing (key TEXT PRIMARY KEY, count INTEGER NOT NULL, last_ms INTEGER NOT NULL,
  next_try_ms INTEGER NOT NULL) STRICT;  -- gleiche lokale Fehlschläge je Schlüssel (§5, §7.6)
CREATE TABLE parked (op_id INTEGER PRIMARY KEY, status INTEGER NOT NULL, reason TEXT NOT NULL,
  since_ms INTEGER NOT NULL, next_try_ms INTEGER NOT NULL) STRICT;
CREATE TABLE issues (key TEXT PRIMARY KEY, kind TEXT NOT NULL, path TEXT, detail TEXT,
  first_ms INTEGER NOT NULL, last_ms INTEGER NOT NULL) STRICT;
-- später, nach der M5-Abnahme und nur bei Bedarf: local (persistiertes L), raw_names, watch (FSEvents-ID, Volume-UUID)

-- local.sqlite
CREATE TABLE hash_cache (local INTEGER PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
  ctime_ns INTEGER NOT NULL, hashed_at_ns INTEGER NOT NULL, hash BLOB NOT NULL) STRICT; -- Schlüssel LocalId, nie st_dev
CREATE TABLE intent (id INTEGER PRIMARY KEY, op_id INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('download','replace','delete')),
  dir_path BLOB NOT NULL, dir_ino INTEGER NOT NULL, target_name BLOB NOT NULL,
  temp_name BLOB, temp_ino INTEGER, orig_ino INTEGER, trash_name BLOB, expect TEXT,
  phase TEXT NOT NULL, created_ms INTEGER NOT NULL) STRICT;
CREATE TABLE trash (id INTEGER PRIMARY KEY, trash_name BLOB NOT NULL UNIQUE, orig_path BLOB NOT NULL,
  ino INTEGER, node INTEGER, hash BLOB, size INTEGER,
  reason TEXT NOT NULL CHECK (reason IN ('delete','replaced','junk','recovered')), trashed_ms INTEGER NOT NULL) STRICT;
CREATE TABLE transfer (op_id INTEGER PRIMARY KEY, kind TEXT NOT NULL, hash BLOB NOT NULL, size INTEGER NOT NULL,
  upload_id TEXT, created_ms INTEGER NOT NULL) STRICT;

-- account.sqlite
CREATE TABLE account (id INTEGER PRIMARY KEY CHECK (id = 1), server_url TEXT NOT NULL,
  device_id INTEGER, device_name TEXT NOT NULL, confirm_until TEXT, status TEXT NOT NULL) STRICT;
CREATE TABLE folder (id TEXT PRIMARY KEY, api_root INTEGER NOT NULL UNIQUE, root_node INTEGER NOT NULL,
  root_name TEXT NOT NULL, role TEXT NOT NULL, local_path BLOB NOT NULL,
  mode TEXT NOT NULL DEFAULT 'mirror' CHECK (mode IN ('mirror','fileprovider')),
  run_state TEXT NOT NULL, run_reason TEXT, created_ms INTEGER NOT NULL) STRICT;
CREATE TABLE folder_filter (folder TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('exclude','ignore')),
  node INTEGER, path BLOB, pattern TEXT) STRICT;  -- ab M5.10; bei jeder Nutzeränderung geschrieben, nur beim Neuaufbau gelesen
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;   -- Bandbreite, Schwellen, Papierkorb-Tage
```

**Zeiten.**
- Alle `*_ms`-Spalten speichern Wanduhr-Zeit (`unix_ms`), nie die monotone Zeit `Millis` des Treibers.
- Beim Laden werden sie in `Millis` umgerechnet. Werte in der Zukunft werden auf „jetzt“ gekappt.
- Gerechnet wird sättigend (§5).

**Commit.** Eine `BEGIN IMMEDIATE`-Transaktion:
- Für jeden Schlüssel im Delta wird der aktuelle Wert aus `engine.state()` gelesen. Ist er vorhanden, wird er geschrieben (Upsert). Fehlt er, wird die Zeile gelöscht.
- Bei `*_replaced` wird die Tabelle geleert und vollständig neu geschrieben.
- Die Skalare werden immer geschrieben, und `commit_seq` wird hochgezählt.

**Laden:**
- R über `Tree::new(true)` und `insert`.
- S über `Synced::default().insert`. Ein `false` bedeutet Korruption.
  - Diese Prüfung ersetzt einen UNIQUE-Index auf `synced.local`.
  - Grund: SQLite prüft UNIQUE je Anweisung. Ein Commit, der eine `LocalId` von einem Knoten an einen anderen weitergibt, könnte sonst je nach Schreibreihenfolge scheitern.
- Danach folgen `Engine::from_state` und `check_invariants()` (`engine.rs:2376`).
  - `check_invariants()` läuft zusätzlich periodisch, etwa stündlich und abseits des heißen Pfads.
  - Eine Verletzung führt zum Neuaufbau. Das ist die Selbstheilung aus PLAN 5.5.
- Die Treiber-Zeilen werden mitgeladen, `held` vor dem ersten `poll`. Grund: Das erste `plan()` liefert die Outbox erneut (§5).
- Ein **Neuaufbau** erfolgt nur in drei Fällen:
  - bei nachgewiesener Korruption: `quick_check` meldet Fehler, eine Zeile lässt sich nicht dekodieren, `Synced::insert` liefert `false` oder `check_invariants` scheitert;
  - bei 409 `cursor_invalid`;
  - bei 409 `op_mismatch` (§8.4, §9).
- **Ablauf des Neuaufbaus:**
  - Die alte Datei wird nach `state.sqlite.corrupt-<ts>` umbenannt.
  - Es folgen eine neue Instanz, ein neues `op_base`, ein Abruf ab Cursor 0 und ein voller Scan.
  - Ein Neuaufbau ist ein Abgleich mit leerem S. Er löscht nichts (Leitplanke 7).
  - Er hat aber Kosten: Noch nicht übertragene Löschungen kommen zurück. Noch nicht übertragene Verschiebungen werden zu Kopien an beiden Orten.
  - Deshalb ermittelt der Treiber vor dem ersten `plan()` die Objekte ohne Gegenstück auf der anderen Seite. Grundlage sind R und der volle Scan. Ergebnis: Zahl der Uploads, Zahl der Downloads und die Byte-Summe.
  - Ab der Wächter-Schwelle fragt die Oberfläche: „Ordner neu abgleichen? N Objekte kommen auf dem Server zurück, M werden heruntergeladen.“ Bis zur Antwort bleibt der Ordner pausiert. Die Entscheidung wird gespeichert.
  - Unter der Schwelle meldet die Oberfläche „Ordner wurde neu abgeglichen“.
- **Auswahl beim Neuaufbau (ab M5.10):**
  - Nach Korruption oder `op_mismatch` setzt der Neuaufbau den Filter aus `folder_filter` in den frischen `State`. Das geschieht, bevor der neue Zustand zum ersten Mal committet oder plant. Die NodeIds bleiben dabei gültig.
  - Nach `cursor_invalid` wird zuerst vollständig ab Cursor 0 abgerufen. Dann werden die Ausschlüsse über `path` im neuen R aufgelöst. Ignorier-Muster werden unverändert übernommen.
  - Nicht auflösbare Pfade werden als Problem gemeldet und nie geraten.
- **Transiente Fehler** (BUSY, IOERR, FULL, CANTOPEN, NOMEM, Keychain nicht lesbar) benennen nichts um und bauen nicht neu auf.
  - Der Ordner pausiert mit Backoff.
  - Er lädt erneut, sobald der Speicher wieder funktioniert. Das ist derselbe Weg wie beim Fail-Stop (§5).
- Ist `schema_version` neuer als unterstützt, pausiert der Ordner mit „Neuere App-Version nötig“. Auch hier wird nichts umbenannt.

**Argument für die Absturzsicherheit:**
1. **Atomar.** Die DB enthält immer genau einen `State`, der an einer Commit-Grenze existiert hat. Das ist das Absturzmodell der Sim (`sim.rs:518-525`), das 1 Mio. strenge Seeds abdecken.
2. **Commit vor Wirkung.**
   - Server-Ops: Die Outbox-Zeile ist vorher dauerhaft. Wiederholt wird mit gleicher OpId, der Server dedupliziert.
   - Ausweich-Umbenennungen: `local_temps` ist vorher dauerhaft.
   - Lokale Ops werden nicht protokolliert. Ihre Wirkung findet der Pflicht-Scan nach dem Neustart.
3. **Wiederholbar.** Abgesichert durch die Vorbedingungen der Server-Ops (`Moved`, `NodeGone`, `RevMismatch`), die Vorbedingungen des Executors und die Intents (§7.6).
4. **Dauerhaft.** WAL mit `synchronous=FULL`, auf Apple zusätzlich `F_FULLFSYNC`.
5. **Fail-Stop.** Nie mit einem Zustand weiterarbeiten, der nicht gespeichert ist.
6. **Letzte Rückfallebene.** Ein frischer Zustand löscht nie. Er bringt aber noch nicht übertragene Löschungen zurück und macht aus noch nicht übertragenen Verschiebungen Kopien (siehe Laden).
7. **Hash-Cache.** Er darf hinterherhinken, aber nie falsch sein:
   - Ein Treffer braucht einen exakten Fingerprint und `max(mtime, ctime) ≤ hashed_at − Fenster`. Das ist die Regel von `xlrx_chunk::CacheEntry::is_racy`, mit dem Fenster aus §7.2.
   - `hashed_at_ns` ist Wanduhr-Zeit und wird *vor* dem Hashen genommen.
   - Ergebnisse ohne gesicherten `fp` kommen nicht in den Cache (§7.6).
   - Nach einem unsauberen Ende gelten jüngere Einträge als unsicher.

### 7. Dateisystem-Adapter

#### 7.1 `xlrx-fs`: Systemaufrufe (rustix 1.1.5, kein `unsafe`)

| Funktion | Linux | macOS (APFS) |
|---|---|---|
| `open_root`, `open_dir_at` | `openat(O_DIRECTORY\|O_NOFOLLOW\|O_CLOEXEC)` | gleich |
| `stat_at` | `statx(AT_SYMLINK_NOFOLLOW, STATX_BTIME)` | `fstatat(AT_SYMLINK_NOFOLLOW)` (`st_flags`, `st_birthtime`) |
| `fs_type` | `fstatfs` (`f_type`) | `fstatfs` (`f_fstypename`) |
| `read_dir` | `rustix::fs::Dir::read_from` | gleich |
| `create_excl_at`, `open_read_at` | `openat(O_CREAT\|O_EXCL\|O_NOFOLLOW)`, `openat(O_RDONLY\|O_NOFOLLOW)` | gleich |
| `rename_at(NoReplace)` | `renameat2(RENAME_NOREPLACE)` | `renameatx_np(RENAME_EXCL)` |
| `rename_at(Exchange)` | `renameat2(RENAME_EXCHANGE)` | `renameatx_np(RENAME_SWAP)` |
| `rename_at(Plain)` | `renameat`, nur für Schreibweisen-Wechsel desselben Inodes (§7.6) | gleich |
| `mkdir_at`, `rmdir_at`, `unlink_at` | `mkdirat`, `unlinkat(AT_REMOVEDIR)`, `unlinkat` (nur für bewiesen eigene Temps) | gleich |
| `sync_file`, `sync_dir` | `fsync` bzw. `fdatasync`, `fsync(dirfd)` | `fcntl_fullfsync`, `fsync(dirfd)` (die Kosten von F_FULLFSYNC auf Verzeichnissen misst die macOS-CI) |

Belege in rustix:
- `renameat_with` ist `cfg(any(apple, linux_kernel, redox))` (`rustix/src/fs/at.rs:298-302`).
- Auf Apple ist `EXCHANGE = RENAME_SWAP` und `NOREPLACE = RENAME_EXCL` (`backend/libc/fs/types.rs:544-548`).
- `fcntl_fullfsync` steht in `fs/fcntl_apple.rs:24`, `fstatfs` in `fs/fd.rs:181`.

#### 7.2 Einrichten, Probe, Wurzel-Identität

Beim Hinzufügen eines Ordners und bei jedem Start läuft Folgendes in `<root>/.xlrx-client/` (`CLIENT_DIR`):

1. **Marker.** `root.json` mit Ordner-UUID und einem zufälligen Token.
   - Die Identität der Wurzel ist `root_ino` **und** dieser Token. `st_dev` wird nicht verwendet, weil es auf macOS bei externen Volumes nicht stabil ist.
   - Abweichung, fehlende Wurzel oder unlesbare Wurzel führen zu `ScanAbort` und Pause. Das deckt eine ausgehängte Platte, einen verschobenen und einen neu angelegten Ordner ab.
2. **Probe** in `probe/`. Gemessen wird nur, was Engine oder Executor beeinflusst. Ergebnis ist `VolumeCaps`:
   - Groß-/Kleinschreibung: Anlegen von `Probe-Aa`, dann `stat` auf `probe-aA`. Das Ergebnis geht in `Config.local_case_insensitive`.
   - Zusätzlich geprüft werden die Paare ß/SS, ẞ/ss, ς/σ, ﬁ/fi und Ä/ä in NFD. Fasst das Volume ein Paar zusammen, das `Name::local_fold_key` (E8) trennt, wird das Volume abgelehnt.
   - Normalisierung: NFC gegen NFD.
   - Zeitstempel-Granularität (Linux) über `utimensat` mit krummen Nanosekunden. Das unsichere Fenster ist `max(RACY_WINDOW_NS, 2 × Granularität)`.
   - Verfügbarkeit von btime.
3. **Ablehnen.**
   - Zugelassen sind nur Dateisystem-Typen einer festen Liste (`fs_type`, §7.1): APFS (macOS); ext4, btrfs, xfs und tmpfs (Linux, Entwicklung).
   - Diese Typen beherrschen Tausch, `NoReplace` und inode-stabiles `rename`.
   - Alles andere wird abgelehnt, etwa exFAT, HFS+, SMB, NFS, FAT und overlayfs.
   - Außerdem abgelehnt werden:
     - Ordner mit `.SynologyWorkingDirectory`, weil Synology Drive und xlrx nie denselben Ordner nutzen dürfen (PLAN 4.1);
     - Ordner unter `~/Library/Mobile Documents` oder `~/Library/CloudStorage`;
     - Schreibtisch oder Dokumente bei aktivem „iCloud Schreibtisch & Dokumente“;
     - die Wurzel eines Volumes. Sonst lägen `.Trashes`, `.Spotlight-V100` und `.fseventsd` im Ordner, und Verschiebungen in den Finder-Papierkorb würden einfrieren statt löschen.
   - Standard-Ordner ist `~/xlrx`; er ist nicht TCC-geschützt.
4. **`CLIENT_DIR` und `ignored()`.**
   - `CLIENT_DIR` steht in `proto::name::ignored()`. Der Server legt deshalb nie einen solchen Knoten an.
   - Der Scanner lässt `CLIENT_DIR` samt Inhalt weg (§7.3).
   - Ein verknüpftes Objekt, das der Nutzer dorthin verschiebt, fehlt im Scan und gilt als gelöscht.

#### 7.3 Identität, Namen, opake Objekte

**`LocalId`:**
- macOS: APFS-Datei-ID (`st_ino`). Dass sie nicht wiederverwendet wird, prüft die macOS-CI.
- Linux: `splitmix64(ino ^ btime_ns.rotate_left(32))`.
  - So bekommt ein schnell wiederverwendeter ext4-Inode eine neue Identität.
  - Ohne btime fällt die Berechnung auf `ino` zurück. Gleichartige Wiederverwendung wirkt dann wie Verschieben plus Ändern, was sicher ist.
- Nie `GONE`.
- Doppelte IDs in einem Scan machen beide Objekte opak.
- Der Executor prüft die Identität immer durch Neuberechnung aus `stat`.

**Namen:**
- Pro `LocalId` hält der `LocalIndex` den rohen `OsString`. Die Engine bekommt `Name` in NFC.
- Neue Namen schreibt der Client immer in NFC.
- Vergleiche in den Vorbedingungen laufen als `NFC(roh) == erwartet`.
- Synchronisierbar ist ein Name, wenn `proto::name::syncable(raw: &OsStr) = raw.to_str().is_some_and(|s| Name::new(s).is_ok())` gilt.
  - Das ist genau das, was der Server-Scanner als Knoten zulässt.
  - Führende oder abschließende Leerzeichen und Steuerzeichen sind damit synchronisierbar.
  - Die Sync-Ops des Servers übernehmen solche Namen exakt, ohne Kürzen (S1b, §11).

**Ganz weggelassen** werden nur:
- `CLIENT_DIR` samt Inhalt.
- *Dateien* mit Namen aus `ignored()`, deren `LocalId` in S nicht verknüpft ist.
  - Für diese Prüfung bekommt der Scanner vom Treiber die Menge der verknüpften `LocalId`s.
  - Ausnahme: `.xlrx-dl-*` wird gemeldet. Die Engine ignoriert diese Namen bereits (`engine.rs:1813`, `:997-1000`).

**Opak** gemeldet werden, mit `opaque = true`, `fp` und `content = None`:
- symbolische Links, FIFOs, Sockets, Geräte;
- Dateien mit `nlink > 1`;
- Ordner auf anderem `st_dev` (Mount-Punkte);
- Einträge mit `EACCES` oder `EPERM` beim Öffnen oder `stat`;
- Einträge mit `SF_DATALESS` auf macOS (iCloud-Platzhalter);
- Namen, für die `syncable` scheitert, also kein UTF-8 oder nach NFC länger als 255 Bytes;
- zwei rohe Namen mit gleicher NFC-Form in einem Ordner (beide werden opak);
- doppelte `LocalId`;
- verknüpfte Dateien mit Namen aus `ignored()`;
- Ordner mit Namen aus `ignored()`;
- alle Nachkommen der Ordner, die durchlaufen werden (siehe Abstieg).

**Abstieg:**
- **Durchlaufen** werden lesbare opake Ordner (unzulässiger Name, NFC-Zwilling, doppelte `LocalId`) und Ordner mit Namen aus `ignored()`.
  - Alle Nachkommen werden mit `opaque = true` gemeldet.
  - So bleibt jede verknüpfte Identität im Scan und friert über E6 (a) ein, auch wenn der Nutzer sie dorthin verschiebt oder den Ordner so umbenennt.
- **Nicht durchlaufen** werden symbolische Links und Mount-Punkte. Dorthin kann keine verknüpfte Identität verschwinden: `rename` über Gerätegrenzen scheitert mit `EXDEV`, und das Ziel eines Links liegt außerhalb des Baums.
- **Nicht durchlaufbare Ordner** (`EACCES`/`EPERM`, `SF_DATALESS`) werden opak gemeldet und zusätzlich in der Menge `blind` des Schnappschusses geführt.
  - Solange `blind` nicht leer ist, hält der Treiber jede `RemoteOp::Delete*` über den `DeleteGuard` an.
  - Das Problem lautet: „Ordner X nicht lesbar, N Löschungen angehalten“.
  - Dafür ist keine neue Engine-Regel nötig.

**Geprüft durch** Sim und Dateisystem-Tests:
- Fälle:
  - verknüpfte Datei bzw. verknüpfter Ordner in einen opaken oder ignorierten Ordner verschoben;
  - verknüpfter Ordner in `#recycle` bzw. `@tmp` umbenannt;
  - Elternordner auf dem Server gelöscht, lokal mit ignoriertem Unterordner.
- Orakel:
  - keine Server-Löschung, solange die Identität unter der Wurzel außerhalb von `CLIENT_DIR` existiert;
  - nie ein Nutzerverzeichnis als `junk` im Papierkorb.

**Übergang bis M5.5:**
- Solange E6 fehlt, bricht ein Scan mit opaken Objekten als `ScanAbort::Unsupported(pfade)` ab, und der Ordner pausiert.
- Opake Objekte werden nie weggelassen.

#### 7.4 Scannen

- **Voller Scan:** paralleles `read_dir` und `stat_at(NOFOLLOW)` ab dem Wurzel-fd.
  - Die Zahl offener fds ist begrenzt (Semaphor). Beim Start wird `RLIMIT_NOFILE` angehoben.
  - Inhalte kommen aus dem Hash-Cache, wenn die Regel aus §6 Punkt 7 einen Treffer erlaubt.
  - Sonst wird mit `Chunker::digest_reader` auf einem `NOFOLLOW`-fd gehasht, mit `fstat` davor und danach. Das ist dasselbe wie `digest_file`, folgt aber keinem symbolischen Link.
  - Ändert sich die Datei während des Lesens, wird `content: None` gemeldet. Die Engine wartet dann (`engine.rs:1829-1831`).
- **Fehlerklassen:**
  - Jeder Fehler beim Lesen eines Ordners bricht den vollen Scan mit `ScanAbort::Io` ab. Es wird nichts geliefert. Beispiele: `EMFILE`, `EIO`, `ESTALE`, `ENOTDIR`, `ELOOP`.
  - Ausnahme 1: `EACCES`/`EPERM` ergibt einen opaken Eintrag und gegebenenfalls einen Eintrag in `blind` (§7.3).
  - Ausnahme 2: `ENOENT` für einen Eintrag, dessen Verschwinden ein erneutes `read_dir` des Elternordners bestätigt.
- **Ruhefrist (PLAN 5.3):**
  - Dateien, die in den letzten 3 s geändert wurden, werden mit `content: None` gemeldet.
  - Für `.sqlite`, `.db`, `.lrcat` und `-wal`/`-journal` gelten 30 s.
  - Ein Rescan wird auf das Fristende gelegt.
- **Nachprüfung fehlender Identitäten** nach einem vollen Scan. Ein voller Scan ist nicht atomar; während er läuft, kann der Nutzer verschieben.
  - Jeder Ordner wird beim Lesen mit `(ino, mtime, ctime)` gestempelt.
  - Vor der Nachprüfung werden die Ereignisse geleert:
    - Linux: die inotify-Queue leer lesen.
    - macOS: über den Rückruf `FsEventsControl::flush(folder)` in die Swift-`FSEventsSource`, der `FSEventStreamFlushSync` aufruft (§12).
  - Erneut gelesen werden:
    - Ordner mit Ereignissen während des Scans;
    - Ordner, deren Stempel sich seit dem Lesen geändert hat oder deren mtime im unsicheren Fenster des Lesezeitpunkts liegt;
    - der letzte bekannte Elternordner jeder fehlenden verknüpften `LocalId`.
  - Dabei neu gefundene Ordner werden **rekursiv** gelesen.
  - Das wiederholt sich, bis der Durchgang stabil ist, höchstens dreimal. Nicht stabil heißt: Ereignisse, abweichende Stempel oder gescheiterte Watches (Linux, Watch-Limits). Ohne Ereignisquelle (vor M5.6, macOS-CLI ohne Swift, §7.5) entscheiden allein die Ordner-Stempel.
  - Geliefert wird ein Schnappschuss nur in zwei Fällen: Der Durchgang war stabil, oder seine fehlenden Identitäten fehlten schon im vorigen vollen Scan. Sonst folgt ein weiterer voller Scan.
  - Nach drei instabilen Scans wird geliefert. Der `DeleteGuard` hält dann alle daraus folgenden Server-Löschungen an und fragt nach, unabhängig von der Schwelle.
  - Grund für diesen Aufwand: Eine Server-Löschung kostet mehr als Durchsatz.
    - Versionen, Freigaben, öffentliche Links und KI-Daten hängen an der `NodeId`.
    - Sie bleiben am Knoten im Papierkorb, und der wird nach 30 Tagen geleert.
    - Der erneute Upload ist ein neuer Knoten.
  - Restrisiko: eine Verschiebung im letzten stabilen Durchgang, die keinen Stempel ändert (Risiken).
  - Geprüft durch:
    - den Sim-Modus „nicht-atomarer voller Scan“ mit Verschiebungen zwischen dem Lesen zweier Ordner;
    - den Dateisystem-Test `verschieben_waehrend_vollem_scan` mit einem Scanner-Haken zwischen zwei Ordner-Lesevorgängen.
- **Teil-Rescans** der schmutzigen Ordner liefern `DirListing` plus die neu abgefragten Vorfahren bis zur Wurzel.
  - `local_changes` liefert Upserts für jeden Eintrag, der von L abweicht, samt abweichender Vorfahren (Vertrag von `on_local_changes`, ADR 0001 §5).
  - Bleibt eine bekannte ID nirgends gefunden, obwohl ihr L-Elternteil gelesen wurde, ist sie *verschwunden*. Das führt zu `NeedFullScan`, und es gibt keine Entfernungen (ADR 0001 §5: „Als gelöscht wird nur gemeldet, was es nicht mehr gibt“).
  - Bekannte Folge: Jeder Atomic Save löst einen vollen Scan ohne Hashing aus, nur mit `stat`, Hash-Cache und Entprellung (300 ms Ruhe, höchstens 2 s, M5.6).
  - Die Kosten werden bei N Dateien gemessen, N = Größe des realen Ordners:
    - in M5.1 mit `examples/scale <N>` und mit einem reinen `stat`-Lauf über N Dateien auf APFS (macOS-CI);
    - in M5.6 mit `live_speichern_a_download_b_unter_2s`.
  - Ein Abwesenheits-Nachweis (`/.vol` + `F_GETPATH`) gehört nicht zu M5; I3 bleibt ohne Ausnahme. Ein solcher Nachweis wäre ein eigener, späterer ADR-Nachtrag, und nur, wenn die Messung ihn verlangt.

#### 7.5 Ereignisse

Ereignisse sind immer nur Hinweise.
- **Linux:** `notify` (inotify) liefert Elternordner als schmutzig.
  - `Flag::Rescan` (`IN_Q_OVERFLOW`, `inotify.rs:212-213`) führt zum vollen Scan.
  - Fehler wegen Watch-Limits führen zu periodischen vollen Scans. Ohne Watches gilt jeder volle Scan als nicht stabil (§7.4).
  - Die Cookie-Paarung (`:232-265`) wird nur zur Entprellung genutzt, **nie** als Nachweis einer Entfernung.
- **macOS:** Ereignisse kommen nur aus der Swift-`FSEventsSource` über `on_fs_events`.
  - Die Abbildung der Flags ist Rust-Code und auf Linux mit künstlichen Ereignissen getestet.
  - `MustScanSubDirs`, `UserDropped`, `KernelDropped`, `EventIdsWrapped`, `RootChanged`, `Mount` und `Unmount` führen zum vollen Scan bzw. zur Prüfung der Wurzel.
  - Für die Nachprüfung (§7.4) leert Rust die Quelle über `FsEventsControl::flush(folder)`.
  - Der FSEvents-Zweig von notify wird nicht eingesetzt:
    - Er panikt im `extern "C"`-Callback bei Pfaden, die kein UTF-8 sind, und bei unbekannten Flags (`fsevent.rs:541`, `:546`). Das bricht den Prozess ab.
    - Er kennt kein `sinceWhen` (`:299`).
  - Ohne Swift, also in der macOS-CLI für CI und Entwicklung, gibt es alle 30 s periodische volle Scans ohne Hashing.
- **Immer:** voller Scan alle 6 h, nach dem Aufwachen und nach einem Netzwechsel, jeweils mit Wurzelprüfung.

#### 7.6 Executor-Protokoll je `LocalOp`

- `T` ist `.xlrx-dl-<opid>-<zufall>` im Zielordner.
- „Unsicher“ heißt: `max(mtime, ctime) > jetzt − Fenster`.
- **Vorprüfung** (vor einer eigenen Umbenennung):
  - `local_id(stat) == local`, Datei, `nlink == 1`, `fp == expect.fp` einschließlich ctime.
  - Ist die Datei unsicher, wird zusätzlich neu gehasht und mit `expect.content` verglichen.
- **Nachprüfung** (nach der *eigenen* Umbenennung):
  - `ino`, `size` und `mtime` müssen gleich `expect` sein.
  - Liegt `expect.fp.mtime_ns` im unsicheren Fenster, wird zusätzlich neu gehasht.
  - **ctime wird nie verglichen.** Tausch und Umbenennung ändern ctime bei unverändertem mtime (auf ext4 gemessen). Ein ctime-Vergleich würde jeden Replace und jedes DeleteFile zurücktauschen und endlos wiederholen.
- **Zielprüfung** (bei `CreateDir`, `Download` und `Move`):
  - Zuerst `stat_at` auf den Zielnamen. Ist er belegt (bei `Move`: von einer anderen Identität), folgt `Precondition`, ohne Inhalte zu holen.
  - Maßgeblich bleibt die Umbenennung mit `NoReplace`.
- **Ergebnis** von `Download` und `Replace`:
  - Identität und `fp` kommen aus `fstat` auf dem offenen fd von `T`.
  - Nach `sync_file` wird `(size, mtime)` versiegelt. Nach dem Umbenennen folgt erneut `fstat` auf demselben fd.
  - Weicht `size` oder `mtime` ab, meldet der Executor `Done{fp: None}` und schreibt keinen Hash-Cache-Eintrag.
  - Für das Ergebnis wird nie `stat_at` per Name verwendet.
- **Pfade auflösen:**
  - komponentenweise ab dem Wurzel-fd mit `open_dir_at(NOFOLLOW)`;
  - die Identität jeder Komponente wird gegen den `LocalIndex` geprüft;
  - weicht sie ab, ist das Ergebnis `Precondition`.

| Op | Schritte | Ergebnis |
|---|---|---|
| `CreateDir` | Eltern auflösen; Zielprüfung; `mkdir_at(nfc)`; `open_dir_at` und `fstat` | `Done{id, fp: None}`. Bei `EEXIST` (auch Schreibweisen- oder NFC-Zwilling auf APFS): `Precondition` |
| `Download` | 1. Eltern auflösen, Zielprüfung. 2. `create_excl_at(T)`; Intent `download/writing{T, temp_ino}` festschreiben. 3. In 8-MiB-Bereichen holen, schreiben und dabei hashen. Bei `size == 0` wird nichts geholt, weil `ServeFile` jeden Range auf eine leere Datei mit 416 beantwortet; `T` bleibt leer. 4. `sync_file`; `(size, mtime)` per `fstat` versiegeln. 5. Hash ≠ `content`: `T` entfernen (bewiesen per `temp_ino`); Ergebnis `Error` plus Abruf-Hinweis. 6. `rename_at(T → nfc, NoReplace)`. 7. `sync_dir`; erneut `fstat` auf dem fd; nur bei gleichem `(size, mtime)` ein Hash-Cache-Eintrag; Intent löschen. | `Done{id, fp}` aus `fstat`. Weicht `(size, mtime)` in Schritt 7 ab: `Done{id, fp: None}`. Bei `EEXIST`: `T` entfernen, `Precondition`. Nie `Done` mit anderem Inhalt, denn die Engine übernimmt `content` der Op in L und S (`apply_local_done`, `engine.rs:511-546`). |
| `Replace` | 1. Vorprüfung. 2. Neuen Inhalt wie bei Download 2–5 nach `T` schreiben (Intent `replace/prepared`). 3. Vorprüfung wiederholen. 4. Intent `swapping{orig_ino, temp_ino}` **festschreiben**. 5. `rename_at(T ⇄ name, Exchange)`. 6. Nachprüfung des Originals, jetzt unter `T`. 7. Abweichung: Trägt `name` noch `temp_ino`, zurücktauschen, den neuen Inhalt entfernen (bewiesen per `temp_ino`); Ergebnis `Precondition`. Trägt `name` nicht mehr `temp_ino` oder scheitert das Zurücktauschen, wird nicht getauscht. Das Original kommt dann sichtbar unter „<Stamm> (gerettet <Datum>)“ mit `NoReplace` und wird später hochgeladen; Ergebnis `Error`. 8. Gleich: Original mit `NoReplace` in den Papierkorb, Papierkorb-Zeile, `sync_dir`, erneut `fstat` auf dem fd des neuen Inhalts, Intent löschen. | `Done{id: neue LocalId, fp}` aus `fstat`; bei Abweichung von `(size, mtime)` `fp: None`. Das Original wird nie entfernt. |
| `Move` | 1. Beide Eltern auflösen; Quelle prüfen (Identität und `NFC(roh) == from_name`); Zielprüfung. 2. `rename_at(NoReplace)`. 3. Bei `EEXIST` gilt eine Ausnahme, wenn das Volume groß/klein- bzw. normalisierungsunempfindlich ist, die Eltern gleich sind und `stat_at(ziel)` dieselbe Identität liefert. Dann folgt `rename_at(Plain)`. Das ist ein reiner Wechsel der Schreibweise desselben Eintrags; nichts kann überschrieben werden. Es gibt keinen Umweg über `.xlrx-tmp-`-Namen außerhalb von `local_temps`. 4. `EINVAL` (Zyklus): `Precondition`. | `Done{id: local, fp}` |
| `DeleteFile` | 1. Vorprüfung. 2. Intent `delete{orig_ino, trash_name, expect}` **festschreiben**. 3. `rename_at(→ trash, NoReplace)`. 4. Nachprüfung im Papierkorb. 5. Abweichung: mit `NoReplace` zurück, bei `EEXIST` unter „(gerettet …)“; Ergebnis `Precondition`. 6. Gleich: Papierkorb-Zeile schreiben, Intent löschen. | `Done` |
| `DeleteDir` | 1. Identität prüfen. 2. `read_dir`: Sind nur noch unverknüpfte Junk-Dateien (Namen aus `ignored()`) oder unbewiesene `.xlrx-dl-*`-Dateien übrig, kommen sie in den Papierkorb (Grund `junk`). Ein Verzeichnis zählt nie als Junk. Das löst den Punkt „Ignorierte Dateien“ aus ADR 0001, Offene Punkte, auf Adapterseite. 3. `rmdir_at`. Der Kernel prüft Leere atomar; `ENOTEMPTY` ergibt `Precondition`. | `Done` |
| Upload-Quelle | `open_read_at(NOFOLLOW)` und `fstat == (local, op.fp)`; beim Lesen hashen; am Ende erneut `fstat`: gleicher fp und `Hash == op.content`. Sonst Abbruch (`DELETE /uploads/{id}`) und `SourceChanged`, aber nur wenn der Server die OpId nicht kennt. | — |

Ein Schreiber, der *nach* der Nachprüfung noch über einen offenen fd in das ausgetauschte Inode schreibt, landet im Papierkorb (30 Tage). Das ist ein dokumentiertes Restrisiko.

**Wiederkehrende lokale Fehlschläge.**
- Der Treiber zählt gleiche lokale Fehlschläge je Schlüssel. Der Schlüssel besteht aus Knoten bzw. `LocalId`, Op-Art sowie Ziel-Elternordner und -Name.
- Jede Änderung an R oder L bei diesem Schlüssel setzt den Zähler zurück.
- Ab dem 3. Fehlschlag wird die neu geplante Op **gehalten** statt ausgeführt.
  - Sie bleibt in `inflight_local`.
  - Nach einem Neustart wird sie über den Schlüssel erneut gehalten.
  - Es gilt ein Backoff von 1 min bis 6 h, und ein Problem wird angezeigt.
- Beispiele: ENOSPC, `EACCES` in einem verknüpften Ordner, ein Server-Hash, der dauerhaft nicht passt.
- Die Server-Tabelle `parked` passt hier nicht, weil lokale Ops in jeder Runde eine neue OpId bekommen.

**Wiederherstellung (`recover()`)** läuft beim Start vor dem ersten Scan, gesteuert über `intent`:

| Phase | Befund | Aktion |
|---|---|---|
| `download/writing`, `replace/prepared` | `T` trägt `temp_ino` | `T` entfernen (bewiesen eigenes) |
| `replace/swapping` | `T` trägt `temp_ino` (kein Tausch erfolgt) | `T` entfernen |
| `replace/swapping` | `T` trägt `orig_ino` (getauscht, Nachprüfung fehlt) | gegen `expect` prüfen: gleich → Papierkorb; anders → „(gerettet …)“ |
| `replace/swapping` | `T` trägt weder `temp_ino` noch `orig_ino` | Inhalt unter `T` gilt als Nutzerdaten: sichtbar als „<Stamm> (gerettet <Datum>)“ mit `NoReplace` ablegen, nie in den Papierkorb |
| `delete` | Papierkorb-Name trägt `orig_ino` | gegen `expect` prüfen: gleich → Papierkorb-Zeile; anders → mit `NoReplace` zurück, sonst „(gerettet …)“ |
| `delete` | Original noch am Platz | Intent verwerfen |
| kein Intent | `.xlrx-dl-*`-Datei | in den Papierkorb (Grund `recovered`), **nie** entfernen |
| keine Zeile | Datei im Papierkorb | übernehmen (Grund `recovered`) |

Ein Temp, das ein ausgetauschtes Original sein könnte, wird nie fortgesetzt, ergänzt oder entfernt. Download-Wiederaufnahme gibt es nur in Phase `writing` mit bewiesenem `temp_ino`. Am Ende wird immer der Hash geprüft.

**Papierkorb:**
- Ablage unter `<root>/.xlrx-client/trash/<JJJJ-MM-TT>/<opid>-<zufall>`.
  - Der ursprüngliche Name und Pfad stehen nur in der Zeile `trash.orig_path`. So kann die Umbenennung nie an `ENAMETOOLONG` scheitern.
  - Der Papierkorb liegt auf demselben Volume, denn Mount-Punkte in der Wurzel sind opak und werden nicht durchlaufen.
- Aufbewahrung **30 Tage**, wie beim Server-Papierkorb. Der Wert ist einstellbar, aber nie kürzer.
- Solange Ersetzungen angehalten sind (Wächter-Seite „Ersetzen“, §5), läuft für Einträge mit Grund `replaced` keine Frist ab.
- Für Löschungen per SMB auf dem NAS gibt es keine Server-Kopie (`scan.rs:658-687`). Der Client-Papierkorb ist dann die letzte Kopie.

**Test-Haken.** `#[cfg(any(test, feature = "test-hooks"))] trait ExecHooks { fn at(&self, step: Step) }`.
- Haken gibt es an diesen Stellen:
  - nach der Vorprüfung;
  - vor und nach dem Tausch;
  - vor und nach der Papierkorb-Umbenennung;
  - vor und nach der endgültigen Umbenennung;
  - nach dem Intent-Commit.
- Damit wird getestet:
  - gleichzeitige Schreiber;
  - Prozessabbruch an jedem Haken: Kindprozess per `current_exe()`, danach `recover()` im Elternprozess, danach ein Orakel über den Inhaltsbestand.

### 8. Netz

#### 8.1 Transport-Vertrag

```rust
pub trait Transport: Send + Sync {                 // Rust: UreqTransport; Swift: URLSessionTransport (Foreign Trait)
    /// Ein Request, eine Response. Folgt NIE Redirects, speichert/sendet keine Cookies, cacht nicht, wiederholt nicht.
    fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
    /// Blockierender Stream (SSE); Chunks gehen an die Rust-eigene Senke, false = abbrechen.
    fn stream(&self, req: HttpRequest, sink: Arc<StreamSink>) -> Result<u16, TransportError>;
}
pub struct HttpRequest  { pub method: String, pub url: String, pub headers: Vec<Header>,
                          pub body: Vec<u8> /* ≤ 8 MiB */, pub timeout_ms: u32, pub max_body: u32, pub prefer_h3: bool }
pub struct HttpResponse { pub status: u16, pub headers: Vec<Header>, pub body: Vec<u8> /* ≤ max_body */ }
pub enum TransportError { Offline, Timeout, Tls { msg: String }, TooLarge, Cancelled, Other { msg: String } }
```

- **Linux und CLI:** `UreqTransport` mit ureq 3.4.2, schon gesperrt (`Cargo.lock:4809`). Einstellungen: `max_redirects(0)` liefert die 3xx-Antwort selbst zurück, `http_status_as_error(false)`, rustls; Cookies sind ohne Feature `cookies` aus.
- **macOS:** `URLSessionTransport`.
  - `.ephemeral`, `httpShouldSetCookies = false`, `urlCache = nil`.
  - Delegate mit `didReceive data` und `willPerformHTTPRedirection` → `completionHandler(nil)`; beides ist auf Linux mit FoundationNetworking geprüft.
  - Auf macOS `assumesHTTP3Capable = prefer_h3`.
  - Synchron über einen Semaphor, nur auf Rust-Threads aufgerufen.
  - **`URLSession.bytes(for:)` wird nicht verwendet**, weil es in FoundationNetworking unter Linux fehlt.
  - Ohne Delegate leitet swift-corelibs bei einem 307 `Authorization` an einen fremden Host weiter (geprüft).
- `FaultTransport` umhüllt jeden Transport in allen Tests: Request oder Response verwerfen, Body abschneiden, verzögern, 5xx liefern.
- **Alle Übertragungen laufen in Rust in Stücken von höchstens 8 MiB.** Die FFI bleibt dadurch winzig und synchron, ohne BodyStream und ohne async.
- **Basis-URL** (`Api`, ab M5.3a):
  - `server_url` muss `https://` sein. `http://` gilt nur für Loopback (Tests).
  - Tokens gehen nur an diese Basis-URL, nie an das Ziel eines 307.

#### 8.2 Änderungen und SSE

- **Abruf:** `GET /api/sync/changes?root=<RootInfo.id>&cursor=<c>&limit=10000&check=<cursor_tag>`. `check` wird erst ab Cursor > 0 mitgeschickt.
  - Schleife, solange `more` gesetzt ist; alle Seiten zusammen in **einen** Aufruf von `on_remote_changes` (`engine.rs:247-258`, `sync.rs:44-46`).
  - `Config.remote_root = RootInfo.node_id`, eine andere Zahl als die Root-ID.
  - **404** führt zu `RootGone`: Pause, nichts wird eingespeist. In M5 wird nur „Ordner trennen, Dateien behalten“ angeboten.
  - **409 `cursor_invalid`** führt zum Neuaufbau (§9).
  - `GET /api/roots` beim Start und alle 10 min erkennt Rollen- und Rechteänderungen.
    - Viewer-Ablagen werden in M5 nicht gespiegelt.
    - Wird eine gespiegelte Ablage auf „Ansehen“ herabgestuft, pausiert der Ordner mit einem Problem, wie bei `RootGone`. Es werden keine Ops gesendet.
- **SSE:** `GET /api/sync/notify`, ein Stream pro Konto.
  - `change [{root, seq}]` stößt nach 100 ms Entprellung den Abruf des passenden Ordners an. Unbekannte Roots, etwa nur geteilte Elemente, werden ignoriert.
  - Nach jedem (Wieder-)Verbinden werden alle Ordner abgerufen, denn der Server sendet kein Initialereignis (`sync.rs:129`).
  - Lese-Timeout 45 s.
  - Wiederverbinden mit 1 s bis 60 s Backoff, **nach jeder Token-Erneuerung** und bei Netzwechsel (`on_network_changed`). Der offene Stream wird nie neu authentifiziert.
  - Solange der Stream fehlt, wird alle 60 s abgefragt.

#### 8.3 Inhalte

- **Upload:**
  - Bis 8 MiB: `PUT /api/sync/content/{hex}?size=`. Unter 256 KiB direkt; darüber erst `GET …/{hex}` (`available`).
  - Größer: `POST /api/uploads {size, target:{kind:"content"}}`, dann `PUT …/parts?offset=` (je 8 MiB, mit `x-content-sha256`), dann `POST …/commit {hash}`.
  - Die Upload-ID steht in `transfer`; Wiederaufnahme über `GET /api/uploads/{id}` (`received`).
  - Nach dem endgültigen Op-Ergebnis folgt `DELETE /api/uploads/{id}`, weil `MAX_OPEN = 200` auch abgeschlossene Uploads zählt.
  - Antwortet einer dieser Aufrufe mit 409 `disk_full` (S10), zeigt der Agent das Problem „NAS voll“ und wiederholt mit Backoff (§8.4).
- **Download:**
  - `GET /api/nodes/{id}/content?purpose=sync` mit `Range: bytes=a-b` in 8-MiB-Stücken.
  - Leere Dateien (`size == 0`) werden nicht abgerufen, sondern leer angelegt (§7.6). `ServeFile` beantwortet jeden Range auf eine leere Datei mit 416.
  - **307:**
    - `Location` mit `Range` erneut senden, **ohne** `Authorization`.
    - Die vorsignierte URL wird für weitere Bereiche wiederverwendet, bis 10 min um sind oder S3 mit 403 antwortet.
    - Inhalt immer gegen `op.content` prüfen. Bei Abweichung: `Error` plus Abruf.
  - Wiederaufnahme mit `If-Unmodified-Since: <Last-Modified>`. `If-Match` funktioniert nicht (immer 412).

#### 8.4 Ablauf einer Server-Op und Abbildung der Ergebnisse

1. Bei `Attempt::MaybeSent`: `GET /api/sync/ops/{device}/{op_id}?with_op=true`.
   - 200 mit gleicher Op: gespeichertes Ergebnis melden; die Quelldatei wird nicht gebraucht. Der Vergleich nutzt dieselbe kanonische Form wie der Server (§9).
   - 200 mit anderer Op: `RemoteOutcome::Rebuild`, also Neuaufbau des Ordners (§9).
   - 404: weiter mit Schritt 2.
   - Bei `Attempt::First` entfällt die Abfrage. `POST` ist unter `sync_lock` selbst idempotent (`sync.rs:372-376`).
   - `Attempt::First` bleibt auch nach einer zurückgespielten `state.sqlite` sicher:
     - Gleiche ID mit anderer Op ergibt 409 und damit einen Neuaufbau.
     - Gleiche ID mit gleicher Op liefert das gespeicherte Ergebnis derselben Op. Das entspricht einem verlorenen Ergebnis im Absturzmodell.
     - Bekannte harmlose Folge: Ein wiederholtes `Move` kann eine spätere Verschiebung zurückdrehen. Es geht nichts verloren.
2. Bei `CreateFile` und `Upload`: Quelle prüfen und Inhalt bereitstellen (§8.3).
3. `POST /api/sync/ops {device, op_id, op}`, zusätzlich mit `cursor` und `check` (S4).

| Antwort | Ergebnis im Treiber |
|---|---|
| 200 `RemoteResult` (nicht `Transient`) | `Final(r)` |
| 200 `"Transient"`, Netzfehler, Timeout, 5xx, 429 | `Retry`: 1 s bis 5 min mit Jitter, gleiche OpId. Ein voller NAS kommt auch als Op-Antwort 409 `disk_full` (S10) und wird wie unten behandelt. |
| 409 `content_missing` | Inhalt erneut hochladen, gleiche OpId, höchstens 3-mal, danach `Retry` |
| 409 `disk_full` (beim Bereitstellen des Inhalts, §8.3, oder auf `POST /api/sync/ops`) | `Retry` wie oben; Problem „NAS voll“ |
| 409 `op_mismatch`, 409 `cursor_invalid` | `RemoteOutcome::Rebuild`: kein Ergebnis an die Engine, Neuaufbau des Ordners (§9). Nie `Rejected`. |
| 401 `token_invalid` | Über den `TokenManager` einmal erneuern und wiederholen. Scheitert die Erneuerung mit `token_invalid`, `revoked` oder `reauth_required`, pausiert der Agent alle Ordner, und die Op bleibt in der Outbox (§10). Datenendpunkte antworten nur mit `token_invalid` (`authenticate` in `auth/device.rs`). `revoked` und `reauth_required` kommen nur vom Erneuerungs-Endpunkt. |
| 400, 403, 404, 415, 422 | **`Park`**: Die Op bleibt „in flight“ und wird nicht in jeder Runde als `Rejected` gemeldet. Sonst plante die Engine in jeder Runde dieselbe Änderung mit neuer OpId, und der Server lehnte sie jedes Mal ab. Gespeichert wird dabei nichts, weil HTTP-Fehler nie in `sync_ops` landen. Erneuter Versuch alle 30 min und nach Neustart. Ändert sich Quelle oder Knoten im Scan oder Feed, meldet der Treiber nach bestätigtem 404 der Abfrage `Rejected`, damit neu geplant wird. Bei 403 und 422 gilt das nur für einzelne Ops: Ist die ganze Ablage auf „Ansehen“ herabgestuft, pausiert der Ordner stattdessen (§8.2). |

#### 8.5 LAN

Es gibt nur Split-DNS (PLAN 5.9, `deploy/synology.md`). Zu Hause zeigt dieselbe Domain direkt auf die Caddy-IP.
- Der Client nutzt überall `server_url` und schaltet nichts um.
- Im LAN antwortet der Server ohne 307 (`mirror::outside`).
  - Kommen Geräte zu Hause per IPv6 mit öffentlichen Adressen an, gehört der Präfix des Anschlusses in `XLRX_LAN_NETS`.
  - Der Client muss mit und ohne 307 funktionieren.
- Ein Netzwechsel (`NWPathMonitor` → `on_network_changed`) stößt die SSE-Neuverbindung (§8.2) und einen vollen Scan mit Wurzelprüfung an (§7.5).
- Ein eigener LAN-Endpunkt (`drive-lan.<domain>`, PLAN 5.9) kommt erst, wenn der M0-Spike ihn verlangt. Dann gilt:
  - nur `https://` unter der eigenen Domain;
  - gültiges Zertifikat;
  - keine IP-Adresse und kein `http://`.

### 9. Eindeutige OpIds, Klon, Wiederherstellung, Server-Rücksetzung

- **`Config.device`** ist der lesbare Gerätename aus der Anmeldung, zum Beispiel „MacBook von Klaus“.
  - Schon `begin_sign_in` trimmt ihn und prüft ihn nach den Regeln von `check_device` (`sync.rs:339-344`): höchstens 100 Bytes, keine Steuerzeichen.
  - Grund: Die Anmeldung selbst (`check_name` in `auth/device.rs`) lässt 100 *Zeichen* zu. Ein dort gültiger Name könnte sonst jede Op mit 400 scheitern lassen.
  - Der Name ist pro Ordner eingefroren und erscheint in Konfliktnamen.
- **Eindeutigkeit über den ID-Raum** (ohne Engine-Änderung, `State.next_op` ist `pub`, `engine.rs:68`):
  - Jeder frische `State` bekommt `op_base = rand31() << 32` und `next_op = op_base + 1`.
  - Die Bereiche sind praktisch disjunkt. Eine Kollision fängt der Body-Vergleich ab.
  - Die Grenze von 2^63 wird eingehalten (der Server speichert `op_id as i64`).
  - Das gilt **ab dem ersten Teilschritt mit Server-Kontakt.**
- **Body-Vergleich auf dem Server (S3):**
  - Der Server speichert zu jeder `(user, device, op_id)` die Op.
  - Ein erneutes Senden mit gleicher ID, aber anderer Op, führt zu 409 `op_mismatch` statt zu einem fremden gespeicherten Ergebnis. `link_if_free` würde einem fremden `Created` blind vertrauen (`engine.rs:826-834`).
  - Verglichen wird eine kanonische, serverrelevante Form, nie rohes jsonb:
    - Der Server deserialisiert die gespeicherte Op als `RemoteOp`.
    - Er entfernt die Client-Felder `source`, `fp` und `origin` und normalisiert Namen auf NFC.
    - Dann vergleicht er mit `PartialEq`.
    - Grund: Sonst ergäbe ein später ergänztes Feld mit `#[serde(default)]` ein falsches 409, und jedes kostet einen Neuaufbau.
  - Gleiche ID mit gleicher kanonischer Op liefert das gespeicherte Ergebnis.
  - 409 `op_mismatch` oder eine Abfrage, die eine andere Op liefert, führt zu `RemoteOutcome::Rebuild`. Der Ordner wird neu aufgebaut wie bei `cursor_invalid`, nie wird `Rejected` gemeldet.
- **Klon, Neuinstallation, zurückgespielter Zustand** (Migrationsassistent, Time Machine) brauchen keine eigene Erkennung:
  - Eine Neuinstallation bekommt ein neues zufälliges `op_base`.
  - Ein Klon oder eine zurückgespielte `state.sqlite` sendet bereits benutzte IDs erneut. Der Body-Vergleich fängt jede abweichende Op ab.
  - `device_id` aus der Token-Antwort steht in `root_meta` nur zur Information. Ein Wechsel allein (Neuanmeldung) baut nicht neu auf. Der Namensraum der OpIds ist der Gerätename (Schlüssel `(user_id, device, op_id)`, `migrations/0005_sync_ops.sql`), und S bleibt gültig.
  - Die Sim prüft Neuinstallation und Torn-Tail mit beiden Zweigen: Wiederholung mit gleichem Body sowie 409 mit Neuaufbau. Orakel: Konvergenz, kein Inhaltsverlust.
- **Rücksetzung der Server-DB:**
  - `nodes.id` ist `IDENTITY` (`0002_files.sql:20`), `journal_seq` eine Sequenz (`:59`). Beide werden mit einem Backup zurückgespielt, und IDs werden danach neu vergeben.
  - Deshalb liefert der Server zu jedem Cursor > 0 einen `cursor_tag`, sonst `null`:
    - Grundlage ist die Journal-Zeile mit `seq = cursor AND root_id = root`.
    - Formel: `hex(sha256(seq_be ‖ node_id_be ‖ at_µs_be))[..16]`.
    - Die Formel ist Teil des Vertrags. Eine spätere Änderung zwänge jeden Client zum Neuaufbau.
  - Der Client schickt den Tag beim nächsten Abruf als `check` mit. Passt er nicht oder fehlt die Zeile, antwortet der Server mit 409 `cursor_invalid`.
  - Das erkennt eine Wiederherstellung auch, wenn danach schon neue Aktivität stattgefunden hat. Ein reines `head < cursor` würde das nicht.
  - Der Server prüft den Tag auch bei jeder Op (S4):
    - `POST /api/sync/ops` trägt `cursor` und `check`; die Seq braucht der Server, um die Journal-Zeile zu finden.
    - Unter `sync_lock` und noch vor dem Dedup-Lookup antwortet er bei abweichendem Tag oder fehlender Zeile mit 409 `cursor_invalid`.
    - Grund: Eine Op aus der Outbox könnte nach der Rücksetzung einen Knoten treffen, dessen ID neu vergeben wurde. `Upload` prüft auf dem Server nur, ob der Knoten lebt, eine Datei ist und `base_rev` hat (`content::write`, `Target::ReplaceOrConflict`). `CreateFile` und `CreateDir` prüfen nur, ob der Elternordner lebt und der Name frei ist.
  - Nach `open()` plant `FolderSync` erst, wenn ein Abruf mit `check` gelungen ist (§5). Vorher gibt es keine Remote-Op, keinen Lookup und kein `decline`.
  - Nach einer Rücksetzung ist meist auch der Refresh-Token unbekannt (§10: `SignedOut`). Die Neuanmeldung allein baut nicht neu auf; erkannt wird die Rücksetzung über `cursor_tag`.
  - Reaktion: Neuaufbau mit frischem Zustand und neuer Instanz (§6, mit Vorschau und Rückfrage ab der Wächter-Schwelle).
    - S wird **nie** behalten, weil sonst wiederverwendete IDs auf fremde Dateien zeigen.
    - Ausschlüsse (ab M5.10) kommen aus `folder_filter` in `account.sqlite` (§6). Sie werden nach dem vollständigen Abruf über den Pfad im neuen R aufgelöst. Nicht auflösbare Pfade werden als Problem gemeldet.
  - Ein späteres Kürzen des Journals (PLAN 5.2, „Cursor zu alt“) antwortet mit eigenem Grund `cursor_expired`, nicht mit `cursor_invalid`. Dafür bleibt `on_remote_snapshot` auf derselben DB reserviert.

### 10. Anmeldung

Die gesamte Logik liegt in Rust. Swift zeigt nur den Browser.

1. **Anmeldung beginnen.** `begin_sign_in(server_url, device_name)`:
   - prüft `server_url` nach der Regel für die Basis-URL (§8.1);
   - trimmt den Gerätenamen und prüft ihn nach `check_device` (§9).

   Danach erzeugt die Funktion:
   - Verifier: 64 Zeichen aus `[A-Za-z0-9-._~]`.
   - Challenge: `base64url_nopad(sha256)`, 43 Zeichen.
   - `state`: 32 Zeichen hex, ohne `+` oder Leerzeichen.
   - URL: `{server_url}/device?challenge=…&redirect_uri=xlrx%3A%2F%2Fauth&state=…&name=…&platform=macos|linux`, mit `+` als `%2B` kodiert (vue-router macht sonst ein Leerzeichen daraus).
2. **Rückkehr.**
   - macOS: Die App startet `ASWebAuthenticationSession(callbackURLScheme: "xlrx")` und reicht die Rückruf-URL per XPC an den Agent.
   - CLI: Die URL wird ausgegeben, und die Rückruf-URL wird eingefügt.
   - Rust dekodiert als Formular und vergleicht `state`.
3. **Tausch.** `POST /api/devices/token` als JSON. `device_id` und `confirm_until` werden persistiert; `confirm_until` wird nachsichtig geparst.
4. **`TokenManager`:**
   - Erneuert genau einmal gleichzeitig (Mutex plus Condvar). Das geschieht nur im Agent und nur, solange er `<Ablage>/agent.lock` hält.
   - Vor jeder Erneuerung liest er den Refresh-Token neu aus dem `SecretStore`.
   - `SecretStore.store(neu)` muss gelingen, **bevor** der neue Access-Token benutzt wird. Scheitert das Speichern, wird mit dem alten Token erneut erneuert; der Server akzeptiert ihn, solange das neue Paar unbenutzt ist.
   - `token_invalid` bei einer Datenanfrage: nur erneuern, wenn der fehlgeschlagene Token noch der aktuelle ist und der Zustand nicht `SignedOut` ist.
   - `reauth_required`: Browser-Ablauf mit `refresh_token = aktuell`; `device_id` bleibt.
   - `revoked` oder `token_invalid` **vom Erneuerungs-Endpunkt** (`POST /api/devices/token`). Bei `token_invalid` ist der Refresh-Token unbekannt: Das widerrufene Gerät wurde nach 60 Tagen gelöscht, oder die Server-DB wurde zurückgespielt.
     - Sofort `SignedOut`. Alle Ordner pausieren; Dateien und Outbox bleiben.
     - Der Zustand wird in `account.sqlite` gespeichert, und der Refresh-Token wird per `SecretStore.delete` entfernt. Weder Neustart noch Netzwechsel noch SSE lösen danach eine Erneuerung aus.
     - Nie erneut versuchen. Jeder Fehlversuch mit unbekanntem Refresh-Token zählt gegen den gemeinsamen IP-Zähler (`throttle::fail`, 20 in 15 min). Die Sperre trifft auch Browser-Anmeldung und Code-Tausch (`login` in `api/auth.rs`, `exchange_code` in `auth/device.rs`), also genau die Neuanmeldung.
   - Neuanmeldung: Browser-Ablauf ohne `refresh_token`. Ergebnis ist eine neue `device_id`; sie allein baut nicht neu auf (§9).
   - **429:** `Throttled` mit Backoff, nie Abmeldung (gemeinsamer IP-Zähler).
5. **Keychain** (nur im Agent):
   - Generic Password in der Data-Protection-Keychain.
   - `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, nicht synchronisierbar.
   - Service `de.xlrx.drive`, Account `<host>/<device_id>`.

### 11. Server-Änderungen in M5

Alle Änderungen außer S9 sind hier auf Linux bau- und prüfbar; S9 prüft die macOS-CI. Zuordnung zu den Teilschritten:
- S1 bis S6 und S10 in M5.1;
- S7 in M5.3a;
- S9 in M5.8.

| # | Änderung | Ort | Grund |
|---|---|---|---|
| S1 | `check_at` vergleicht NFC-Formen, beim Verschieben und beim Löschen. Test `nfd_name_verschieben_und_loeschen`. | `files/ops.rs:272-285` | NFD-Namen von SMB ergäben sonst für immer `Rejected(Moved)`. Der Server-Scanner speichert den rohen Namen (`files/scan.rs`: `db::set_location`, `db::insert_node`). Der Feed liefert ihn über `Name::new` in `entry()` als NFC (`api/sync.rs`). `check_at` wirkt in `ops::update` (Move) und `ops::trash_if` (DeleteFile, DeleteDir). |
| S1b | Sync-Ops prüfen Namen exakt mit `proto::name::valid_sync_name` (`Name::new` plus `!ignored()`): kein Kürzen, kein Steuerzeichen-Verbot. Betroffen sind auf dem Sync-Pfad `ops::mkdir`, `ops::update` und `content::write` mit `Target::New`. Browser, Uploads und Links behalten `valid_name`. Test `name_mit_leerzeichen_am_rand`. | `api/sync.rs` (`execute`), `files/ops.rs` (`valid_name`, `mkdir`, `update`), `files/content.rs` (`write`) | Der Server-Scanner nimmt jeden Namen auf, den `Name::new` zulässt, auch mit Leerzeichen am Rand oder Steuerzeichen (per SMB). Der Client muss solche Knoten ohne Umbenennen anlegen und verschieben können (§7.3, `syncable`). |
| S2 | `op_result` nimmt `sync_lock`, nach `check_device` | `api/sync.rs:393-404` | `GET` darf nicht 404 melden, während ein früheres `POST` noch läuft |
| S3 | Migration `0023_sync_ops_op.sql`: `sync_ops.op jsonb` (nullable). Gleiche `(user, device, op_id)` mit anderer Op ergibt 409 `{"reason":"op_mismatch"}`; mit gleicher Op das gespeicherte Ergebnis. Verglichen wird die kanonische Form (§9), nie rohes jsonb. `GET …/{device}/{op_id}?with_op=true` liefert `{result, op}`; das Server-Feld `with_op: Option<String>` nimmt „1“ und „true“. Test `op_body_abweichung_409`. | `api/sync.rs` (`op`, `known`, `op_result`), `migrations/` | Klon, zurückgespielter Zustand, Neuinstallation unter gleichem Gerätenamen |
| S4 | `Changes.cursor_tag`, nur für Cursor > 0, sonst `null` (Formel in §9). Geprüft wird bei `GET /sync/changes?check=` und bei `POST /api/sync/ops` mit `cursor` und `check`, dort unter `sync_lock` und vor dem Dedup-Lookup. Abweichender Tag oder fehlende Journal-Zeile ergibt 409 `{"reason":"cursor_invalid"}`. Tests `cursor_tag_erkennt_ruecksetzung`, `op_mit_veraltetem_cursor_tag_409`. | `api/sync.rs:67-113` (`changes`), `api/sync.rs` (`op`) | Rücksetzung der Server-DB erkennen, auch für Ops aus der Outbox |
| S5 | `GET /nodes/{id}/content?purpose=sync`: kein `suggest::record`, kein `mirror::fetched` | `api/files.rs:354-357` | Sync-Downloads verfälschen sonst Vorschläge und S3-Spiegelung; auch `bytes=0-` zählt heute (`:326-337`) |
| S6 | Nach `xlrx_proto::name` wandern: `ignored`, `valid_name` mit `NameRefused`, `syncable`, `valid_sync_name`, `TEMP_PREFIX`, `DOWNLOAD_TEMP_PREFIX`, `SERVER_TEMP_PREFIX` und `CLIENT_DIR`. Dazu kommt `ContentHash::from_hex`. `xlrx-sync` exportiert die Präfixe weiter. | `files/fs.rs:15-34` (nutzt heute `xlrx_sync::DOWNLOAD_TEMP_PREFIX`, `:33`), `files/ops.rs:68-80` | eine Definition; `proto` darf nicht von `sync` abhängen |
| S7 | Testkit als eigenes Crate `crates/xlrx-testkit` (`TestDb`, `Env`, Browser-Hälfte der Geräte-Anmeldung, `FakeS3`), Inhalt aus `tests/common`. `xlrx-server` und `xlrx-e2e` binden es nur unter `[dev-dependencies]` ein. Es gibt kein Feature und keine Selbst-Dev-Abhängigkeit. | `tests/common` → `crates/xlrx-testkit` | E2E ohne Kopie des Test-Gerüsts. Eine Selbst-Dev-Abhängigkeit mit Feature zöge das Testkit bei jedem Bau mit `--examples` in die Release-Binärdatei des Servers; `deploy/Dockerfile.server` baut mit `--bins --examples`. |
| S8 | entfällt: Der Server behält seine Structs, nur der Client nutzt `xlrx-wire` | — | Golden JSON aus den heutigen Server-Structs (Server-Test `wire_formen`) und der Rundlauf in `xlrx-wire` (`crates/xlrx-wire/tests/golden.rs`) halten beide Seiten gleich, ohne die getesteten Handler umzubauen |
| S9 | Der Server baut auf macOS. `ioctl_ficlone` steht nur unter `cfg(target_os = "linux")`, sonst wird kopiert. `notify` bekommt auf macOS das Feature `macos_kqueue`. | `files/store.rs:49` (`clone_file`), `crates/xlrx-server/Cargo.toml` (`notify`) | Voraussetzung für den nächtlichen Job `apple-e2e` (§14). Nicht nur `ioctl_ficlone` blockiert den Bau. Ohne `macos_kqueue` bindet notify 8.2.0 auf macOS sein FSEvents-Modul ein, das `fsevent_sys` importiert. Mit `default-features = false` fehlt diese Abhängigkeit. |
| S10 | Jede 409 der Sync-API trägt `reason`: `op_mismatch`, `cursor_invalid`, `content_missing` und `disk_full`. `disk_full` ersetzt auch den reinen Text bei ENOSPC auf `PUT /sync/content`, `…/parts` und `POST /uploads`. Neu ist `ApiError::Refused(&'static str, String)`. Auch `POST /sync/ops` antwortet bei ENOSPC aus `content::write` mit 409 `disk_full`, nicht mehr mit `Transient`. | `error.rs`, `api/sync.rs` (`missing_content`), `api/uploads.rs`, `files/content.rs` (`stage`, `write`) | Der Client unterscheidet die 409-Fälle ohne Textvergleich und kann „NAS voll“ anzeigen |
| später | `sync_ops`-Aufräumen (> 180 Tage); Chunk-Index und Chunk-Listen für Delta-Übertragung (PLAN 5.3, 5.4); Batch-Uploads (PLAN 5.3); Signal für entzogene Ablage (PLAN 5.2); mtime aus `fp` anwenden (Entscheidung des Eigentümers, Offene Punkte) | — | nicht nötig für die Korrektheit |

### 12. FFI und Swift

```rust
uniffi::setup_scaffolding!();   // xlrx-ffi; nur synchrone Foreign Traits → erzeugtes Swift baut im Swift-6-Modus

#[uniffi::export(with_foreign)] pub trait Transport: Send + Sync {
    fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
    fn stream(&self, req: HttpRequest, sink: Arc<StreamSink>) -> Result<u16, TransportError>;
}
#[derive(uniffi::Object)] pub struct StreamSink { /* Kanal zum SSE-Parser */ }
#[uniffi::export] impl StreamSink { pub fn on_data(&self, chunk: Vec<u8>) -> bool; }
#[uniffi::export(with_foreign)] pub trait SecretStore: Send + Sync {
    fn load(&self, key: String) -> Result<Option<String>, SecretError>;
    fn store(&self, key: String, value: String) -> Result<(), SecretError>;   // dauerhaft bei Rückkehr
    fn delete(&self, key: String) -> Result<(), SecretError>;
}
#[uniffi::export(with_foreign)] pub trait AgentEvents: Send + Sync { fn on_event(&self, ev: AgentEvent); } // blockiert nie
#[uniffi::export(with_foreign)] pub trait ThreadHooks: Send + Sync { fn on_thread_start(&self, role: ThreadRole); } // QoS in Swift
#[uniffi::export(with_foreign)] pub trait FsEventsControl: Send + Sync {
    fn flush(&self, folder: String);   // FSEventStreamFlushSync: kehrt erst zurück, wenn alle bis dahin angefallenen
}                                      // Ereignisse über on_fs_events eingereiht sind (Nachprüfung, §7.4)

#[derive(uniffi::Object)] pub struct XlrxAgent { inner: Arc<xlrx_client::Agent> }
#[uniffi::export] impl XlrxAgent {
    #[uniffi::constructor] pub fn open(cfg: AgentConfig, transport: Arc<dyn Transport>, secrets: Arc<dyn SecretStore>,
        events: Arc<dyn AgentEvents>, hooks: Arc<dyn ThreadHooks>, fs_events: Arc<dyn FsEventsControl>)
        -> Result<Arc<Self>, AgentError>;   // flock auf <Ablage>/agent.lock; läuft schon ein Agent: AgentError::AlreadyRunning
    pub fn begin_sign_in(&self, server_url: String, device_name: String) -> Result<SignInRequest, AgentError>;
        // server_url nur https:// (http:// nur für Loopback); Gerätename nach den check_device-Regeln (§10)
    pub fn complete_sign_in(&self, callback_url: String) -> Result<AccountInfo, AgentError>; // blockiert: nie auf dem Main Actor
    pub fn sign_out(&self) -> Result<(), AgentError>;
    pub fn remote_roots(&self) -> Result<Vec<RemoteRoot>, AgentError>;
    pub fn add_folder(&self, api_root: i64, local_path: String) -> Result<FolderInfo, AgentError>; // Volume-Probe, Marker, op_base
    pub fn remove_folder(&self, folder: String) -> Result<(), AgentError>; // Ordner trennen, Dateien bleiben; in M5 ohne keep_files
    pub fn start(&self); pub fn shutdown(&self, timeout_ms: u32);
    pub fn set_paused(&self, folder: Option<String>, paused: bool);
    pub fn decide(&self, decision: u64, answer: DecisionAnswer);
    pub fn status(&self) -> AgentStatus;                                  // aus einem Schnappschuss, blockiert nicht
    pub fn path_status(&self, paths: Vec<String>) -> Vec<PathStatus>;
    pub fn observe_dirs(&self, observer: u64, dirs: Vec<String>);
    pub fn on_fs_events(&self, folder: String, events: Vec<FsEvent>);    // nur einreihen
    pub fn on_network_changed(&self); pub fn on_wake(&self);
    pub fn report_opened(&self, path: String, at_unix_ms: i64);         // PLAN 8.1 → POST /nodes/{id}/opened, gebündelt
    pub fn web_url(&self, path: String) -> Option<String>;
}
// Records/Enums: HttpRequest, HttpResponse, Header, TransportError, FsEvent{path, flags, event_id, inode: Option<u64>},
// AgentEvent{StatusChanged, ItemsChanged{observer, paths}, Decision{…}, Auth{…}, Activity{…}}, PathStatus{path, badge},
// Badge{Synced, Syncing, Error, Excluded, Ignored, None}. Hashes kreuzen die FFI nie als JSON.
```

**Gründe für die Bauform:**
- Synchrone Foreign Traits mit einem blockierenden Rust-Worker bauen im strengen Swift-6-Modus ohne Fehler und Warnungen (geprüft).
- Die async-Glue von UniFFI 0.32.2 für Foreign Traits (`uniffiTraitInterfaceCallAsync*`) scheitert unter `-swift-version 6` mit `SendingClosureRisksDataRace`. Das zeigt sich erst bei SIL, `-typecheck` findet es nicht.
- Deshalb gibt es kein tokio und keine async FFI.
- Auch `FsEventsControl::flush` ist synchron. Rust ruft ihn auf einem Rust-Thread vor der Nachprüfung eines vollen Scans (§7.4).
- Die Rust-Threads sind:
  - ein `RootActor` je Ordner;
  - Dateisystem-Pool (2–4);
  - Netz-Pools: Metadaten 2, Uploads 3, Downloads 4;
  - Hash-Pool mit rayon (Kerne/2, ein `Chunker` mit etwa 16 MiB je Thread);
  - ein SSE-Thread und ein Watcher-Thread.
- Kanäle sind `crossbeam-channel` 0.5.17 (gesperrt). Jeder Thread ruft `ThreadHooks.on_thread_start`; Swift setzt QoS `.utility` bzw. `.userInitiated`. Rust braucht dafür kein `unsafe`.

**Swift-Pakete** (Swift 6, auf Linux mit FoundationNetworking testbar, soweit nicht `#if os(macOS)`):

```
apple/Packages/XlrxCore/      Package.swift: Linux .systemLibrary(XlrxFFI) + libxlrx_ffi.so; Apple .binaryTarget(XlrxFFI.xcframework)
  Sources/XlrxCore/Generated/xlrx_ffi.swift   eingecheckt; CI erzeugt neu und bricht bei Abweichung ab
  Sources/XlrxPlatform/       URLSessionTransport (Delegate-Streaming), FileSecretStore,
                              KeychainSecretStore, FSEventsSource (auch FsEventsControl), DarwinThreadHooks (#if os(macOS))
  Tests/                      Linux + macOS: Fake-Transport; echter Server auf localhost
apple/Packages/XlrxIPC/       versionierte Codable-DTOs; NSXPC-Glue nur #if os(macOS)
apple/Packages/XlrxUI/        SwiftUI (macOS)
apple/macOS/{App,Agent,FinderSync,Shared}   apple/Project.yml (XcodeGen)
apple/scripts/{gen-bindings.sh, build-xcframework.sh}
```

- Bindings werden im Library-Modus aus der Linux-`.so` erzeugt: `uniffi-bindgen-swift --swift-sources --headers --modulemap --xcframework`.
- Das XCFramework (arm64 + x86_64, `lipo`, `xcodebuild -create-xcframework`) entsteht **nur auf der macOS-CI**. blake3 und das gebündelte SQLite lassen sich hier nicht für Apple kreuzübersetzen.

### 13. Apple-Prozessmodell

```
XlrxDrive.app        MenuBarExtra, Onboarding, Einstellungen; LSUIElement; CFBundleURLTypes "xlrx"; KEIN Rust/DB/Token
 ├─ Contents/Library/LoginItems/XlrxAgent.app    Helfer-App-Bundle (nötig für Provisioning-Profil → keychain-access-groups);
 │                                                nicht sandboxed; linkt XlrxCore; einziger Token-Erneuerer
 ├─ Contents/Library/LaunchAgents/de.xlrx.drive.agent.plist
 │     Label de.xlrx.drive.agent · BundleProgram Contents/Library/LoginItems/XlrxAgent.app/Contents/MacOS/XlrxAgent
 │     MachServices { "<TEAMID>.de.xlrx.drive.agent": true } (App-Group-Präfix, damit der sandboxed FinderSync nachschlagen darf)
 │     KeepAlive {SuccessfulExit: false} · RunAtLoad · ProcessType Adaptive · AssociatedBundleIdentifiers [de.xlrx.drive]
 └─ Contents/PlugIns/XlrxFinderSync.appex          sandboxed; nur App Group; nur XPC
```

- **Mindestversion:** macOS 15 (PLAN 1). Das gilt als Deployment-Target für App, Agent, FinderSync und die Swift-Pakete.
- **Registrierung:** `SMAppService.agent(plistName:)`. Bei `.requiresApproval` zeigt die App eine Anleitung und ruft `openSystemSettingsLoginItems()`.
- **XPC:** Ein Einstiegspunkt `call(Data) -> Data` plus ein Rückkanal `event(Data)`. Die Peers werden per Code-Signing-Requirement auf die eigene Team-ID geprüft.
- **Updates:**
  - Die App vergleicht die Versionen über XPC und bittet den Agent um `shutdown` und `exit(0)`.
  - `KeepAlive {SuccessfulExit: false}` startet ihn danach nicht neu. Das tut es nur nach einem Absturz bzw. bei Exit ≠ 0.
  - launchd startet die neue Binärdatei bei Bedarf über `MachServices`, sobald die App (Unterbrechungs-Handler, erneuter `call`) oder ein FinderSync die Verbindung wieder aufnimmt.
  - Die App verbindet sich nach der Übergabe sofort neu.
- **FinderSync:**
  - `beginObservingDirectory` führt zu `observe_dirs`.
  - Badges kommen aus `path_status`; Änderungen werden über 250 ms gebündelt.
  - Ordner zeigen „wird synchronisiert“, solange eine Op unter ihnen läuft.
  - „Link kopieren“ öffnet den Browser, weil das Anlegen eines Links einen frischen zweiten Faktor verlangt (`links.rs:133`). Den haben Geräte nie.
  - „Teilen…“ öffnet ebenfalls den Browser, weil es die Personenauswahl dort schon gibt. Technisch ginge `POST /api/nodes/{id}/shares` auch mit Geräte-Token.
  - Auch „Versionen…“ und „Im Browser öffnen“ öffnen den Browser.

### 14. Tests und CI

| Ebene | Tests | Hier (Linux) | macOS-CI | Mac |
|---|---|---|---|---|
| Engine und Sim | bestehende Seeds; Orakel der Engine-Erweiterungen (§4); Trace-Hash-Tor; Treiber-Modus; Torn-Tail (Wiederholung mit gleichem Body sowie 409 `op_mismatch` → Neuaufbau); Neuinstallation; opake Inodes; nicht-atomarer voller Scan; Namen mit ß/ss, ς/σ und NFD-Paaren in einer neuen Variante ohne Groß-/Kleinschreibung (E8); Wächter-Entscheidungen; Rücksetzung; Orakel für Inhaltserhalt (ADR 0001, Offene Punkte „Orakel des Simulators“) | ja | — | — |
| Treiber | Unit-Tests mit Fake-Uhr: Entprellung, Barriere, Backoff, Park, Wächter-Schwellen für Löschen und Ersetzen, Neustart mit erneutem Anhalten; Proptests | ja | ja | — |
| Speicher | Proptest „Delta → Neu laden → `State ==`“; Sim-Seeds über `SqliteStore` (`run_with_store`); SIGKILL-Folter (Kindprozess committet in Schleife, 500 Abbrüche, danach `integrity_check` und „geladen == ein committetes `commit_seq`“); ENOSPC auf tmpfs mit `size=` (hier als root) führt zu Fail-Stop | ja | ja (fullfsync) | — |
| Dateisystem | jede Op mit Erfolg und jeder Vorbedingung; Hooks für gleichzeitige Schreiber; Abbruch an jedem Hook mit Wiederherstellung; NFD und NFC-Zwillinge; Inode-Wiederverwendung; opake Klassen; Wurzel weg, ersetzt oder ausgehängt (tmpfs umount); ENOSPC; Regression „Replace auf unveränderter Datei gelingt“ (kein ctime-Vergleich); Verschieben während des vollen Scans (`verschieben_waehrend_vollem_scan`, Scanner-Haken zwischen zwei Ordnern) | ext4. Rechte-Tests nur ohne root, hier übersprungen (`geteuid() == 0`). casefold-ext4 nur auf dem GitHub-Runner (`mkfs.ext4 -O casefold`, `chattr +F`, überspringen, falls nicht verfügbar; `CONFIG_UNICODE` fehlt hier). | gleiche Suite auf APFS; `hdiutil`-Abbilder: case-sensitives APFS (wird zugelassen), HFS+ und exFAT (Probe muss ablehnen); Faltungspaare der Probe (ß/SS, ẞ/ss, ς/σ, ﬁ/fi, NFD); Schreibweisen-Umbenennung; Verhalten von `RENAME_SWAP`/`RENAME_EXCL`; Schleife zur Datei-ID-Wiederverwendung | — |
| Netz und Anmeldung | gegen den echten Server: Seiten mit `limit=1`, 409 und Upload, Park bei einzelnem 403 oder 422, Herabstufung auf Ansehen pausiert, Teile-Wiederaufnahme, Range-Wiederaufnahme, 307 an FakeS3 ohne `Authorization` (außen per `X-Forwarded-For` und `trust_proxy`), SSE, Rotation, verlorene Antwort, scheiternder `SecretStore`, 429, `revoked`, `token_invalid` beim Erneuern, `reauth` | ja (Postgres: `pg_ctlcluster 16 main start`) | — (die Apple-URLSession gegen den Server prüft nachts `apple-e2e`) | — |
| E2E (`xlrx-e2e`) | `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` auf eigenem tokio-Runtime, Clients in einfachen blockierenden `#[test]`, also keine geschachtelten Runtimes. `auth/session.rs:87` liest `ConnectInfo` nur optional. Ohne ihn gilt jede Anfrage als „unbekannt“ und damit als LAN, und alle Clients teilen sich einen Drossel-Schlüssel. Deshalb startet der Test wie `main.rs` mit `into_make_service_with_connect_info`: realistische Drosselung und LAN-Erkennung. Szenarien unter „Teilschritte“. | ja; nächtlich zusätzlich Datei > 4 GiB mit Wiederaufnahme und konstantem RSS | nächtlich eine Teilmenge auf APFS (`apple-e2e`) | 50-GB-Datei, SMB-Änderungen während des Syncs, Ruhezustand mitten im Upload (Mac-Checkliste, M5.13) |
| FFI und Swift | Bindings neu erzeugen und Diff-Tor; `swift test` mit `-swift-version 6`; `URLSessionTransport` gegen Server auf localhost inklusive 307 und SSE | ja (Swift aus dem Scratchpad; nicht dauerhaft, die CI installiert selbst) | XCFramework, `swift test`, Keychain mit temporärer Keychain, FSEvents auf APFS samt `flush`; nächtlich `URLSessionTransport` auf der Apple-URLSession gegen den echten Server inklusive 307 ohne `Authorization` und SSE | — |
| Apple-Hülle | XPC-DTOs, View-Models | nur Modelle | `xcodegen`, `xcodebuild … CODE_SIGNING_ALLOWED=NO build test` | SMAppService, FinderSync, ASWebAuthenticationSession, TCC, Schlafen und Aufwachen, HTTP/3, Update-Übergabe |

**CI-Jobs:**
- **`rust`** (bestehender Job, ergänzt):
  - `cargo test --workspace` mit Postgres-Service; das umfasst die neuen Crates und `xlrx-e2e`.
  - Neu: Target `aarch64-apple-darwin` in der Toolchain-Action und `cargo clippy --target aarch64-apple-darwin -p xlrx-proto -p xlrx-sync -p xlrx-wire -p xlrx-fs -- -D warnings`; ab M5.3b zusätzlich `-p xlrx-driver`.
- **`swift-linux`** (neu, bewusst leicht; die einzige Swift-Prüfung, die auch hier läuft):
  - Swift 6.4 als Tarball mit Cache.
  - `cargo build -p xlrx-ffi`, `gen-bindings.sh`, `git diff --exit-code` (Diff-Tor der Bindings).
  - `swift test --package-path apple/Packages/XlrxCore` mit FoundationNetworking. Damit ist swift-corelibs geprüft, nicht die Apple-URLSession. Die prüft `apple-e2e`.
- **`apple`** (neu, `macos-15`, Xcode per `xcode-select` gepinnt; die erzeugten Bindings werden auch mit dieser Swift-Version gebaut):
  - `cargo test -p xlrx-fs -p xlrx-driver -p xlrx-client`, nie `--workspace`. Server-Tests und `xlrx-e2e` brauchen Postgres und laufen auf macOS nur nächtlich in `apple-e2e`.
  - Danach die `hdiutil`-Suite, `build-xcframework.sh`, `swift test`, `xcodegen` und `xcodebuild`.
  - Signieren und Notarisieren nur bei Tags.
- **`apple-e2e`** (neu, nächtlich, `macos-15`, ab M5.8; braucht S9, damit der Server auf macOS baut):
  - Postgres und pgvector per Homebrew, weil Migration `0020_ai.sql` die Erweiterung `vector` braucht.
  - Eine Teilmenge von `xlrx-e2e` läuft auf APFS über den Client: Atomic Save, Schreibweisen-Umbenennung, NFD-Namen aus dem Kernel, Ersetzen per `RENAME_SWAP`, 100k kleine Dateien und ein rsync-Sturm.
  - `swift test` prüft `URLSessionTransport` auf der Apple-URLSession gegen den echten Server, inklusive 307 ohne `Authorization` und SSE.
  - FSEvents → Rescan → Server.
- **nächtlich (Linux):** Sim-Varianten einschließlich `--driver`, opak, Wächter, Torn-Tail, nicht-atomarer voller Scan, lokale Faltung (E8) und Filter. Dazu eine Datei > 4 GiB mit Wiederaufnahme und konstantem RSS und die Messung der Speicher-Kurve.

### 15. Speicherbedarf

**Gemessen** mit `examples/scale` (M5.1, hier im Container):

| Dateien | Spitze (VmHWM) | Ruhe (VmRSS nach Freigabe der Eingaben, obere Schranke) | voller Rescan + Planung |
|---|---|---|---|
| 200 000 | 405 MB | ≤ 384 MB | 1,0 s |
| 1 000 000 | 2015 MB | ≤ 1892 MB | 5,4 s |

Das PLAN-Ziel „< 200 MB bei 1 Mio.“ (5.8) ist damit weit entfernt. Die Messung bei N, der Dateizahl des realen Spiegel-Ordners, steht noch aus, weil das NAS gerade nicht erreichbar ist.

**Entscheidung:** M5 macht dieses Ziel **nicht** zum Abnahmekriterium.
- Zuerst wird gemessen, in M5.1. `examples/scale <N>` läuft mit N = Dateizahl des realen Spiegel-Ordners, zur Einordnung zusätzlich bei 200k und 1M:
  - Spitze (VmHWM) und Ruhe (VmRSS nach Freigabe der Eingaben), beide aus `/proc/self/status`. Der Ruhewert ist nur eine obere Schranke, weil glibc freigegebenen Speicher nicht vollständig an das System zurückgibt.
  - Dauer von vollem Rescan plus Planung.
  - Die macOS-CI misst, wie lange ein reiner `stat`-Durchlauf über N Dateien auf APFS dauert.
- Nachts misst die CI die Kurve an denselben Messpunkten und bricht bei mehr als 10 % Regression ab.
- Abnahme für M5: Der reale Datenbestand des Eigentümers läuft unter einem Budget, das er festlegt (Vorschlag: 1 GB Ruhe-RSS). Die Kurve wird veröffentlicht.
- Die Speicherdiät (M5.12a) folgt nur, wenn der Ruhe-RSS bei N das Budget überschreitet. Dann kommt sie vor M5.13, weil das Budget Teil der M5-Abnahme ist. Sie umfasst:
  - Namen per `Arc<str>` zwischen R, S und L teilen;
  - `by_name`-Schlüssel ohne Kopien;
  - kompakte Einträge;
  - den ersten Abruf seitenweise über SQLite einspielen.

## Alternativen verworfen

| Alternative | Warum verworfen |
|---|---|
| Nicht unterstützte Objekte im Scan weglassen | Das wirkt wie eine lokale Löschung und wird per `plan_local_gone` auf den Server übertragen (Muster: Ordner auf externe Platte, Ersatz durch symbolischen Link). Stattdessen opak und einfrieren. |
| Ignorieren als Maske mit Räumen synchronisierter Kopien | Neue Regeln würden löschen. Stattdessen gitignore-Semantik in der Engine. |
| Entfernungen aus Teil-Rescans nach 3 s Wartezeit | Eine Heuristik ist kein Nachweis dafür, dass es ein Objekt nicht mehr gibt (ADR 0001 §5). Ein spätes Zielereignis erzeugt Server-Löschung plus Neu-Upload und verliert die Identität. |
| `/.vol`-Inode-Auflösung bzw. inotify-Cookies als Korrektheitsgrundlage | Verworfen für M5. Auf dem Mac ist das ungeprüft, und ein ungepaartes `MOVED_FROM` bei frisch angelegten, noch nicht beobachteten Ordnern ist keine Entfernung. I3 gilt ohne Ausnahme. Ein Abwesenheits-Nachweis wäre ein späterer, eigener ADR-Nachtrag mit eigenem Sim-Modell, nur wenn Messungen ihn verlangen. |
| Lösch-Tor in der Engine (`emit_remote`/`emit_local`) | Größter Eingriff in die kritischste Komponente. Treiber-Wächter plus `decline` lässt den Planer unverändert, und angehaltene Ops halten das Sicherheitsnetz ruhig. |
| Namensraum im Gerätenamen (`"Label [epoch]"`) plus Server-Feld `label` | Konfliktnamen würden verschmutzt oder es brauchte ein zusätzliches Server-Feld. Zufälliges `op_base` erreicht dasselbe ohne Änderung an Engine oder Protokoll. |
| `ns`-Spalte, Primärschlüsselwechsel `sync_ops`, Feld `head` | Schwer. Der `op`-Vergleich (409 `op_mismatch` → Neuaufbau) und `cursor_tag` decken Klon, Torn-Tail und Rücksetzung leichter ab. |
| Untergrenze über `ops-max`, Probe bei `op_base+1`, `ensure_next_op` (E4), `install_id` gegen Klon | Für die Sicherheit nicht nötig. Zufälliges `op_base` und Body-Vergleich (S3) tragen sie: Gleiche OpId mit anderem Body ergibt 409 `op_mismatch` und damit den Neuaufbau. Gleicher Body liefert das gespeicherte Ergebnis derselben Op. Gespart würde nur ein Neuaufbau im seltenen Fall einer allein zurückgespielten `state.sqlite`. |
| S über eine Server-Rücksetzung behalten (`on_remote_snapshot`) | IDs werden nach einer Wiederherstellung neu vergeben, S würde auf fremde Dateien zeigen. Stattdessen Neuaufbau. |
| Zwei Schritte über `.xlrx-tmp-`-Namen für Umbenennungen nur in der Schreibweise | Der Name steht nicht in `local_temps`. Nach einem Absturz schiebt `plan_both` ihn als Move auf den Server (`engine.rs:1485-1500`). Stattdessen einfaches `renameat` mit Identitätsnachweis. |
| `take_delta()` vor dem Commit | Ein fehlgeschlagener Commit verliert das Delta. Stattdessen Peek-then-Clear und Fail-Stop. |
| Tabelle `downloads` als einziges Kriterium bei der Wiederherstellung | Verwechselt ein ausgetauschtes Original mit einem eigenen Temp. Stattdessen Intent mit `temp_ino` und `orig_ino`. |
| Byte-genauer POSIX-Simulator (`xlrx-csim`) | Wochen Aufwand. Echte Dateisystem-Tests mit `ExecHooks`, Abbruch an Haken und der Treiber im Simulator liefern den größten Teil des Werts. |
| tokio und async Foreign Traits in der FFI | Die erzeugte Glue scheitert in Swift 6. Ein zweites Laufzeitsystem bringt keinen Nutzen. |
| `URLSession.bytes(for:)` | Fehlt unter Linux, der Swift-Transport wäre hier nicht testbar. |
| notify-FSEvents als Mac-Watcher | Panik im C-Callback bricht den Prozess ab; kein `sinceWhen`. |
| rusqlite 0.40 | Konflikt bei `links = "sqlite3"` mit sqlx 0.9 im Workspace. |
| hyper-Transport für Linux | ureq 3.4.2 ist schon gesperrt und passt zum Modell mit blockierenden Threads. |
| Wire-DTOs in `xlrx-sync` | HTTP-Formen gehören nicht in die sans-IO-Engine. Stattdessen `xlrx-wire`. |
| Eine SQLite-Datei für alle Ordner (wie in PLAN 13.1) | Ein Schreiber für alle Ordner. Pro Ordner zwei Dateien trennen Engine-Commit und Executor-Intents. |
| Agent als bloßes LaunchAgent-Werkzeug | Die `keychain-access-groups`-Berechtigung braucht ein Provisioning-Profil, und das kann nur ein Bundle einbetten. |
| Volle `xlrx-e2e`-Suite mit allen Folterszenarien aus PLAN 17.4 bei jedem Push auf der macOS-CI | Teuer: macOS-Runner haben keine Service-Container, also müssen Postgres und pgvector per Homebrew kommen. Eine 50-GB-Datei passt nicht auf gehostete Runner (etwa 14 GB SSD). Stattdessen gibt es den nächtlichen Job `apple-e2e` als Teilmenge (§14) und nächtlich auf Linux eine Datei > 4 GiB. 50 GB, SMB-Änderungen und Ruhezustand laufen über die Mac-Checkliste und M5.13. PLAN 17.4 steht als Entscheidung des Eigentümers in den Offenen Punkten. |
| Papierkorb 7 Tage | Kürzer als der Server (30 Tage). Bei Löschungen per SMB ist der Client-Papierkorb die letzte Kopie. |
| Erster Teilschritt nur als Gerüst der Apple-Pipeline | Hat keinen Sync-Wert. Die Pipeline startet stattdessen mit dem ersten Dateisystem-Code (M5.2). |

## Teilschritte

Größen: S = Tage, M = 1–3 Wochen, L = 3–6 Wochen. „Hier“ bedeutet diesen Linux-Container (Postgres vorher starten). Testnamen folgen der Konvention des Repos.

**Umsetzungsstand 2026-10-04:** M5.1 ist umgesetzt, nur die Messung bei N fehlt noch (NAS nicht erreichbar). Aus M5.2 sind `xlrx-fs`, `xlrx-client::local` (LocalId, `local.sqlite`, voller Scan, Executor mit Intents und `recover()`, Probe) und E8 umgesetzt. Offen in M5.2: der CI-Job `apple`, `inode_wiederverwendung_neue_identitaet`, `enospc_beim_download` und `wurzel_entfernt_ersetzt_ausgehaengt` (bisher nur `wurzel_fehlt_bricht_ab` und `markierung_erkennt_ersetzten_ordner`).

### M5.1 Vorarbeiten: Verträge, Server-Härtung, Messgrundlage (S–M)
- **Umfang**, in dieser Reihenfolge:
  1. **Trace-Hash-Tor** (§4) als eigener erster Commit auf dem sonst unveränderten Stand.
     - Je Variante eine Golden-Datei `crates/xlrx-sim/tests/golden/<variante>.txt`. Jede Zeile hat die Form `seed hash`, mit 1000 Seeds wie in der CI.
     - Geprüft wird im selben Lauf wie `tests/seeds.rs`. Mit `XLRX_SIM_BLESS=1` werden die Dateien neu geschrieben.
     - Der Hash ist FNV-1a 64 über jede Trace-Zeile ohne `debug_summary`. Er wird in `Sim::log` fortgeschrieben, nicht aus dem Ringpuffer genommen, und zieht nichts aus dem RNG.
  2. **`xlrx_proto::name`** (S6) mit folgendem Inhalt:
     - `ignored`, `valid_name` mit `NameRefused`, `syncable`, `valid_sync_name` und `CLIENT_DIR`;
     - die Präfixe `TEMP_PREFIX`, `DOWNLOAD_TEMP_PREFIX` und `SERVER_TEMP_PREFIX`, deren Werte wörtlich übernommen werden; `xlrx-sync` exportiert sie weiter;
     - dazu `ContentHash::from_hex`.

     Außerdem kommt **E0**: Das ungenutzte Feld `Config::max_unconfirmed_deletes` entfällt. Einziger Schwellwert ist `GuardConfig` im Treiber. ADR 0001, Offene Punkte „Massenlösch-Schutz“, wird im selben Commit angepasst.
  3. **Sim:** `SimServer` und `World` (`scenario.rs`) deduplizieren nach Gerätename statt nach Client-Index und speichern den Op-Body. Eine Body-Abweichung ist in M5.1 ein Simulator-Fehler (`SimFailure`).
  4. **Crate `xlrx-wire`** für den Client. Der Server behält seine Structs. Das Golden JSON wird vorher aus den heutigen Server-Structs erzeugt (Server-Test `wire_formen`), und `xlrx-wire` prüft es im Rundlauf (`crates/xlrx-wire/tests/golden.rs`).
  5. **Server:** S1, S1b, S2, S3 (reduziert), S4, S5 und S10 (§11). Dazu die Migration `0023_sync_ops_op.sql` (`ALTER TABLE sync_ops ADD COLUMN op jsonb`).
  6. **Messung bei N**, der Dateizahl des realen Spiegel-Ordners:
     - `examples/scale <N>` misst die Spitze (VmHWM) und die Ruhe-RSS. Die Ruhe-RSS wird nach Freigabe der Eingaben gemessen und ist nur eine obere Schranke, weil glibc freigegebenen Speicher behält.
     - Gemessen werden außerdem ein voller Rescan und die Planung.
     - Ein einmaliger, manuell gestarteter Lauf auf einem `macos-15`-Runner misst einen reinen `stat`-Durchlauf über N Dateien auf APFS; der dauerhafte Job `apple` beginnt erst in M5.2.
     - Die Speicher-Entscheidung wird festgehalten (§15).
  - Die Schritte 2 und 3 lassen die Golden-Dateien byte-gleich. Die Schritte 4 und 5 berühren Engine und Sim nicht und können parallel laufen.
- **Ergebnis:** Der Server ist bereit für einen echten Client. Die Golden-Dateien halten das Verhalten der Engine fest.
- **Abnahme:**
  - Alle bestehenden Suites sind grün.
  - Die Golden-Trace-Hashes sind nach Präfix-Umzug, E0 und `SimServer`-Umstellung byte-gleich zum Stand davor. In Altvarianten und Szenarien tritt keine Body-Abweichung auf.
  - `crates/xlrx-server/tests/wire_formen.rs` (`antworten`, `fehler`, `anfragen`) und `crates/xlrx-wire/tests/golden.rs` (`antworten_des_servers`, `anfragen_des_clients`): Golden JSON für jede Wire-Form.
  - `nfd_name_verschieben_und_loeschen`: NFD-Namen auf ext4 anlegen und scannen. Move und DeleteFile mit dem NFC-Namen aus dem Feed gelingen.
  - `name_mit_leerzeichen_am_rand`:
    - CreateDir „ Wichtig“, CreateFile „Rechnung “ und Move nach „a\u{1}b“ behalten den Namen exakt.
    - Ein per SMB angelegtes „ Wichtig“ lässt sich per Sync-Op verschieben, ohne umbenannt zu werden.
  - `op_abfrage_wartet_auf_laufende_op` (S2).
  - `op_body_abweichung_409`:
    - Eine abweichende Op ergibt 409 `op_mismatch`, und nichts wird ausgeführt.
    - Die gleiche Op ergibt das gespeicherte Ergebnis, auch wenn sich nur `source`, `fp` oder `origin` unterscheiden.
    - `?with_op=true` liefert `{result, op}`.
  - `op_mit_veraltetem_cursor_tag_409`.
  - `cursor_tag_erkennt_ruecksetzung`: Die Journal-Zeile am Cursor wird verändert bzw. entfernt, danach kommt 409 `cursor_invalid`.
  - `sync_download_zaehlt_nicht`: kein Eintrag in `access_events`.
- **Hier prüfbar:** alles außer dem APFS-Durchlauf. **Braucht Mac:** nur die macOS-CI für diese Messung.

### M5.2 Dateisystem-Adapter (M)
- **Umfang:**
  - `xlrx-fs` (§7.1).
  - `xlrx-client::local`:
    - Probe als Allowlist nach Dateisystemtyp (§7.2), Wurzel-Identität, `LocalId`-Schema, `LocalIndex`.
    - Scanner mit Opak-Klassifizierung, auch innerhalb opaker und ignorierter Ordner (§7.3), mit `blind`-Menge, Fehlerklassen (`ScanAbort::Io`, begrenzte offene fds), Ruhefrist und Nachprüfung mit Ordner-Stempeln (§7.4).
    - Executor für alle sechs Ops (§7.6).
  - Engine **E8**, vor dem ersten echten APFS-Lauf:
    - `Name::local_fold_key()` mit voller Unicode-Faltung gilt für den lokalen Baum und für `SimFs`.
    - Die Golden-Dateien der Altvarianten bleiben byte-gleich (die heutigen Namen falten mit `local_fold_key` wie mit `fold_key`). Die Paare ß/ss, ς/σ und NFD laufen in der neuen Variante `volle_unicode_faltung` (Sim-Schalter `--folding`) mit eigener Golden-Datei.
  - `local.sqlite` (Hash-Cache, Intents, Papierkorb, `SqlU64`); `recover()`; `ExecHooks`.
  - Opake Objekte führen vorerst zu `ScanAbort::Unsupported`.
  - Neuer CI-Job `apple`, zunächst nur mit Client-Tests auf APFS.
- **Ergebnis:** Ein Adapter, der ADR 0001 §4.1 erfüllt und gegen gleichzeitige Schreiber und Abstürze getestet ist.
- **Abnahme:**
  - Je Op Erfolg und jede Vorbedingung.
  - `ersetzen_mit_gleichzeitigem_schreiber_an_jedem_haken`: tauscht zurück, nichts geht verloren.
  - `loeschen_mit_schreiber_im_papierkorb`.
  - `abbruch_an_jedem_haken_dann_wiederherstellung`, mit Bestands-Orakel: Jede vorhandene Fassung liegt am Platz, im Papierkorb oder unter „(gerettet …)“.
  - `ersetzen_unveraenderte_datei_gelingt` (ctime-Falle).
  - `nfc_zwillinge_opak`, `inode_wiederverwendung_neue_identitaet`, `enospc_beim_download` (tmpfs).
  - `wurzel_entfernt_ersetzt_ausgehaengt`, `symlink_hardlink_fifo_mountpunkt_opak`.
  - `ordner_loeschen_mit_ds_store` (ADR 0001, Offene Punkte „Ignorierte Dateien“): Unverknüpfte Junk-Dateien im zu löschenden Ordner kommen in den Papierkorb (Grund `junk`). Ein Verzeichnis zählt nie als Junk.
  - `verschieben_waehrend_vollem_scan`: Ein Haken zwischen zwei Ordner-Lesevorgängen verschiebt einen verknüpften Ordner. Danach wird kein Schnappschuss geliefert, dem eine noch vorhandene verknüpfte Identität fehlt.
  - Sim-Seeds mit den Paaren ß/ss, ς/σ und NFD-Paaren (E8).
  - Probe auf casefold-ext4 und APFS mit den Paaren ß/SS, ẞ/ss, ς/σ, ﬁ/fi und NFD Ä/ä. Verschmilzt das Volume ein Paar, das `local_fold_key` trennt, wird es abgelehnt.
  - `cargo check --target aarch64-apple-darwin -p xlrx-fs`.
  - macOS-CI: dieselbe Suite auf APFS plus `hdiutil`-Abbilder. exFAT und HFS+ werden abgelehnt.
- **Hier prüfbar:** alles außer APFS, HFS+, exFAT und casefold-ext4. Rechte-Tests laufen nur in der CI (nicht als root). **Braucht Mac:** nur die macOS-CI.

### M5.3a Netz, Anmeldung, Testgerüst (S–M)
- **Umfang:**
  - `xlrx-client::net`:
    - Transport, `UreqTransport`, `FaultTransport`.
    - `Api` mit allen Endpunkten aus §8, einschließlich `with_op`, `check` sowie `cursor` und `check` bei `POST /api/sync/ops`.
    - Downloads in 8-MiB-Bereichen mit 307-Logik, PUT bis 8 MiB, Teile-Upload noch ohne Wiederaufnahme.
    - Transport-Regel: `server_url` muss `https://` sein, `http://` gilt nur für Loopback (Tests). Tokens gehen nur an diese Basis-URL, nie an das Ziel eines 307.
  - `auth` mit `FileSecretStore` (0600):
    - `begin_sign_in` trimmt den Gerätenamen und prüft ihn nach den Regeln von `check_device`: höchstens 100 Bytes, keine Steuerzeichen.
    - Antwortet der Erneuerungs-Endpunkt mit `token_invalid`, gilt sofort `SignedOut` wie bei `revoked`. Der Refresh-Token wird gelöscht, und es gibt keine weitere Erneuerung (§10).
  - Ein Prozess je Ablage:
    - `xlrx_client::Agent::open` nimmt `flock` auf `<Ablage>/agent.lock`. Ist die Sperre vergeben, endet `open` mit `AgentError::AlreadyRunning`.
    - Der `TokenManager` erneuert nur unter dieser Sperre und liest den Refresh-Token vorher neu aus dem `SecretStore`.
    - Bis M5.6 öffnen die CLI-Befehle den Agent selbst, also mit derselben Sperre, oder brechen mit einem Hinweis ab.
  - `xlrx-cli` mit `login` und `roots`.
  - Server S7: Testgerüst als eigenes Crate `crates/xlrx-testkit` mit dem Inhalt aus `tests/common`. Es ist nur Dev-Abhängigkeit von `xlrx-server` und `xlrx-e2e`, ohne Feature und ohne Selbst-Dev-Abhängigkeit.
- **Ergebnis:** Anmeldung, Token-Erneuerung und Ablagen-Liste laufen gegen den echten Server. Das Testgerüst steht als eigenes Crate bereit.
- **Abnahme:**
  - `geraet_anmelden` prüft:
    - Rotation und Wiederholung nach verlorener Antwort;
    - Speichern vor Benutzen bei scheiterndem `SecretStore`;
    - 429 ist keine Abmeldung;
    - Widerruf führt zu `SignedOut`, und Reauth behält die `device_id`;
    - ein unbekannter Refresh-Token führt zu genau einem `POST /api/devices/token`, dann zu `SignedOut` mit `auth_throttle.failures` = 1. Auch nach einem Neustart des Agents gibt es keine weitere Erneuerung.
  - `redirect_ohne_authorization` (FakeS3).
  - `zweiter_agent_startet_nicht`.
- **Hier prüfbar:** alles. **Braucht Mac:** nein.

### M5.3b Treiber-Kern, Headless-Client Ende-zu-Ende (M)
- **Umfang:**
  - `xlrx-driver::FolderSync` mit `MemoryRows`:
    - Abruf und voller Scan.
    - Planen erst, wenn nach `open()` ein Abruf mit `check` gelungen ist. Vorher gibt es keine Remote-Op und keinen Lookup.
    - Barriere.
    - Ergebnis-Abbildung (§8.4) mit Park, Retry und `Rebuild`.
    - Zufälliges `op_base` und `Attempt`.
  - Jeder Ordner-Lauf sperrt `<root>/.xlrx-client/lock`. Ist die Sperre belegt, pausiert der Ordner mit einem Problem.
  - `xlrx-cli` mit `sync --once <root> <dir>`. Jeder Lauf startet mit frischem Zustand.
  - `xlrx-e2e`.
  - Sim-Aktion „Neuinstallation“ mit neuem `op_base` und der Prüfung „OpIds je Gerät eindeutig über alle Zustände“.
- **Ergebnis:** Der erste echte Client hält zwei Ordner gegen den echten Server konvergent. Löschungen werden über Läufe hinweg bewusst noch nicht übertragen, weil ein frischer Zustand nie löscht.
- **Abnahme:**
  - `zwei_clients_konvergieren`: anlegen, ändern, verschieben, Konflikt. Dazu Schreiben im Stil von SMB ins Server-Datenverzeichnis plus `POST /roots/{id}/scan`.
  - `erstabgleich_ohne_uebertragung`, `seiten_zusammen` (`limit=1`), `fehlender_inhalt_409`.
  - Fehler-Matrix: Request oder Response an jedem Aufruf verwerfen, danach Konvergenz.
  - `neuinstallation_gleicher_name_keine_fremden_ergebnisse`.
  - `zwei_ablagen_gleicher_ordner_pausiert`.
- **Hier prüfbar:** alles. **Braucht Mac:** nein.

### M5.4 Dauerhafter Zustand und Absturzsicherheit (M)
- **Umfang:**
  - Engine E1.
  - Sim:
    - Schatten-Delta-Orakel und `run_with_store`.
    - Torn-Tail mit Rücksprung um k ∈ 1..3 Persistierungen.
    - Der `SimServer` beantwortet eine Body-Abweichung ab jetzt mit 409 statt mit einem Sim-Fehler. Die Sim baut den Client daraufhin neu auf.
    - Torn-Tail deckt beide Zweige ab: Die gleiche Op liefert das gespeicherte Ergebnis. Eine abweichende Op führt zu 409 und zum Neuaufbau.
  - `state.sqlite` und `account.sqlite` mit Commit, Laden, Fail-Stop und Gruppen-Commit.
  - Neuaufbau nur in diesen Fällen (§6):
    - bei nachgewiesener Korruption;
    - bei 409 `cursor_invalid` oder `op_mismatch`.

    Vor dem Neuaufbau gibt es eine Vorschau der Objekte ohne Gegenstück. Ab der Wächter-Schwelle folgt eine Rückfrage.
  - Transiente Ladefehler pausieren den Ordner mit Backoff. Eine neuere `schema_version` pausiert ihn ebenfalls.
  - `check_invariants` läuft beim Laden und periodisch.
  - `device_id` wird nur zur Information gespeichert. Ein Wechsel allein baut nicht neu auf.
- **Ergebnis:** Ein Client, der Neustarts, Abstürze, Klone und Rücksetzungen übersteht und Löschungen überträgt.
- **Abnahme:**
  - Sim:
    - In der CI 1000 Seeds je Variante mit Schatten-Orakel und 2000 Seeds über SQLite.
    - Nachts 1 Mio. strenge Seeds über alle neuen Varianten zusammen, mindestens 200 000 je Variante.
    - Orakel für Torn-Tail: Konvergenz, kein Inhaltsverlust.
  - Altvarianten bleiben byte-gleich (Trace-Hash-Tor).
  - Store-Proptest; SIGKILL-Folter.
  - `enospc_commit_fail_stop`: Nach einem fehlgeschlagenen Commit wird keine Op freigegeben, und das Neuladen konvergiert.
  - E2E an Absturzpunkten: nach dem Commit und vor der Ausführung, nach der Wirkung und vor dem Ergebnis, mitten im Download. Danach Konvergenz ohne Duplikate und ohne Verlust.
  - `zweimal_sync_loescht`.
  - `zurueckgespielter_zustand`: Eine ältere `state.sqlite` wird eingespielt. Die gleiche Op liefert das gespeicherte Ergebnis, eine abweichende Op führt zu 409 und zum Neuaufbau. Es entstehen keine fremden Ergebnisse.
  - `server_ruecksetzung_baut_neu_auf`: Das gilt auch mit wartenden Ops in der Outbox. Keine alte Op wirkt nach der Rücksetzung, und nichts wird lokal gelöscht.
  - `korrupte_db_baut_neu_auf`.
  - Transiente Ladefehler (z. B. BUSY, ENOSPC) pausieren ohne Neuaufbau. Ein Wechsel der `device_id` behält S. Vor einem Neuaufbau über der Schwelle wird gefragt.
- **Hier prüfbar:** alles. **Braucht Mac:** nein (fullfsync-Lauf in der macOS-CI).

### M5.5 Sicherheitsnetze – Tor vor jedem Dogfooding (M)
- **Umfang:**
  - Engine E6 (opak und einfrieren, einschließlich Kinder- und Nachbarregeln) und E5 (`decline` für Löschungen und für `Replace`), jeweils mit Sim-Varianten.
  - `DeleteGuard` (§5), persistent und mit Entscheidungen. Er zählt drei Seiten: Server-Löschungen, lokale Löschungen und lokales Ersetzen.
    - Er bewertet die Lösch-Ops eines `plan()`-Ergebnisses als Ganzes.
    - Er gibt erst frei, wenn die Seite ruhig ist.
    - Er protokolliert jede Freigabe in `delete_window_log`.
    - Er führt zusätzlich eine gleitende 24-h-Summe.
  - „Wiederherstellen“ umfasst auch die schon ausgeführten Löschungen des Fensters. Auf der Server-Seite läuft das über `POST /api/trash/{id}/restore`, Ordner vor ihren Kindern. Der Endpunkt besteht schon und nimmt Geräte-Tokens an.
  - Der Treiber hält alle Server-Löschungen an, solange der Scan `blind` meldet (§7.3) und nach drei instabilen vollen Scans (§7.4).
  - Lokale Ops, die dreimal gleich scheitern, hält der Treiber mit Backoff von 1 min bis 6 h an und zeigt ein Problem (§7.6).
  - Der Ordner pausiert in diesen Fällen:
    - bei `RootGone`; in M5 gibt es dann nur „Ordner trennen, Dateien behalten“;
    - bei Herabstufung auf „Ansehen“;
    - bei Widerruf und bei Reauth.

    Dazu kommen Prompts beim Wurzel-Schutz.
  - Der Scanner speist opake Objekte ein; `ScanAbort::Unsupported` entfällt. Weitere Punkte:
    - Probleme werden für Namen gemeldet, die nicht synchronisiert werden können.
    - Geparkte Ops sind sichtbar.
    - Der Papierkorb läuft nach 30 Tagen ab. Der Ablauf ruht, solange Ersetzungen angehalten sind.
- **Ergebnis:** Der Kern darf mit echten Daten laufen.
- **Abnahme:**
  - Nachts 1 Mio. strenge Seeds über alle neuen Varianten zusammen, mindestens 200 000 je Variante, bevor der Schritt geschlossen wird. Altvarianten bleiben byte-gleich (Trace-Hash-Tor).
  - Sim-Fälle im strengen Modus:
    - Ein verknüpftes Objekt wird opak, danach löscht der Server den Elternordner. Dazu der Spiegelfall mit lokaler Löschung.
    - „Opak und verknüpft“ mit Tausch und Umbenennung auf dem Server sowie lokaler Verschiebung des opaken Objekts. Das Orakel für I2 prüft auch lokale Ops ohne Knoten.
    - Szenarien mit ausgelassenen Orten:
      - eine verknüpfte Datei bzw. ein verknüpfter Ordner wird in einen opaken oder ignorierten Ordner verschoben;
      - ein verknüpfter Ordner wird in `#recycle` bzw. `@tmp` umbenannt;
      - der Server löscht einen Elternordner, unter dem lokal ein ignorierter Unterordner liegt.

      Orakel: keine Server-Löschung, solange die Identität unter der Wurzel außerhalb von `CLIENT_DIR` existiert, und nie ein Nutzerverzeichnis als `junk` im Papierkorb.
    - Angehaltene Replace-Ops werden zufällig abgelehnt. Orakel: Konvergenz, die lokale Fassung liegt danach auf dem Server, die verworfene Server-Fassung liegt als Version vor, kein Verlust.
    - Optional eine dauerhaft angehaltene Op. Orakel: Alle nicht abhängigen Knoten konvergieren.
  - E2E:
    - `rm_rf_1000_angehalten_ablehnen_stellt_wieder_her`: Alle 1000 Dateien sind wieder da, auch wenn der Scan die Löschung in zwei Teilmengen zerlegt. „Erlauben“ führt in den Server-Papierkorb.
    - `web_massenloeschung_angehalten_lokal`.
    - `entscheidung_ueberlebt_neustart`.
    - `langsames_troepfeln_loest_aus` (Fenster und 24-h-Summe).
    - Viele Dateien werden im Server-Datenverzeichnis überschrieben (SMB-Stil). Das Ersetzen wird angehalten. „Ablehnen“ lädt die lokalen Fassungen in dieselben Knoten hoch, und die per SMB geschriebenen Fassungen bleiben als Versionen erhalten.
    - `symlink_ersetzt_datei_server_bleibt`; `unlesbarer_ordner_eingefroren` (CI, nicht root); `mount_ueber_ordner_eingefroren` (tmpfs).
    - `wurzel_verschoben_pausiert`; `geraet_im_browser_widerrufen_dateien_bleiben`; `ablage_entzogen_404_pausiert`; `herabstufung_auf_ansehen_pausiert`.
- **Hier prüfbar:** alles außer Rechte-Tests ohne root (CI). **Braucht Mac:** nein.

### M5.6 Live-Betrieb: Watcher, SSE, Nebenläufigkeit, Treiber im Simulator (M–L)
- **Umfang:**
  - E3 (`local_changes`, gemeinsam mit der Sim).
  - inotify-Watcher; Teil-Rescans mit Eskalation; Entprellung (300 ms Ruhe, höchstens 2 s); Zeitgeber für die Ruhefrist. Vor der Nachprüfung eines vollen Scans wird die inotify-Queue leer gelesen (§7.4).
  - SSE mit Wiederverbinden und Abfrage-Ersatz.
  - Pools und Prioritäten (Metadaten vor Inhalt, klein vor groß, Ordner im Wechsel), Backoff, Neuversuch geparkter Ops, Neuplanung alle 2 s.
  - Sim:
    - Treiber-Modus mit der Prüfung „`SourceChanged` für bekannte Op“.
    - Modus „nicht-atomarer voller Scan“: Ordner werden einzeln gelesen, dazwischen gibt es zufällige Verschiebungen, Umbenennungen und Atomic Saves. Orakel: Keine Server-Löschung wird für einen Knoten freigegeben, dessen verknüpfter Inode zu diesem Zeitpunkt noch existiert.
  - `xlrx run` als Dienst mit Beispiel-Units für systemd und launchd.
    - Die anderen CLI-Befehle sprechen dann über `$XDG_RUNTIME_DIR/xlrx-drive/agent.sock` (Verzeichnis 0700) mit dem laufenden Agent.
    - `status` darf `state.sqlite` nur lesend öffnen.
- **Ergebnis:** Linux-Dogfooding gegen einen lokalen Docker-Server, ohne NAS.
- **Abnahme:**
  - Treiber-Modus und nicht-atomarer voller Scan: in der CI 1000 Seeds, nachts 1 Mio. strenge Seeds über alle neuen Varianten zusammen, mindestens 200 000 je Variante. Die Golden-Dateien ändern sich nur einmal gewollt durch E3 (§4).
  - Watcher-Tests: Umbenennungspaare, `rm -rf`, Schub durch `git checkout`, Atomic Save wie bei vim und Word. Ergebnis: **keine** Konfliktkopie, die Knoten-ID bleibt, die Version liegt auf dem Server.
  - `live_speichern_a_download_b_unter_2s` mit N Dateien, je einmal mit Speichern an Ort und mit Atomic Save. Auf Linux ist der Atomic-Save-Fall nur eine Messung.
  - `chaos_mit_neustarts` mit Seed, `FaultTransport` und Abbrüchen. Das Orakel der Sim wird auf den echten Stapel angewandt: konvergent, jeder geschriebene Inhalt noch vorhanden, nichts unter `.xlrx-` auf dem Server.
  - `cli_waehrend_run_nutzt_socket`: `xlrx roots` und `xlrx decide` laufen während `xlrx run`. Danach ist das Gerät nicht widerrufen, und `commit_seq` ist monoton.
  - Messung: Kosten der Scan-Eskalation bei 200k Dateien und bei N.
- **Hier prüfbar:** alles. **Braucht Mac:** nein.

### M5.7 Übertragungen und Netz-Feinschliff (M)
- **Umfang:**
  - Wiederaufnahme von Teile-Uploads und Bereichs-Downloads (Intent-Nachweis).
  - Wiederverwendung vorsignierter URLs.
  - Bandbreiten-Token-Bucket; adaptive Parallelität.
  - Messung der Kosten von fsync und fullfsync.
- **Abnahme:**
  - `roundtrip_100_mib`.
  - `abbruch_mitten_im_upload_bzw_download_nimmt_wieder_auf`, mit weniger als 2× der Größe übertragen.
  - `waehrend_upload_geaendert_source_changed_dann_erfolg`.
  - `staging_abgelaufen_409`.
  - Nachts (Linux): Datei > 4 GiB, Wiederaufnahme, konstanter RSS.
- **Hier prüfbar:** alles. **Braucht Mac:** nein.

### M5.8 FFI und Swift-Kern (M)
- **Umfang:**
  - `xlrx-ffi` (§12) mit eingecheckten Bindings und Diff-Tor. Dazu der Foreign Trait `FsEventsControl`: `flush(folder)` ruft `FSEventStreamFlushSync` für die Nachprüfung auf (§7.4).
  - `XlrxCore` (`URLSessionTransport`, `FileSecretStore`, `KeychainSecretStore`, `FSEventsSource`, `DarwinThreadHooks`).
  - Abbildung der FSEvents-Flags in Rust.
  - `build-xcframework.sh`.
  - CI-Jobs:
    - `swift-linux`, nur leicht: Bindings-Diff-Tor und `swift test` auf FoundationNetworking;
    - vollständiges `apple`;
    - neuer nächtlicher Job `apple-e2e` (§14).
  - Server-Änderung S9: Der Server baut auf macOS. `ioctl_ficlone` wird nur unter Linux aufgerufen, sonst wird kopiert. `notify` bekommt auf macOS das Feature `macos_kqueue`.
- **Ergebnis:** XCFramework als CI-Artefakt; ein Swift-Client, der auf Linux gegen den echten Server synchronisiert.
- **Abnahme:**
  - Linux:
    - Swift-Test für Anmeldung und vollständigen Sync gegen den E2E-Server über `URLSessionTransport`, einschließlich 307 und SSE.
    - `-swift-version 6` ohne Warnungen.
    - Unit-Tests der Flag-Abbildung.
  - macOS-CI:
    - XCFramework für arm64 und x86_64, `swift test`, Keychain-Rundlauf.
    - FSEvents auf APFS speisen Rescans.
    - Die Bindings bauen mit der gepinnten Xcode-Swift-Version.
    - `cargo check -p xlrx-server`.
  - Nachts `apple-e2e` (`macos-15`, Postgres und pgvector per Homebrew):
    - Teilmenge von `xlrx-e2e` auf APFS über den Client: Atomic Save, Schreibweisen-Umbenennung, NFD-Namen aus dem Kernel, Ersetzen per `RENAME_SWAP`, 100k kleine Dateien, rsync-Sturm.
    - `swift test` mit Apples URLSession gegen den echten Server, einschließlich 307 ohne `Authorization` und SSE.
    - FSEvents → Rescan → Server.
- **Hier prüfbar:** Linux-Teil. **Braucht Mac:** nur die macOS-CI.

### M5.9 Mac-Agent und Menüleisten-App (L)
- **Umfang:**
  - `apple/`-Baum, `Project.yml` (Mindestversion macOS 15, PLAN 1), Helfer-Bundle `XlrxAgent`, LaunchAgent-Plist, `SMAppService`, XPC (`XlrxIPC`).
  - `MenuBarExtra` mit:
    - Status, Aktivität, Konfliktkopien (PLAN 11.1) und Pause;
    - Prompts für Massenlöschung, massenhaftes Ersetzen, Neuabgleich, Probleme, Reauth und Widerruf.
  - Onboarding: `ASWebAuthenticationSession`, Standard-Ordner `~/xlrx`, Wahl der Ablagen mit Probe und Ablehnungen, Erkennung von Synology-Ordnern.
  - Einstellungen: Ordner, Bandbreite.
  - `NWPathMonitor` führt zu `on_network_changed` (SSE neu verbinden, Scan); Aufwachen führt zu `on_wake`; QoS.
- **Abnahme:**
  - macOS-CI: unsignierter Build, Tests von View-Models und XPC-DTOs (die DTOs auch auf Linux).
  - Mac-Checkliste:
    - Installation; das Login-Objekt übersteht erneutes Anmelden.
    - Anmeldung, Erstabgleich, offline und wieder online.
    - Schlafen und Aufwachen ohne laufende Übertragung (Ruhezustand mitten im Upload: Punkt 10 der Mac-Checkliste, M5.13).
    - Reauth; Widerruf im Browser führt zur Pause.
    - Prompt bei Massenlöschung.
    - Update-Übergabe: Nach `shutdown` und `exit(0)` startet der nächste XPC-Aufruf den Agent mit der neuen Binärdatei.
- **Hier prüfbar:** IPC-DTOs und Logik. **Braucht Mac:** ja, für die Schlussabnahme.

### M5.10 Selective Sync und Ignorier-Regeln (L)
- **Umfang:**
  - Engine E7 hinter einem Schalter, bis 1 Mio. strenge Seeds über alle neuen Varianten zusammen bestanden sind, mindestens 200 000 je Variante.
  - `exclusion_impact`, `forget_subtree` und „Speicher freigeben“ (PLAN 5.7).
  - Vorlage für einen Neuaufbau: Tabelle `folder_filter` in `account.sqlite` für Ausschlüsse und Ignorier-Muster (§4 E7, §6). Sie wird bei jeder Änderung durch den Nutzer mitgeschrieben und nur beim Neuaufbau gelesen.
  - `.xlrxignore`; Standard-Ausschlüsse (`.photoslibrary`, optional `.app`, Caches).
  - CLI `xlrx exclude/include`; FFI; Baum für Selective Sync in den Einstellungen (Mac).
- **Abnahme:**
  - Seeds wie oben. Altvarianten bleiben byte-gleich (Trace-Hash-Tor).
  - E2E:
    - `ausschliessen_kein_server_loeschen`, `neue_ignorier_regel_raeumt_nicht`.
    - `ausgeschlossen_wird_nicht_heruntergeladen`, `wieder_einschliessen_laedt`.
    - `speicher_freigeben_nur_synchronisiertes`.
    - `neuaufbau_behaelt_ausschluesse`: Bei korrupter `state.sqlite` wird der ausgeschlossene Teilbaum nicht heruntergeladen, und ignorierte lokale Namen werden nicht hochgeladen.
    - `server_ruecksetzung_ausschluesse_per_pfad`: Nach `cursor_invalid` werden Ausschlüsse über den Pfad neu aufgelöst. Nicht auflösbare Pfade werden als Problem gemeldet.
- **Hier prüfbar:** Engine und CLI. **Braucht Mac:** Oberfläche.

### M5.11 FinderSync und Öffnungen melden (M)
- **Umfang:**
  - FinderSync-Badges und Kontextmenü (Browser-Fallback).
  - `NSMetadataQuery` mit `kMDItemLastUsedDate` führt gebündelt zu `POST /nodes/{id}/opened` (`source: "mac"`).
- **Abnahme:**
  - Logik von `path_status`, Bündelung und Zusammenfassen auf Linux.
  - Mac: Badges aktualisieren sich in unter 1 s und funktionieren auch in Öffnen- und Sichern-Dialogen.
- **Hier prüfbar:** Logik. **Braucht Mac:** ja.

### M5.12a Speicherdiät – nur bei Bedarf (M)
- **Bedingung:** Die Messung aus M5.1 zeigt bei N eine Ruhe-RSS über dem Budget des Eigentümers (Offene Punkte). Dann kommt dieser Schritt vor M5.13, weil §15 das Budget zur M5-Abnahme macht.
- **Umfang:** Speicherdiät (§15).
- **Abnahme:**
  - Die Ruhe-RSS bei N liegt unter dem Budget; die RSS-Kurve ist veröffentlicht.
  - Altvarianten bleiben byte-gleich (Trace-Hash-Tor).
- **Hier prüfbar:** alles. **Braucht Mac:** nein.

**Nach der M5-Abnahme, nur bei Bedarf:** persistiertes L und FSEvents `sinceWhen`. Sie kommen nur, wenn Startzeit oder Aufwach-Scans bei N stören. Das weicht von PLAN 5.8 ab, und der Eigentümer entscheidet (Offene Punkte). Voraussetzung bleibt die Sim-Variante „Neustart mit veraltetem L und verlorenen Hinweisen“.

### M5.13 Verteilung und Dogfooding (M + 4 Wochen)
- **Umfang:**
  - Signieren mit Developer ID, Notarisieren (`notarytool`), DMG, Update-Weg.
  - Dogfooding in zwei getrennten Phasen:
    1. **Staging (Probebetrieb, zählt nicht zur Abnahme).**
       - `xlrx-server` läuft per `docker compose` auf einem Linux-Rechner oder in Docker Desktop auf dem Mac, mit den Daten in einem Docker-Volume.
       - Er läuft mit einer Teilkopie, also mit einem Ordner und nicht mit dem ganzen Bestand.
       - Änderungen dort erreichen weder das NAS noch Synology Drive. Im Staging-Ordner wird deshalb nicht produktiv gearbeitet.
       - Hier laufen auch die Folterfälle aus PLAN 17.4, die auf GitHub-Runnern nicht gehen: eine 50-GB-Datei, SMB-Änderungen während des Syncs, Ruhezustand mitten im Upload.
    2. **Abnahme: 4 Wochen produktiver Betrieb gegen das NAS mit echten Daten (PLAN 17.12, 19).**
       - xlrx spiegelt in einen eigenen Ordner (Standard `~/xlrx`).
       - Synology Drive läuft auf seinem eigenen Ordner als Rückfallebene weiter. Nach PLAN 4.1 ist es nie derselbe lokale Ordner.
       - Der Mac braucht Platz für beide Kopien, sonst gilt Selective Sync (M5.10).
       - Ein Datenverlust startet die Frist neu.
  - **Umstieg nach der Abnahme:**
    - Den Synology-Drive-Client abmelden und beenden.
    - `~/xlrx` bleibt der Arbeitsordner.
    - Der alte Synology-Ordner wird nach einem Vergleich gelöscht, nicht übernommen.
- **Abnahme:** PLAN 19: 4 Wochen ohne Datenverlust gegen das NAS, danach wird Synology Drive abgeschaltet. Der Eigentümer entscheidet über zwei Fragen (Offene Punkte): Host und Umfang des Staging, und ob eine Änderung an Engine (`xlrx-sync`), Treiber (`xlrx-driver`), Scanner und Executor (`xlrx-client::local`) oder `xlrx-fs` während der 4 Wochen die Frist neu startet.
- **Hier prüfbar:** nichts. **Braucht Mac:** ja, für die Abnahme auch das NAS. Das NAS ist derzeit nicht erreichbar.

**Reihenfolge und parallele Spuren:**
- M5.1 bis M5.7 sind vollständig auf Linux baubar und prüfbar und tragen den größten Teil des Korrektheitsrisikos. Die APFS-Messung aus M5.1 und die APFS-Suite aus M5.2 laufen zusätzlich in der macOS-CI.
- Korrektheitsrelevant sind außerdem:
  - M5.8: `URLSessionTransport` (307 ohne `Authorization`) und die Abbildung der FSEvents-Flags;
  - M5.10: E7;
  - M5.12a, falls sie nötig wird: Speicherdiät in der Engine.
- M5.8 kann nach M5.3a beginnen (Transport-Konformität). Für die Agent-API braucht es M5.6.
- Die Engine-Arbeit von M5.10 kann parallel zu M5.6–M5.9 laufen.
- Die 4 Wochen auf dem NAS (M5.13) beginnen erst, wenn alle korrektheitsrelevanten Schritte abgeschlossen sind. Spätere Änderungen deckt das Dogfooding nicht ab.
- Diese Punkte blockieren den Start nicht: Wiederverwendung vorsignierter URLs, Token-Bucket, adaptive Parallelität und das Melden von Öffnungen (`NSMetadataQuery`). Sie dürfen während des Dogfoodings folgen, solange sie Engine, Treiber-Zustand und Executor nicht ändern.
- Dogfooding in drei Stufen:
  1. Linux-CLI nach M5.6;
  2. Mac-App gegen Staging nach M5.9 (zählt nicht zur Abnahme);
  3. NAS in M5.13 (Abnahme).

## Risiken

| # | Risiko | Gegenmaßnahme |
|---|---|---|
| 1 | Datenverlust durch Fehler im Adapter (Tausch, Papierkorb, Wiederherstellung) | Intents mit Inode-Nachweis; nie unbewiesen entfernen; Hooks für Schreiber und Abbrüche auf ext4 **und** APFS; Papierkorb 30 Tage; Server-Versionen; Synology Drive als Rückfallebene |
| 2 | Engine-Erweiterungen (E5–E8) schwächen bewiesene Invarianten | Zuerst in der Sim. Je Teilschritt 1 Mio. strenge Seeds über alle neuen Varianten zusammen, mindestens 200 000 je Variante, vor dem Einschalten; im Treiber-Modus dieselbe Schranke. Altvarianten byte-gleich (Trace-Hash-Tor); die gewollte Änderung durch E3 wird im selben Commit begründet neu geschrieben. Neue Orakel je Erweiterung. |
| 3 | Lücken bei Ereignissen (FSEvents-Zusammenfassung, Race bei inotify-Watches, Überlauf) | Ereignisse sind nur Hinweise; verschwundene Identität führt zum vollen Scan; periodische und Aufwach-Scans |
| 4 | Kosten der Scan-Eskalation bei Atomic Saves in großen Ordnern | Scan nur mit `stat` und Hash-Cache, Entprellung. Messung bei N Dateien (Größe des realen Ordners) schon in M5.1: `examples/scale <N>`, dazu ein `stat`-Durchlauf auf APFS in der macOS-CI. Latenztest mit N Dateien in M5.6. Ein Abwesenheits-Nachweis ist nicht Teil von M5. Er wäre ein eigener ADR-Nachtrag, nur wenn die Messung es verlangt. |
| 5 | OpId-Wiederverwendung (Neuinstallation, Klon, Time Machine) | Zufälliges `op_base` je frischem Zustand; Body-Vergleich auf dem Server in kanonischer Form (S3). 409 `op_mismatch` oder eine Abfrage mit anderem `op` führt zum Neuaufbau wie bei `cursor_invalid`. Gleiche ID mit gleicher Op liefert das gespeicherte Ergebnis. Sim mit Neuinstallation und Torn-Tail (beide Zweige). Restfolge: Ein wiederholtes `Move` aus einem zurückgespielten Zustand kann eine spätere Verschiebung zurückdrehen; kein Verlust. |
| 6 | Rücksetzung der Server-DB | `cursor_tag` beim Abruf und bei `POST /api/sync/ops` (S4); nach `open()` keine Remote-Op vor einem geprüften Abruf. Neuaufbau mit frischem Zustand, ab der Wächter-Schwelle erst nach Rückfrage; Ausschlüsse werden über den Pfad übernommen. Ein unbekannter Refresh-Token führt sofort zu `SignedOut`, ohne Wiederholung (IP-Drossel). |
| 7 | Gerät durch Token-Wiederverwendung widerrufen | Ein Erneuerer, erzwungen über `agent.lock`; Refresh-Token vor jeder Erneuerung neu aus dem `SecretStore`; Speichern vor Benutzen; `ThisDeviceOnly`, nicht synchronisierbar; Fehler-Tests |
| 8 | APFS verhält sich anders als ext4 (ctime beim Tausch, `RENAME_EXCL` bei Schreibweise, NFD, volle Unicode-Faltung wie ß/ss) | Suite in der macOS-CI ab M5.2, nächtlich `apple-e2e`. Lokale Faltung `local_fold_key` als Obermenge (E8). Die Probe lässt auf dem Mac nur APFS zu. Sie lehnt ein Volume ab, das ein Namenspaar zusammenlegt, das `local_fold_key` trennt. Identitätsnachweis beim einfachen `renameat`; Mac-Checkliste. |
| 9 | Drift bei Toolchain und UniFFI (Swift 6.4 hier, älteres 6.x in Xcode) | Nur synchrone Traits; Pins `=0.32.2` und `=0.39.0`; eingecheckte Bindings mit Diff-Tor; Bau mit beiden Swift-Versionen |
| 10 | Apple-Annahmen nicht hier prüfbar (SMAppService, Mach-Lookup aus dem Sandbox-FinderSync, Keychain-Gruppe des Helfers, Neustart des Agents nach einem Update) | Helfer-Bundle, früher macOS-CI-Build, Mac-Checkliste (unten) |
| 11 | Speicherziel verfehlt | Messung bei N Dateien in M5.1; Budget statt PLAN-Ziel (§15). Speicherdiät in M5.12a nur, wenn der Ruhe-RSS bei N das Budget überschreitet, dann vor M5.13. |
| 12 | Fehlalarme des Lösch- und Ersetzen-Wächters (Branch-Wechsel mit `git`, auch auf einem anderen Gerät) | Schwellen einstellbar; „Erlauben“ setzt ein Kontingent; Fenster-Reset nach Ruhe |
| 13 | Last auf dem J3455 (eine Anfrage pro Op, volles Neu-Hashen bei `CreateFile`, Schleifen gleicher lokaler Fehlschläge) | `Attempt::First`, kleine Dateien direkt per PUT. `Download` prüft den Zielnamen vor dem Holen. Gleiche lokale Fehlschläge werden ab dem 3. angehalten (Backoff 1 min bis 6 h). Messung in M5.7; Batch und Delta nach M5. |
| 14 | Papierkorb in der Wurzel belegt Platz | Größe in der Oberfläche, „Leeren“, 30 Tage |
| 15 | Umgebung: Swift nur im Scratchpad, Tests laufen als root, Postgres gestoppt | CI installiert Swift; Rechte-Tests nur ohne root; `pg_ctlcluster 16 main start` dokumentiert |
| 16 | Verschiebung während eines vollen Scans (der Scan ist nicht atomar) | Jeder Ordner wird mit `(ino, mtime, ctime)` gestempelt. Vor der Nachprüfung werden die Ereignisse geleert (inotify-Queue, `FsEventsControl::flush`). Danach wird erneut gelesen, bis der Durchgang stabil ist, höchstens dreimal. Geliefert wird nur ein stabiler Durchgang oder einer, dessen fehlende Identitäten schon im vorigen vollen Scan fehlten. Nach drei instabilen Scans hält der `DeleteGuard` alle Server-Löschungen an, unabhängig von der Schwelle. Restrisiko: eine Verschiebung im letzten stabilen Durchgang, die keinen Stempel ändert. Folge: Löschung in den Server-Papierkorb und ein neuer Knoten. Versionen, Freigaben, Links und KI-Daten bleiben am alten Knoten, der nach 30 Tagen geleert wird. |
| 17 | Ransomware oder Massen-Überschreiben per SMB (der Server-Scan übernimmt neuen Inhalt ohne Version) | Btrfs-Snapshots und Hyper Backup (PLAN 15.1) als primäre Abwehr. Ersetzen-Wächter (§5) und Client-Papierkorb als zweite Schicht und Frühwarnung. Der Papierkorb-Ablauf ruht, solange Ersetzungen angehalten sind. |

**Mac-Checkliste für den Eigentümer** (Punkte 1–9 etwa eine Stunde, speist M5.2, M5.8 und M5.9; Punkt 10 im Dogfooding von M5.13):
1. Status von `SMAppService.agent(…).register()` und das Verhalten der Systemeinstellungen.
2. Der Sandbox-FinderSync erreicht `<TEAMID>.de.xlrx.drive.agent`.
3. Der Helfer schreibt mit seiner Gruppe in die Data-Protection-Keychain.
4. `renamex_np(RENAME_SWAP)`: ctime und mtime. `RENAME_EXCL` bei reinem Schreibweisenwechsel.
5. Nur zur Information, nicht Teil von M5: `/.vol/<dev>/<ino>` plus `F_GETPATH` für Dateien und Ordner.
6. Erweiterte FSEvents-Daten enthalten den Inode.
7. `SF_DATALESS` bei iCloud-Platzhaltern.
8. `assumesHTTP3Capable` gegen Caddy.
9. Update-Übergabe: Nach `shutdown` und `exit(0)` startet `KeepAlive {SuccessfulExit: false}` den Agent nicht neu. launchd startet die neue Binärdatei über `MachServices`, sobald die App oder ein FinderSync die Verbindung wieder aufnimmt.
10. Folterszenarien aus PLAN 17.4, die die macOS-CI nicht abdeckt: eine 50-GB-Datei, SMB-Änderungen während des Syncs, Ruhezustand mitten im Upload.

## Offene Punkte

**Entscheidungen des Eigentümers** (bis zur Entscheidung gilt jeweils der Vorschlag):
1. **Schwellen für Massenlöschungen und massenhaftes Ersetzen.**
   - Vorschlag: 200 Dateien bzw. 10 % von S, mindestens 20, für jede Zählseite (Server, lokal, Ersetzen).
   - Reset des Fensters nach 10 min Ruhe. Zusätzlich gilt eine gleitende 24-h-Summe je Seite mit vierfacher Schwelle.
   - „Wiederherstellen“ umfasst die angehaltenen und die im Fenster schon ausgeführten Löschungen:
     - Server-Seite: nicht gesendete Ops per `decline`, ausgeführte per `POST /api/trash/{id}/restore`, Ordner vor ihren Kindern.
     - Lokale Seite: bevorzugt aus dem Server-Papierkorb, sonst `decline` und Rückholen aus dem Client-Papierkorb.
   - Ablehnen beim Ersetzen behält die lokale Fassung und lädt sie hoch. Die überschriebene Server-Fassung bleibt als Version erhalten.
2. **Speicherziel für M5** (§15). Vorschlag: ein Budget für den Ruhe-RSS am realen Bestand statt „200 MB bei 1 Mio.“ (PLAN 5.8). Gemessen wird in M5.1 bei N Dateien (Größe des realen Ordners). Die Speicherdiät (M5.12a) folgt nur, wenn der Ruhe-RSS bei N das Budget überschreitet.
3. **Delta-Übertragung** (Chunk-Index für Delta-Uploads, PLAN 5.3; Chunk-Listen für Delta-Downloads, PLAN 5.4). PLAN 5.3 sieht sie für M5 vor. Vorschlag: nicht in M5, sondern nach dem Dogfooding und nur, wenn Messungen sie verlangen. PLAN 5.3 wird im Commit dieses ADR angepasst.
4. **Konfliktnamen** angleichen: Engine „Konflikt Gerät N“ (`engine.rs:2266`), Server „Konflikt – Gerät Datum“ (`content.rs:320`). Bis zur Entscheidung bleiben beide Formen.
5. **Groß-/Kleinschreibungskonflikte.** PLAN 4.5 wird an ADR 0001 §3 angepasst (der Ankömmling wird auf dem Server umbenannt). Mit E8 gilt das auch für Namen, die erst die volle Unicode-Faltung gleichsetzt, etwa „Maße.pdf“ und „Masse.pdf“.
6. **Viewer-Ablagen** sind in M5 nicht spiegelbar (Vorschlag). Wird eine gespiegelte Ablage auf „Ansehen“ herabgestuft, pausiert der Ordner mit einem Problem.
7. **Verteilung und Kennung.** Developer ID, Notarisierung und Update-Weg; der PLAN legt dafür nichts fest. Bundle-Präfix (Vorschlag `de.xlrx.drive`) und Team-ID.
8. **HFS+.** Vorschlag, so in §7.2 umgesetzt: auf dem Mac nur APFS zulassen, weil HFS+ Datei-IDs wiederverwenden kann. Die Probe prüft dafür den Dateisystem-Typ.
9. **Ruhefristen** (PLAN 5.3). Vorschlag: 3 s, für Datenbanken 30 s.
10. **mtime-Synchronisierung** (PLAN 4.3). `RemoteOp::CreateFile` und `Upload` übertragen schon `fp.mtime_ns`. Der Server ignoriert es: `api/sync.rs:446` und `:465` übergeben `None` an `content::write`. Nötig ist nur eine kleine Server-Änderung, keine Protokolländerung. Vorschlag: nach M5.
11. **Geteilte Elemente in „Meine Ablage“** (PLAN 9.1). Laut PLAN 19 (Stand zu M3) „kommt mit M5“. Vorschlag: nach M5. PLAN 19 wird im Commit dieses ADR angepasst.
12. **APNs auf dem Mac** (PLAN 8.4; PLAN 19, Stand zu M3: „mit den Apps ab M5“). Vorschlag: nach M5. PLAN 19 wird im Commit dieses ADR angepasst.
13. **Rechteentzug** (PLAN 5.2: lokal unveränderte Dateien werden entfernt). Vorschlag: Der Ordner pausiert stattdessen. In M5 gibt es nur „Ordner trennen, Dateien behalten“.
14. **Abwählen** (PLAN 5.7). Vorschlag: Abwählen räumt nicht automatisch; lokalen Platz gibt nur „Speicher freigeben“ frei. Eine Voreinstellung je Freigabe gibt es in M5 nicht.
15. **Akku- und Stromspar-Pause, Bandbreiten-Zeitpläne** (PLAN 5.8). Vorschlag: nicht in M5.
16. **Pakete wie `.rtfd` als Einheit** (PLAN 11.2). Vorschlag: nicht in M5.
17. **Transport-Tests** mit gesperrtem UDP und SSE durch Caddy (PLAN 17.7). Vorschlag: nicht in M5.
18. **Drei SQLite-Dateien** (`account.sqlite`, je Ordner `state.sqlite` und `local.sqlite`, §6) statt „eine Datei“ (PLAN 13.1). Vorschlag: wie in §6.
19. **LAN-URL mit gepinntem Server-Zertifikat** (PLAN 5.8). Vorschlag: entfällt für M5.
    - Die LAN-Direktverbindung aus PLAN 19 kommt über Split-DNS (PLAN 5.9, §8.5).
    - Einen eigenen LAN-Endpunkt gibt es nur, wenn der M0-Spike ihn verlangt. Dann gilt: nur `https://` unter der eigenen Domain mit gültigem Zertifikat.
20. **FSEvents `sinceWhen` und persistiertes L** (PLAN 5.8). Vorschlag: nach der M5-Abnahme und nur, wenn Startzeit oder Aufwach-Scans bei N stören. Das Speicherziel aus PLAN 5.8 steht unter Punkt 2.
21. **E2E auf macOS-Runnern** (PLAN 17.4). Vorschlag:
    - Ein nächtlicher Job `apple-e2e` (§14) prüft eine Teilmenge von `xlrx-e2e` auf APFS: Atomic Save, Schreibweisen-Umbenennung, NFD aus dem Kernel, `RENAME_SWAP`-Ersetzen, 100k kleine Dateien, rsync-Sturm.
    - Eine Datei > 4 GiB läuft nächtlich auf Linux.
    - 50-GB-Datei, SMB-Änderungen und Ruhezustand mitten im Upload deckt Punkt 10 der Mac-Checkliste in M5.13 ab.
22. **Konfliktkopien in der Menüleiste** (PLAN 11.1). Vorschlag: in M5.9.
23. **Staging und Abnahme-Frist** (M5.13). Vorschlag:
    - Staging per `docker compose` auf einem Linux-Rechner oder in Docker Desktop auf dem Mac, mit einem Ordner als Teilkopie. Es zählt nicht zur Abnahme.
    - Ein Datenverlust oder eine Änderung an Engine, Treiber-Zustand oder Executor während der 4 Wochen startet die Frist neu.

**Technisch offen:**
- **[Mac]** Alles aus der Mac-Checkliste (Punkt 5 nur zur Information).
- casefold-ext4 auf dem GitHub-Runner.
- Kosten von `F_FULLFSYNC` für Verzeichnisse und Dateien.
- Sehr lange Namen auf Ausweichnamen (ADR 0001, Offene Punkte).
- Der Dev-Zyklus `xlrx-server` ⇄ `xlrx-testkit` (S7) ist hier noch nicht gebaut; der erste Bau folgt in M5.3a.

## Belege (in dieser Umgebung geprüft, Stand 2026-10-04; Zeilenangaben zu Dateien des Repos beziehen sich auf den Stand vor M5.1, Commit `820b0c9`)

Geprüft in der Linux-Entwicklungsumgebung. Pfade mit `scratchpad/` sind einmalige Proben dieser Umgebung und liegen nicht im Repo.

| Aussage | Beleg |
|---|---|
| rusqlite 0.40 löst im Workspace nicht auf; 0.39.0 nutzt `libsqlite3-sys 0.37.0` | `scratchpad/m5arch/meta40.err`; `Cargo.lock:2386`, `:4024`; Registry: beide `links = "sqlite3"` |
| rusqlite-`u64` ist nur mit `fallible_uint` möglich | `rusqlite-0.39.0/src/types/to_sql.rs:272-274`, `from_sql.rs:136-137` |
| uniffi 0.32.2 unter `forbid(unsafe_code)`; synchrone Foreign Traits bauen in Swift 6 | `scratchpad/m5arch/crates/probe-ffi`, `gen/probe_ffi.swift` |
| async-Glue für Foreign Traits scheitert in Swift 6 | Probe in `scratchpad/m5apple` (2 Fehler `SendingClosureRisksDataRace`) |
| URLSession unter Linux: Delegate-Streaming und Ablehnen von Redirects; `bytes(for:)` fehlt | `scratchpad/m5arch/urls/main.swift`; Compilerfehler „no member 'bytes'“ |
| Abbildung von `renameat_with`, `fcntl_fullfsync`, `statx`-BTIME, `Dir::read_from` | `rustix-1.1.5/src/fs/at.rs:298-302`, `backend/libc/fs/types.rs:544-548`, `fs/fcntl_apple.rs:24`, `fs/statx.rs:110-111`, `backend/libc/fs/dir.rs:102` |
| notify-FSEvents: `SinceNow` fest, Panik im Callback; inotify-Überlauf wird `Rescan` | `notify-8.2.0/src/fsevent.rs:299`, `:541-546`; `inotify.rs:212-213`; Features: `default = ["macos_fsevent"]` |
| ureq 3.4.2 gesperrt, `max_redirects`, `http_status_as_error`, Cookies standardmäßig aus | `Cargo.lock:4809`; `ureq-3.4.2/src/config.rs:491`, `:554`; `Cargo.toml [features]` |
| Mutationsstellen der Engine für `StateDelta` | `engine.rs:271`, `:277`, `:286`, `:304`, `:319`, `:382`, `:586`, `:647`, `:838`, `:926-927`, `:1126-1250`, `:1819`, `:1983`, `:2122`, `:2129`, `:2265`, `:2283` |
| `op_result` ohne `sync_lock`; Dedup-Schlüssel `(user, device, op_id)`; `check_at` vergleicht roh | `api/sync.rs:366-404`; `migrations/0005_sync_ops.sql`; `files/ops.rs:272-285` |
| Server-IDs werden mit einem Backup zurückgespielt | `migrations/0002_files.sql:20` (`IDENTITY`), `:59` (`journal_seq`) |
| Server baut auf macOS heute nicht: `ioctl_ficlone` ohne cfg-Schutz; `notify` ohne `macos_kqueue` bindet `fsevent_sys` ein (S9) | `files/store.rs:49`; `crates/xlrx-server/Cargo.toml:26`; `notify-8.2.0/src/lib.rs:204-205`, `fsevent.rs:21` |
| Ein Feature über eine Selbst-Dev-Abhängigkeit gelangt bei jedem Bau mit `--examples` in den Release-Bau des Servers; deshalb ist `xlrx-testkit` ein eigenes Crate (S7) | cargo 0.98.0 (Quelltext in `scratchpad/cargosrc`): `src/cargo/ops/cargo_compile/compile_filter.rs:218-234`, `src/cargo/core/resolver/features.rs:203-206`; Bau mit `--examples`: `deploy/Dockerfile.server:21`, `.github/workflows/ci.yml:95` |
| `POST /api/trash/{id}/restore` nimmt Geräte-Token an; ein Kind ohne lebenden Elternordner kommt an die Wurzel der Ablage | `api/mod.rs:115-116`, `:165` (Router für Browser und Geräte); `api/files.rs:496`; `files/ops.rs:705-716` |
| Daten-Endpunkte antworten bei jedem Token-Fehler mit 401 `token_invalid`; ein unbekannter Refresh-Token ergibt ebenfalls `token_invalid` und zählt gegen die IP-Drossel (20 in 15 min) | `auth/device.rs:438-444`, `:321-335`; `auth/throttle.rs` (`IP_LIMIT`, `WINDOW_MINUTES`) |
| `CreateFile` und `Upload` tragen `fp.mtime_ns`; der Server übergibt `None` als mtime | `xlrx-sync/src/ops.rs` (`RemoteOp`), `xlrx-sync/src/types.rs:29-33`; `api/sync.rs:446`, `:465` |
| `Config::max_unconfirmed_deletes` wird nirgends gelesen | `xlrx-sync/src/types.rs:123-124`; sonst nur Initialisierungen in Tests, Sim und `examples/scale.rs` |
| `Synced` zählt jede Änderung in `mutations`; ein abgeleitetes `PartialEq` würde den Zähler mitvergleichen | `xlrx-sync/src/synced.rs:10-17`, `:75`, `:90`; `same_entries` `:108` |
| Der Server-Scan übernimmt per SMB überschriebenen Inhalt ohne Version | `files/scan.rs:614` (`db::set_content`), `files/db.rs:226-255` |
| APFS (case-insensitiv) faltet voll, etwa ß → ss; `Name::fold_key` hält ß und ss getrennt | linux-apfs-rw `unicode.c`, Tabelle `apfs_cf`: Eintrag nach þ ist `0x73 0x73` (Kopie `scratchpad/apfs_unicode.c:2248`); `xlrx-proto/src/name.rs:56-58` |
| Spitzen-RSS der Engine 414 868 kB bei 200 000 Dateien | `examples/scale 200000`, VmHWM über `/proc` |
| Apple-Rust-Targets hier installiert; C-Crates lassen sich nicht kreuzübersetzen | `rustup target list --installed`: `aarch64-apple-darwin`, `aarch64-apple-ios`; Review: blake3 und `cc` mit `-arch` |
| Umgebung: Tests als root, kein `CONFIG_UNICODE`, Swift nur im Scratchpad, Postgres-Cluster gestoppt | Reviews; `scratchpad/swift/swift-6.4.0-RELEASE-ubuntu24.04` |