# ADR 0001 – Sync-Engine: Drei-Bäume-Modell, sans-IO, deterministische Simulation

Status: angenommen · Stand: 2026-10-02 · Code: `crates/xlrx-sync`, `crates/xlrx-sim`

## Ziel

Der Sync muss **schnell** sein und darf **niemals Daten verlieren**. Beides wird nicht durch
Vorsicht im Einzelfall erreicht, sondern durch eine Architektur, die sich vollständig testen lässt.

## Entscheidung

### 1. Sans-IO

Die Engine (`xlrx-sync`) trifft nur Entscheidungen. Sie liest keine Dateien, spricht kein Netzwerk
und kennt keine Uhr. Eingaben sind Server-Änderungen, lokale Scans und Ergebnisse ausgeführter
Operationen; Ausgabe sind Operationen mit Vorbedingungen. Dieselbe Engine läuft im Mac-Client, in der
iOS-App und im Simulator.

### 2. Drei Bäume

| Baum | Inhalt | Quelle |
|---|---|---|
| **R** (Remote) | Server-Zustand bis zum Journal-Cursor | Server-Journal |
| **S** (Synced) | zuletzt vereinbarter Zustand je Knoten, mit lokaler Identität und Fingerprint | Engine |
| **L** (Local) | beobachteter Zustand der Platte (Inode, Fingerprint, Inhalts-Hash) | Scanner |

Für jeden Knoten werden **Ort** (Eltern + Name) und **Inhalt** getrennt drei-Wege-verglichen:
R gegen S ergibt die Server-Änderung, L gegen S die lokale Änderung.

### 3. Regeln

| Lage | Ergebnis |
|---|---|
| Nur eine Seite geändert | auf die andere Seite übertragen |
| Beide Seiten gleich geändert | nur S nachziehen |
| Inhalt beidseitig verschieden geändert | **Konfliktkopie**: lokaler Inhalt wird als neue Datei „Name (Konflikt Gerät N)“ hochgeladen, dann wird das Original heruntergeladen. Nichts wird überschrieben. |
| Ort beidseitig verschieden geändert | Server gewinnt (lokal verschieben) |
| Löschen gegen Änderung (Inhalt **oder** Ort) | **Änderung gewinnt**, auf beiden Seiten: Verknüpfung wird gelöst, das Objekt wird neu hoch- bzw. heruntergeladen. Wird zuerst geprüft, noch vor jeder Neu-Verknüpfung. |
| Ordner löschen | nur wenn leer. Kinder, die gelöscht werden, werden abgewartet. Was bleiben muss (neu, geändert, hineinverschoben), holt den Ordner zurück. |
| Gleicher Name, beide neu | gleiche Art und gleicher Inhalt → nur verknüpfen (z.B. Ersteinrichtung, Migration von Synology Drive). Sonst Konfliktkopie. |
| Name auf einer Seite belegt | wer zuletzt kommt, weicht auf einen Konfliktnamen aus |
| Groß-/Kleinschreibungsvariante auf dem Server (per SMB „a“ neben „A“), Client ohne Unterscheidung | der Ankömmling wird auf dem Server auf einen Konfliktnamen umbenannt |
| Gelöschter Ordner mit noch wegziehendem Inhalt steht einem anderen Objekt im Weg | der Ordner weicht kurz auf einen temporären Namen aus und wird gelöscht, sobald er leer ist (sonst mögliche Warte-Zyklen) |
| Verschiebung würde auf dem Server einen Zyklus erzeugen | Server gewinnt |
| Tausch-Zyklen (a↔b, Datei wird zu gleichnamigem Ordner, …) | ein Beteiligter weicht kurz auf einen temporären Namen aus (`.xlrx-tmp-<Gerät>-<n>~<Heimatname>`) |
| „Atomic Save“ (neue Datei über alte umbenannt), lokal oder auf dem Server | als Inhaltsänderung desselben Objekts erkannt, nicht als Löschen + Neu. Auf dem Server nur, wenn das lokale Objekt unverändert ist. Bei Ordnern gehen die vereinbarten Kinder auf den neuen Ordner über (gelöschter Inhalt kommt nicht zurück). |

### 4. Sicherheitsnetze gegen Datenverlust

1. **Vorbedingungen an jeder Operation.**
   - Lokales Ersetzen und Löschen nur am erwarteten Ort, bei unverändertem Fingerprint **und** unverändertem Inhalt (`Expected`, siehe 4.1).
   - Hochladen nur, wenn die Quelldatei noch genau dem gehashten Stand entspricht.
   - Auf dem Server: Upload und Löschen nur mit passender Basis-Revision, Ordner löschen nur, wenn leer, Anlegen und Verschieben nur bei freiem Namen.
   - Verschieben und Löschen auf dem Server nur, wenn der Knoten noch am erwarteten Ort liegt. Eine Verschiebung, die der Client noch nicht gesehen hat, wird so nie überschrieben oder mitgelöscht (`Reject::Moved`).
