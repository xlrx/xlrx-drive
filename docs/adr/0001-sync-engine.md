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
| Löschen gegen Änderung (Inhalt **oder** Ort) | **Änderung gewinnt**, auf beiden Seiten: Verknüpfung wird gelöst, das Objekt wird neu hoch- bzw. heruntergeladen |
| Ordner löschen | nur wenn leer. Kinder, die gelöscht werden, werden abgewartet. Was bleiben muss (neu, geändert, hineinverschoben), holt den Ordner zurück. |
| Gleicher Name, beide neu | gleiche Art und gleicher Inhalt → nur verknüpfen (z.B. Ersteinrichtung, Migration von Synology Drive). Sonst Konfliktkopie. |
| Name auf einer Seite belegt | wer zuletzt kommt, weicht auf einen Konfliktnamen aus |
| Verschiebung würde auf dem Server einen Zyklus erzeugen | Server gewinnt |
| Tausch-Zyklen (a↔b, Datei wird zu gleichnamigem Ordner, …) | ein Beteiligter weicht kurz auf einen temporären Namen aus (`.xlrx-tmp-<Gerät>-<n>~<Heimatname>`) |
| „Atomic Save“ (neue Datei über alte umbenannt), lokal oder auf dem Server | als Inhaltsänderung desselben Objekts erkannt, nicht als Löschen + Neu |

### 4. Sicherheitsnetze gegen Datenverlust

1. **Vorbedingungen an jeder Operation.**
   - Lokales Ersetzen und Löschen nur bei unverändertem Fingerprint.
   - Hochladen nur, wenn die Quelldatei noch genau dem gehashten Stand entspricht.
   - Auf dem Server: Upload und Löschen nur mit passender Basis-Revision, Ordner löschen nur, wenn leer, Anlegen und Verschieben nur bei freiem Namen.
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

### 5. Inkrementelle Planung

Die Engine prüft nicht bei jedem Durchlauf alle Dateien, sondern nur:
- Objekte, die sich geändert haben, samt Nachbarn: Elternordner, Kinder bei Ordnern, deren Existenz oder Verknüpfung sich ändert, und Beteiligte an Operationsergebnissen.
- Objekte, die beim letzten Mal warten mussten.

Alle Änderungen an R, S und L laufen über Hilfsfunktionen, die diese Markierungen setzen. Nach einem
Neustart, einem vollständigen Scan oder einem vollständigen Server-Stand wird einmal alles geprüft.

Lokale Änderungen kommen einzeln über `on_local_changes`, im Client aus FSEvents und einem Rescan der
betroffenen Ordner. **Vertrag dieser Schnittstelle:**
- Verschobene Objekte werden mit ihrem neuen Ort gemeldet, zusammen mit allen Vorfahren, die die Engine nicht kennt.
- Als gelöscht wird nur gemeldet, was es nicht mehr gibt.

Der Simulator wechselt zufällig zwischen vollständigen Scans, Änderungslisten und Teil-Rescans einzelner
Ordner. Nach **jeder** Planung ohne Ergebnis prüft er, dass auch eine vollständige Planung nichts mehr fände
(`Engine::verify_incremental`).

Messwerte (`cargo run --release -p xlrx-sync --example scale -- 1000000`, 1 Mio Dateien in 10.000 Ordnern):

| Vorgang | vorher (alles prüfen) | inkrementell |
|---|---|---|
| Planung im Ruhezustand | 725 ms | **0,0004 ms** |
| 1 geänderte Datei erkennen und Upload planen | ~1 s | **0,14 ms** |
| Ersteinrichtung (identische Stände verknüpfen, ohne Übertragung) | ~3 s | ~10 s (einmalig) |
| vollständiger Rescan + Planung (nur beim Start ohne gespeicherten Stand) | 4,5 s | 6,6 s |

Der vollständige Fall ist gegenüber vorher langsamer, weil nun jede Änderung ihre Nachbarn markiert.
Er kommt nur beim ersten Start vor; danach arbeitet der Client mit gespeichertem Stand und FSEvents.

### 6. Deterministische Simulation (`xlrx-sim`)

- **Aufbau eines Laufs:** simulierter Server (Journal, Idempotenz, gleiche Konfliktsemantik) und 2–3 Clients mit simuliertem Dateisystem (Inodes, POSIX-`rename`, optional ohne Groß-/Kleinschreibung wie APFS).
- **Zufällig gemischt werden:**
  - Nutzeraktionen auf allen Seiten: anlegen, schreiben, Atomic Save, verschieben (auch über bestehende Dateien), löschen, `rm -rf`
  - Sync-Schritte in zufälliger Reihenfolge (Operationen laufen auch außer der Reihe)
  - verlorene Anfragen und Antworten
  - Abstürze an beliebiger Stelle, auch zwischen Ausführung und Ergebnis
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

**Datenverlust trat in keinem einzigen Lauf auf.** Alle gefundenen Fehler betrafen Konvergenz:
Der Sync blieb stehen oder ließ Unterschiede übrig.

## Offene Punkte

- **Vollständiger Scan/Erstplanung beschleunigen:** Baum-Aufbau mit vorberechneten Namensschlüsseln, Persistenz von L im Client.
- **Selective Sync:** ausgeschlossene Teilbäume dürfen nicht als „lokal gelöscht“ gelten.
- **Ignorierte Dateien:** `.DS_Store` u.ä. in zu löschenden Ordnern.
- **Massenlösch-Schutz:** Der Konfigurationswert existiert, die Bestätigungslogik fehlt noch.
- **Groß-/Kleinschreibungs-Varianten** vom Server (über SMB) bei Clients ohne Unterscheidung: Die Regel existiert, sie wird im Simulator noch nicht erzeugt.
