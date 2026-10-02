//! Operationen, die die Engine plant, und ihre Ergebnisse.
//!
//! Die Engine führt nichts selbst aus. Der Aufrufer (Client bzw. Simulator) führt die Operationen
//! aus und meldet das Ergebnis zurück. Jede Operation trägt die Vorbedingungen, die beim Ausführen
//! geprüft werden müssen. Hat sich der Zustand inzwischen geändert, wird die Operation nicht
//! ausgeführt (`Precondition` bzw. `Rejected`), und die Engine plant neu.

use serde::{Deserialize, Serialize};
use xlrx_proto::{FileContent, Name, NodeId, Rev, Seq};

use crate::types::{Fingerprint, LocalId, OpId};

/// Herkunft eines neu angelegten Server-Knotens.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Origin {
    /// Gewöhnliches neues lokales Objekt; es liegt lokal am selben Ort wie auf dem Server.
    New,
    /// Konfliktkopie: Das lokale Objekt liegt noch unter `local_parent`/`local_name` und wird danach
    /// lokal auf den Konfliktnamen umbenannt. War es bisher mit `replaces` verknüpft, wird diese
    /// Verknüpfung gelöst (der Knoten `replaces` wird dann neu heruntergeladen).
    ConflictCopy {
        local_parent: NodeId,
        local_name: Name,
        replaces: Option<NodeId>,
    },
}

/// Operation auf dem Server. Wird vor dem Senden im Outbox persistiert und nach einem Neustart
/// mit derselben [`OpId`] wiederholt; der Server führt jede `OpId` nur einmal aus.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RemoteOp {
    CreateDir {
        parent: NodeId,
        name: Name,
        source: LocalId,
        origin: Origin,
    },
    /// Neue Datei. Der Ausführende lädt den Inhalt aus `source` hoch und prüft dabei, dass die Datei
    /// noch den Fingerprint `fp` und den Inhalt `content` hat (sonst `SourceChanged`).
    CreateFile {
        parent: NodeId,
        name: Name,
        content: FileContent,
        source: LocalId,
        fp: Fingerprint,
        origin: Origin,
    },
    /// Neuer Inhalt für eine bestehende Datei. Nur gültig, wenn der Server noch `base_rev` hat;
    /// sonst legt der Server den Inhalt als Konfliktkopie an (Ergebnis `Conflict`).
    Upload {
        node: NodeId,
        base_rev: Rev,
        content: FileContent,
        source: LocalId,
        fp: Fingerprint,
    },
    /// Verschieben/Umbenennen, aber nur, wenn der Knoten noch unter `from_parent`/`from_name` liegt.
    /// Hat ihn inzwischen jemand anderes verschoben, gewinnt dessen Verschiebung (`Rejected(Moved)`).
    Move {
        node: NodeId,
        from_parent: NodeId,
        from_name: Name,
        parent: NodeId,
        name: Name,
    },
    /// Löscht eine Datei, aber nur, wenn sie noch `base_rev` hat und unter `parent`/`name` liegt.
    /// Inhalte, die der Client nicht kennt, und Verschiebungen, die er nicht gesehen hat, gehen so nie verloren.
    DeleteFile {
        node: NodeId,
        base_rev: Rev,
        parent: NodeId,
        name: Name,
    },
    /// Löscht einen Ordner, aber nur, wenn er auf dem Server leer ist und unter `parent`/`name` liegt.
    DeleteDir {
        node: NodeId,
        parent: NodeId,
        name: Name,
    },
}