2. **Upload mit veralteter Basis** → der Server überschreibt nicht, sondern legt eine Konfliktkopie an.
3. **Idempotenz:** Server-Operationen stehen vor dem Senden in der persistierten Outbox und werden
   nach Abbruch oder Absturz mit derselben `OpId` wiederholt; der Server führt jede `OpId` nur einmal aus.
4. **Pending-Marker:** Nach einer eigenen Server-Operation gilt S als neuer als R, bis der Journal-Cursor
   die Operation erreicht hat. So wird ein veralteter Server-Stand nie als „Änderung“ missverstanden.
5. **Ausweich-Umbenennungen** werden vor dem Ausführen persistiert und zählen nie als Nutzeränderung.
6. **Sicherheitsnetz für Lebendigkeit:**
   - Wann es eingreift: Plant die Engine über mehrere Durchläufe bei frischem Zustand nichts, obwohl Unterschiede bestehen.
   - Was es tut: Es löst den Wartezyklus mit einer garantiert datensicheren Aktion auf, also umbenennen oder behalten statt löschen.
   - Status: In den Simulationsläufen wird es derzeit nicht benötigt, es ist eine Rückfallebene.
7. **Server-Versionen und Papierkorb** (siehe PLAN 4.2/4.3) bleiben als zusätzliches Netz.
8. **Verspätete Ergebnisse** (Operationen laufen im Client nebenläufig): Ein Ergebnis überschreibt nie,
   was ein späterer Scan schon gesehen hat, und wird nicht in einen unbekannten Ordner eingetragen.
9. **Widersprüchlicher lokaler Stand** (Zyklus, fehlender Vorfahr): Nichts wird entfernt (das sähe wie
   Löschen aus). Die Engine plant nichts mehr und verlangt einen vollständigen Scan (`wants_full_scan`).
10. Halbfertige Downloads (`.xlrx-dl-…`) werden nie hochgeladen; temporäre Ausweichnamen gelangen nie als
    neues Objekt auf den Server.

### 4.1 Vertrag für den Ausführenden (Client)

Die Engine kann nur Vorbedingungen formulieren; prüfen muss sie der Client, und zwar so, dass zwischen
Prüfen und Handeln nichts verloren gehen kann (TOCTOU):

- **Ersetzen** (`Replace`): neuen Inhalt vollständig in eine temporäre Datei im Zielordner schreiben und
  per Hash prüfen. Dann atomar mit dem Original **tauschen** (`renamex_np(RENAME_SWAP)` auf APFS). Danach
  das ausgetauschte Original erneut prüfen: Hat es sich zwischen Prüfung und Tausch geändert, zurücktauschen
  (bzw. es als Konfliktkopie behalten). Nie per `rename` über ein ungeprüftes Original.
- **Löschen** (`DeleteFile`): die Datei per `rename` in den eigenen Papierkorb des Sync-Ordners
  verschieben, dort erneut prüfen, bei Abweichung zurückverschieben. Der Papierkorb wird erst nach Tagen
  geleert.
- **Inhalt prüfen:** Fingerprint (Größe, mtime, ctime) vergleichen. Liegt der Fingerprint im unsicheren
  Zeitfenster des Hash-Caches (grobe Zeitstempel, gerade geschrieben), die Datei neu hashen und mit
  `Expected::content` vergleichen. Sonst könnte eine Änderung gleicher Größe im selben Zeitstempel-Intervall
  unbemerkt überschrieben werden.
- **Idempotenz vor Inhalt:** Bei wiederholten Server-Operationen fragt der Client zuerst, ob der Server die
  `OpId` schon kennt, und braucht dann die Quelldatei nicht mehr.

Der Simulator bildet Prüfungen und Hash-Cache genau so nach (inklusive Neu-Hashen im unsicheren Fenster);
der Tausch selbst ist dort atomar und wird mit dem echten Dateisystem-Adapter getestet.

### 5. Inkrementelle Planung

Die Engine prüft nicht bei jedem Durchlauf alle Dateien, sondern nur:
- Objekte, die sich geändert haben, samt Nachbarn: Elternordner, Kinder bei Ordnern, deren Existenz oder Verknüpfung sich ändert, und Beteiligte an Operationsergebnissen.
- Objekte, die beim letzten Mal warten mussten.

Alle Änderungen an R, S und L laufen über Hilfsfunktionen, die diese Markierungen setzen. Nach einem
Neustart, einem vollständigen Scan oder einem vollständigen Server-Stand wird einmal alles geprüft.