/// Grund, aus dem der Server eine Operation abgelehnt hat. Der Zustand bleibt unverändert.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Reject {
    NameTaken,
    ParentGone,
    NodeGone,
    NotEmpty,
    RevMismatch,
    WouldCycle,
    /// Der Knoten liegt nicht (mehr) am erwarteten Ort.
    Moved,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RemoteResult {
    Created {
        node: NodeId,
        rev: Rev,
        seq: Seq,
    },
    Updated {
        rev: Rev,
        seq: Seq,
    },
    /// `Upload` mit veralteter Basis: Der Server hat den hochgeladenen Inhalt als neuen Knoten
    /// (Konfliktkopie) neben dem Original abgelegt. Nichts wurde überschrieben.
    Conflict {
        node: NodeId,
        rev: Rev,
        seq: Seq,
    },
    Moved {
        seq: Seq,
    },
    Deleted {
        seq: Seq,
    },
    Rejected(Reject),
    /// Die lokale Quelldatei hat sich vor oder während des Hochladens geändert. Nichts wurde übernommen.
    SourceChanged,
    /// Netzwerkfehler o.ä.: Ob die Operation ausgeführt wurde, ist unbekannt. Später mit derselben ID wiederholen.
    Transient,
}

/// Erwarteter Zustand einer lokalen Datei vor einer Operation, die sie ersetzt oder löscht.
///
/// Der Ausführende prüft den Fingerprint und – wenn der Fingerprint im unsicheren Zeitfenster liegt
/// (grobe Zeitstempel, gerade geschrieben) – zusätzlich den Inhalt durch erneutes Hashen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Expected {
    pub fp: Fingerprint,
    pub content: FileContent,
}

/// Operation im lokalen Dateisystem.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LocalOp {
    /// Ordner anlegen. Schlägt fehl, wenn der Name belegt ist.
    CreateDir {
        parent: LocalId,
        name: Name,
        node: NodeId,
        node_parent: NodeId,
    },
    /// Neue Datei herunterladen: in eine temporäre Datei schreiben, Hash prüfen, dann ohne Ersetzen
    /// an den Zielnamen verschieben. Schlägt fehl, wenn der Name belegt ist.
    Download {
        parent: LocalId,
        name: Name,
        node: NodeId,
        node_parent: NodeId,
        rev: Rev,
        content: FileContent,
    },
    /// Inhalt einer bestehenden Datei ersetzen, aber nur, wenn sie noch unter `parent`/`name` liegt
    /// und `expect` entspricht. Der Ausführende tauscht die fertige neue Datei atomar mit dem
    /// Original und prüft das ausgetauschte Original danach erneut; hat es sich in der Zwischenzeit
    /// geändert, wird zurückgetauscht (nichts geht verloren).
    Replace {
        local: LocalId,
        parent: LocalId,
        name: Name,
        expect: Expected,
        node: NodeId,
        rev: Rev,
        content: FileContent,
    },
    /// Umbenennen/Verschieben ohne Ersetzen. Prüft, dass das Objekt noch unter `from_parent`/`from_name` liegt.
    /// `synced_to`: Ort, der danach als vereinbart gilt (`None` bei rein lokalen Ausweich-Umbenennungen).
    Move {
        local: LocalId,
        from_parent: LocalId,
        from_name: Name,
        parent: LocalId,
        name: Name,
        synced_to: Option<(NodeId, Name)>,
    },
    /// Datei löschen, aber nur, wenn sie noch unter `parent`/`name` liegt und `expect` entspricht.
    /// Der Ausführende verschiebt sie dazu in den eigenen Papierkorb und prüft sie dort erneut;
    /// hat sie sich geändert, kommt sie zurück.
    DeleteFile {
        local: LocalId,
        parent: LocalId,
        name: Name,
        expect: Expected,
        node: NodeId,
    },
    /// Ordner löschen, aber nur, wenn er leer ist und unter `parent`/`name` liegt.
    DeleteDir {
        local: LocalId,
        parent: LocalId,
        name: Name,
        node: NodeId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LocalResult {
    /// Ausgeführt. `id`/`fp` beschreiben das Ergebnisobjekt (bei `Replace` evtl. eine neue Identität).
    Done {
        id: LocalId,
        fp: Option<Fingerprint>,
    },
    /// Vorbedingung verletzt (geändert, verschwunden, Name belegt). Nichts wurde verändert.
    Precondition,
    /// Sonstiger Fehler. Nichts wurde verändert.
    Error,
}

/// Eine geplante Operation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    Remote(OpId, RemoteOp),
    Local(OpId, LocalOp),
}