Lokale Änderungen kommen einzeln über `on_local_changes`, im Client aus FSEvents und einem Rescan der
betroffenen Ordner. **Vertrag dieser Schnittstelle:**
- Verschobene Objekte werden mit ihrem neuen Ort gemeldet, zusammen mit allen Vorfahren auf dem Pfad, die die Engine anders kennt.
- Als gelöscht wird nur gemeldet, was es nicht mehr gibt.
- Ergibt eine Meldung trotzdem einen widersprüchlichen Baum (z.B. weil eine ältere Verschiebung noch nicht
  gemeldet ist), verlangt die Engine einen vollständigen Scan, statt zu raten (4, Punkt 9).

Der Simulator wechselt zufällig zwischen vollständigen Scans, Änderungslisten und Teil-Rescans einzelner
Ordner. Nach **jeder** Planung ohne Ergebnis prüft er, dass auch eine vollständige Planung nichts mehr fände
(`Engine::verify_incremental`).

Messwerte (`cargo run --release -p xlrx-sync --example scale -- 1000000`, 1 Mio Dateien in 10.000 Ordnern):

| Vorgang | vorher (alles prüfen) | inkrementell |
|---|---|---|
| Planung im Ruhezustand | 725 ms | **0,0002 ms** |
| 1 geänderte Datei erkennen und Upload planen | ~1 s | **0,19 ms** |
| Ersteinrichtung (identische Stände verknüpfen, ohne Übertragung) | ~3 s | ~9 s (einmalig) |
| vollständiger Rescan + Planung (nur beim Start ohne gespeicherten Stand) | 4,5 s | 6,1 s |

Der vollständige Fall ist gegenüber vorher langsamer, weil nun jede Änderung ihre Nachbarn markiert.
Er kommt nur beim ersten Start vor; danach arbeitet der Client mit gespeichertem Stand und FSEvents.

### 6. Deterministische Simulation (`xlrx-sim`)

- **Aufbau eines Laufs:** simulierter Server (Journal, Idempotenz, gleiche Konfliktsemantik) und 2–3 Clients mit simuliertem Dateisystem (Inodes, POSIX-`rename`, optional ohne Groß-/Kleinschreibung wie APFS).
- **Zufällig gemischt werden:**
  - Nutzeraktionen auf allen Seiten: anlegen, schreiben, Atomic Save, verschieben (auch über bestehende Dateien), löschen, `rm -rf`
  - Sync-Schritte in zufälliger Reihenfolge (Operationen laufen auch außer der Reihe)
  - verlorene Anfragen und Antworten
  - Abstürze an beliebiger Stelle, auch zwischen Ausführung und Ergebnis
  - verspätete Ergebnisse in beliebiger Reihenfolge, nach weiteren Scans und Planungen
  - Server-Nutzer mit exakter Namensprüfung (SMB), also Varianten wie „A“ neben „a“ (`--exact-names`)
  - grobe Zeitstempel, bei denen alle Inhalte gleich groß sind; dann sind Fingerprints mehrdeutig (`--coarse`)
- **Prüfungen nach jedem Schritt:** Invarianten der Engine.
- **Prüfungen am Ende einer Ruhephase:**
  - **Konvergenz:** Jeder Client hat exakt den Server-Stand.
  - **Datenerhalt:** Jeder Inhalt, den ein Nutzer geschrieben und nicht selbst entfernt hat, existiert noch.
  - Es sind keine temporären Namen übrig.

Jeder Fehler ist über den Seed exakt reproduzierbar (`xlrx-sim --seed N --trace` zeigt Ablauf und Zustand).

## Ergebnisse

Die Entwicklung lief simulationsgetrieben: Jeder gefundene Fehler wurde am Seed analysiert, die Regel
verallgemeinert und erneut über alle Seeds geprüft. Gefunden und behoben wurden unter anderem:

- **Ordner-Deadlocks:** gelöschter Ordner gegen hineinverschobenes Kind, auf beiden Seiten
- **Abhängigkeitszyklen über erst zu erzeugende Ordner:** Datei wird zu gleichnamigem Ordner, Ringtausch über drei Objekte
- **Verschiebe-Zyklen über Kreuz:** A in B, B in A
- **Atomic Save auf dem Server:** Ordner gelöscht und gleichnamig neu angelegt
- **Hängengebliebene temporäre Namen:** nach Absturz, gleichzeitigem Löschen oder zurückgenommener Verschiebung

Ein anschließender **adversarialer Review** (eigene Szenarien und Simulator-Varianten gezielt gegen die
Regeln) fand Fehler, die der Simulator bis dahin nicht erzeugen konnte, darunter **zwei echte
Datenverluste**:

| Befund | Folge vorher | Behebung |
|---|---|---|
| Server löscht f und legt f neu an, lokal wird f umbenannt | lokale Datei wurde dem neuen Knoten zugeschlagen und mit dessen Inhalt überschrieben (**Datenverlust**) | lokale Änderung wird vor der Neu-Verknüpfung geprüft |
| lokal gelöscht, gleichzeitig auf dem Server verschoben | das Löschen traf die verschobene Datei (**Datenverlust**, nur im Server-Papierkorb) | Ortsprüfung bei Löschen/Verschieben auf dem Server |
| grobe Zeitstempel, Änderung gleicher Größe | Ersetzen/Löschen hätte die Änderung übersehen (im echten Client) | `Expected` mit Inhalt, Neu-Hashen im unsicheren Fenster |
| Server ersetzt Ordner durch leeren gleichnamigen | gelöschter Inhalt kam zurück | Kinder gehen auf den neuen Ordner über |
| überlange Namen mit „Endung“ | Endlosschleife bei der Konfliktnamen-Suche | `with_suffix` liefert immer gültige, verschiedene Namen |
| SMB-Namensvariante bei Mac ohne Groß-/Kleinschreibung | Sync kam nie zur Ruhe | Ankömmling wird umbenannt |
| verspätete Ergebnisse | widersprüchlicher lokaler Baum | 4, Punkte 8 und 9 |
| Gerätename mit „~“, Upload nach Atomic Save, Anlegen nach Atomic Save | falscher Heimatname, unnötige Konfliktkopie, neuer statt alter Knoten | jeweils behoben |

Danach (Stand 2026-10-02) liefen **1.000.000 Seeds im strengen Modus ohne einen einzigen Befund**:
keine Abweichung, kein Datenverlust, keine Invariantenverletzung, und das Sicherheitsnetz wurde nie gebraucht.

| Variante | Läufe |
|---|---|
| 2 Clients, späte Ergebnisse | 400.000 |
| 3 Clients, viele späte Ergebnisse | 150.000 |
| Mac ohne Groß-/Kleinschreibung, SMB-Namensvarianten auf dem Server | 250.000 |
| grobe Zeitstempel, gleich große Inhalte, viele späte Ergebnisse | 200.000 |
| **Summe:** 124 Mio. Nutzeraktionen, 99 Mio. Sync-Operationen, 7,6 Mio. Abstürze, 256.000 Server-Konflikte | **1.000.000** |

Damit ist das Abnahmekriterium von B1 („1 Mio Seeds ohne Verletzung“, PLAN 19) erfüllt.

Jeder Befund ist als Regressionstest in `crates/xlrx-sim/tests/regressions.rs` festgehalten. Jeder dieser
Tests wurde gegen die zurückgenommene Korrektur geprüft und schlägt dann fehl. Die neuen Fehlerarten sind
als Simulator-Varianten dauerhaft in `tests/seeds.rs` und in der nächtlichen CI.

## Offene Punkte

- **Vollständiger Scan/Erstplanung beschleunigen:** Baum-Aufbau mit vorberechneten Namensschlüsseln, Persistenz von L im Client.
- **Selective Sync:** ausgeschlossene Teilbäume dürfen nicht als „lokal gelöscht“ gelten. Geplant als
  Engine-Erweiterung E7 in [ADR 0002](0002-mac-client.md) (§4, M5.10).
- **Ignorierte Dateien:** `.DS_Store` u.ä. in zu löschenden Ordnern. Gelöst im Dateisystem-Adapter
  ([ADR 0002](0002-mac-client.md) §7.3 und §7.6): Unverknüpfte Junk-Dateien kommen mit dem Ordner in den
  Papierkorb des Clients (Test `ordner_loeschen_mit_ds_store`).
- **Massenlösch-Schutz:** Er gehört in den Ordner-Treiber des Clients (`DeleteGuard`,
  [ADR 0002](0002-mac-client.md) §5). Der ungenutzte Konfigurationswert `max_unconfirmed_deletes` ist
  entfallen (E0).
- **Dateisystem-Adapter des Clients** (`xlrx-fs`, `xlrx-client::local`) mit Tausch-, Papierkorb- und
  Neu-Hash-Protokoll (4.1) und eigenen Tests gegen gleichzeitige Schreiber: umgesetzt nach
  [ADR 0002](0002-mac-client.md) §7.
- **Sehr lange Namen:** Ein temporärer Ausweichname kann den Heimatnamen nicht vollständig tragen. Bleibt
  ein solches Objekt nach einem Absturz liegen, wird es unter dem gekürzten Namen wiederhergestellt.
- **Orakel des Simulators:** Inhalte, die ein Nutzer irgendwo entfernt hat, gelten global als entfernbar.
  Ein Verlust einer weiteren Kopie desselben Inhalts (z.B. Konfliktkopie auf einem anderen Gerät) würde
  nicht erkannt.
