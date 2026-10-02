//! Die Sync-Engine: Drei-Wege-Abgleich zwischen Server (R), vereinbartem Stand (S) und lokaler Platte (L).
//!
//! Die Regeln sind in `docs/adr/0001-sync-engine.md` beschrieben. Kurzfassung:
//!
//! - Je Knoten wird **Ort** (Eltern + Name) und **Inhalt** getrennt verglichen.
//! - Nur eine Seite geändert → auf die andere Seite übertragen.
//! - Beide Seiten gleich geändert → nur S nachziehen.
//! - Inhalt auf beiden Seiten verschieden geändert → Konfliktkopie, nichts wird überschrieben.
//! - Ort auf beiden Seiten verschieden geändert → Server gewinnt (lokal verschieben).
//! - Löschen gegen Änderung → **die Änderung gewinnt** (auf beiden Seiten).
//! - Ordner werden nur gelöscht, wenn sie leer sind; was noch darin überleben muss, holt den Ordner zurück.
//!
//! Sicherheitsnetze: Jede Operation trägt Vorbedingungen (Fingerprint, Revision, leerer Ordner,
//! freier Name), Server-Operationen sind idempotent (Outbox mit [`OpId`]), und lokaler Inhalt wird
//! nur ersetzt oder gelöscht, wenn er nachweislich dem vereinbarten Stand entspricht.

use std::collections::{BTreeMap, BTreeSet};

use xlrx_proto::{Kind, Name, NodeId, Rev, Seq};

use crate::ops::{Expected, LocalOp, LocalResult, Op, Origin, RemoteOp, RemoteResult};
use crate::synced::Synced;
use crate::tree::Tree;
use crate::types::{
    Config, LocalEntry, LocalId, LocalObservation, OpId, RemoteChange, RemoteEntry, SyncedEntry,
};

/// Längste Kette, die bei der Suche nach Tausch-Zyklen verfolgt wird.
const MAX_CHAIN: usize = 64;

/// Präfix temporärer Ausweichnamen.
pub const TEMP_PREFIX: &str = ".xlrx-tmp-";

/// Präfix der temporären Dateien, in die der Ausführende herunterlädt, bevor er sie an ihren Platz
/// tauscht. Bleibt nach einem Absturz eine liegen, wird sie nie hochgeladen (der Ausführende räumt sie weg).
pub const DOWNLOAD_TEMP_PREFIX: &str = ".xlrx-dl-";

/// Heimatname eines temporären Ausweichnamens (Teil nach dem ersten „~“; der Gerätename im
/// Präfix enthält nie „~“). Ist der Heimatname beim Ausweichen gekürzt worden, wird der gekürzte
/// Name wiederhergestellt.
fn temp_home(name: &Name) -> Option<Name> {
    let rest = name.as_str().strip_prefix(TEMP_PREFIX)?;
    let home = rest.split_once('~').map(|(_, h)| h).unwrap_or("");
    Some(Name::new(home).unwrap_or_else(|_| Name::new("Wiederhergestellt").expect("gültiger Name")))
}

/// Persistierter Zustand. Nach einem Absturz wird die Engine daraus neu aufgebaut
/// ([`Engine::from_state`]); der lokale Baum wird per Scan neu erhoben.
/// (Im Client wird er strukturiert in SQLite gespeichert, im Simulator als Kopie.)
#[derive(Clone, Debug)]
pub struct State {
    pub config: Config,
    /// Journal-Position, bis zu der R den Server widerspiegelt. `None` vor dem ersten Abruf.
    pub cursor: Option<Seq>,
    pub remote: Tree<NodeId, RemoteEntry>,
    pub synced: Synced,
    /// Server-Operationen, die gesendet wurden oder werden. Werden erst nach verarbeitetem Ergebnis entfernt.
    pub outbox: BTreeMap<OpId, RemoteOp>,
    /// Knoten, deren S-Stand durch eigene Operationen neuer ist als R. Bis R diese Sequenz erreicht hat,
    /// gilt für sie S als Server-Stand, und sie werden nicht neu geplant.
    pub pending: BTreeMap<NodeId, Seq>,
    /// Lokale Ausweich-Umbenennungen (Tausch-Zyklen): Objekt → (Ordner, temporärer Name).
    /// Solange es dort liegt, gilt für den Abgleich sein vereinbarter Ort, damit das Ausweichen
    /// nie als Nutzeränderung zählt. Wird vor dem Ausführen persistiert.
    pub local_temps: BTreeMap<LocalId, (LocalId, Name)>,
    pub next_op: u64,
    pub name_counter: u64,
    /// Wie oft das Sicherheitsnetz einen Warte-Zyklus auflösen musste (Diagnose).
    pub breakers_used: u64,
    pub last_breaker: Option<String>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            cursor: None,
            remote: Tree::new(true),
            synced: Synced::default(),
            outbox: BTreeMap::new(),
            pending: BTreeMap::new(),
            local_temps: BTreeMap::new(),
            next_op: 1,
            name_counter: 0,
            breakers_used: 0,
            last_breaker: None,
        }
    }
}

#[derive(Default)]
struct Ctx {
    ops: Vec<Op>,
    busy_n: BTreeSet<NodeId>,
    busy_l: BTreeSet<LocalId>,
    /// Stellen, an denen eine Regel wartet, mit einer sicheren Aktion, die das Warten auflöst.
    stuck: Vec<Breaker>,
}

/// Sichere Aktionen, mit denen ein unvorhergesehener Warte-Zyklus aufgelöst wird.
/// Keine davon kann Daten verlieren: Es wird nur umbenannt oder etwas behalten statt gelöscht.
#[derive(Clone, Copy, Debug)]
enum Breaker {
    /// Lokales Objekt vorübergehend auf einen freien Namen schieben.
    TempLocal(LocalId),
    /// Server-Objekt vorübergehend auf einen freien Namen schieben.
    TempRemote(NodeId),
    /// Verknüpfung lösen: Der Ordner wird behalten und neu angelegt statt gelöscht.
    Unlink(NodeId),
}

/// Schlüssel für die inkrementelle Planung.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Key {
    Node(NodeId),
    Local(LocalId),
}

#[derive(Clone)]
pub struct Engine {
    st: State,
    local: Tree<LocalId, LocalEntry>,
    local_root: Option<LocalId>,
    inflight_local: BTreeMap<OpId, LocalOp>,
    sent: BTreeSet<OpId>,
    need_scan: bool,
    need_fetch: bool,
    /// Aufeinanderfolgende Planungen ohne Fortschritt bei frischem Zustand.
    stalled: u32,
    /// Inkrementelle Planung: Objekte, deren Lage sich seit der letzten Planung geändert hat.
    dirty: BTreeSet<Key>,
    /// Objekte, die bei der letzten Planung warten mussten; sie werden jedes Mal neu geprüft.
    waiting: BTreeSet<Key>,
    /// Verknüpfte Knoten, deren lokales Objekt fehlt (Kandidaten für das Neu-Verknüpfen).
    orphans: BTreeSet<NodeId>,
    /// Die nächste Planung prüft alles (nach Start, vollständigem Scan oder Server-Stand).
    full: bool,
    /// Der lokale Baum ist widersprüchlich geworden (Teil-Scan mit unbekannten Vorfahren, verspätetes
    /// Ergebnis auf veraltetem Stand). Bis zum nächsten vollständigen Scan wird nichts geplant:
    /// Ein unvollständiges Bild darf nie als „lokal gelöscht“ gelesen werden.
    local_untrusted: bool,
}

/// So viele Planungen ohne Fortschritt (bei frischem Server- und lokalem Stand) müssen
/// vergehen, bevor das Sicherheitsnetz eingreift.
const STALL_LIMIT: u32 = 3;

impl Engine {
    pub fn new(config: Config) -> Self {
        Self::from_state(State::new(config))
    }

    /// Neustart aus persistiertem Zustand. Lokale Operationen, die noch liefen, gelten als verloren;
    /// ein Scan stellt fest, was davon geschehen ist. Server-Operationen aus der Outbox werden
    /// mit derselben ID erneut gesendet.
    pub fn from_state(st: State) -> Self {
        let fold = st.config.local_case_insensitive;
        Self {
            st,
            local: Tree::new(fold),
            local_root: None,
            inflight_local: BTreeMap::new(),
            sent: BTreeSet::new(),
            need_scan: true,
            need_fetch: true,
            stalled: 0,
            dirty: BTreeSet::new(),
            waiting: BTreeSet::new(),
            orphans: BTreeSet::new(),
            full: true,
            local_untrusted: false,
        }
    }

    pub fn state(&self) -> &State {
        &self.st
    }

    pub fn local_tree(&self) -> &Tree<LocalId, LocalEntry> {
        &self.local
    }

    /// Hat die Engine schon einen lokalen Stand (vollständiger Scan seit dem Start)?
    pub fn has_local_tree(&self) -> bool {
        self.local_root.is_some()
    }

    pub fn wants_scan(&self) -> bool {
        self.need_scan
    }

    /// Nur ein vollständiger Scan ([`Self::on_local_snapshot`]) hilft weiter.
    pub fn wants_full_scan(&self) -> bool {
        self.local_root.is_none() || self.local_untrusted
    }

    fn mark_untrusted(&mut self) {
        self.local_untrusted = true;
        self.need_scan = true;
    }

    pub fn wants_fetch(&self) -> bool {
        self.need_fetch
    }

    /// Keine offenen Operationen.
    pub fn is_idle(&self) -> bool {
        self.st.outbox.is_empty() && self.inflight_local.is_empty()
    }

    fn root(&self) -> NodeId {
        self.st.config.remote_root
    }

    // ------------------------------------------------------------------------------------------
    // Eingaben
    // ------------------------------------------------------------------------------------------

    /// Änderungen aus dem Server-Journal seit dem letzten Cursor.
    pub fn on_remote_changes(&mut self, changes: Vec<RemoteChange>, cursor: Seq) {
        let root = self.root();
        let mut inserted = Vec::new();
        let mut deleted = Vec::new();
        // Erst alle neuen Zustände übernehmen, dann Löschungen auswerten: Ein Knoten, der im selben
        // Stapel aus einem gelöschten Ordner heraus verschoben wurde, darf nicht mitgelöscht werden.
        for ch in changes {
            if ch.node == root {
                continue;
            }
            match ch.state {
                Some(e) => {
                    self.r_insert(ch.node, e);
                    inserted.push(ch.node);
                }
                None => deleted.push(ch.node),
            }
        }
        for n in deleted {
            // Gelöscht oder aus dem Sichtbereich verschoben: samt (verbliebenem) Unterbaum entfernen.
            for d in self.st.remote.descendants(n) {
                self.r_remove(d);
            }
            self.r_remove(n);
        }
        // Ein Knoten, dessen Elternknoten nach dem ganzen Stapel fehlt, ist nicht sichtbar.
        for n in inserted {
            if let Some(p) = self.st.remote.get(n).map(|e| e.parent)
                && p != root
                && !self.st.remote.contains(p)
            {
                for d in self.st.remote.descendants(n) {
                    self.r_remove(d);
                }
                self.r_remove(n);
            }
        }
        self.finish_remote_update(cursor);
    }

    /// Vollständiger Server-Stand (erster Abruf oder nach gekürztem Journal).
    pub fn on_remote_snapshot(&mut self, entries: Vec<(NodeId, RemoteEntry)>, cursor: Seq) {
        let mut t = Tree::new(true);
        for (n, e) in entries {
            if n != self.root() {
                t.insert(n, e);
            }
        }
        t.retain_reachable(self.root());
        self.st.remote = t;
        self.full = true;
        self.finish_remote_update(cursor);
    }

    fn finish_remote_update(&mut self, cursor: Seq) {
        self.st.cursor = Some(cursor);
        let cleared: Vec<NodeId> = self
            .st
            .pending
            .iter()
            .filter(|(_, s)| **s <= cursor)
            .map(|(n, _)| *n)
            .collect();
        for n in cleared {
            self.st.pending.remove(&n);
            self.mark_node_dir(n);
            if let Some(p) = self.st.remote.get(n).map(|e| e.parent) {
                self.dirty.insert(Key::Node(p));
            }
        }
        self.need_fetch = false;
    }

    /// Vollständiger Scan des lokalen Sync-Ordners.
    pub fn on_local_snapshot(&mut self, root: LocalId, observations: Vec<LocalObservation>) {
        let mut t = Tree::new(self.st.config.local_case_insensitive);
        for o in observations {
            if o.id != root && o.id != LocalId::GONE {
                t.insert(o.id, o.entry);
            }
        }
        t.retain_reachable(root);
        self.local = t;
        self.local_root = Some(root);
        self.need_scan = false;
        self.local_untrusted = false;
        self.full = true;
        self.orphans = self.st.synced.ids().into_iter().collect();
        let moving: BTreeSet<LocalId> = self
            .inflight_local
            .values()
            .filter_map(|op| match op {
                LocalOp::Move { local, .. } => Some(*local),
                _ => None,
            })
            .collect();
        let local = &self.local;
        self.st.local_temps.retain(|l, (tp, tn)| {
            moving.contains(l)
                || local
                    .get(*l)
                    .is_some_and(|e| e.parent == *tp && e.name == *tn)
        });
        self.rebind();
    }

    /// Einzelne lokale Änderungen (z.B. aus FSEvents und einem Rescan der betroffenen Ordner).
    /// Vorher muss es einen vollständigen Scan gegeben haben.
    pub fn on_local_changes(&mut self, upserts: Vec<LocalObservation>, removed: Vec<LocalId>) {
        let Some(root) = self.local_root else {
            self.need_scan = true;
            return;
        };
        for l in removed {
            if l == root {
                continue;
            }
            for d in self.local.descendants(l) {
                if let Some(n) = self.st.synced.node_of(d) {
                    self.orphans.insert(n);
                }
                self.l_remove(d);
            }
            if let Some(n) = self.st.synced.node_of(l) {
                self.orphans.insert(n);
            }
            self.l_remove(l);
        }
        let mut upserted = Vec::with_capacity(upserts.len());
        for o in upserts {
            if o.id == root || o.id == LocalId::GONE {
                continue;
            }
            if let Some(n) = self.st.synced.node_of(o.id)
                && self.local.get(o.id).is_some_and(|e| e.kind != o.entry.kind)
            {
                self.orphans.insert(n);
            }
            upserted.push(o.id);
            self.l_insert(o.id, o.entry);
        }
        // Konsistenz: Jedes gemeldete Objekt muss von der Wurzel aus erreichbar sein. Sonst fehlt eine
        // Meldung (unbekannter Vorfahr, Zyklus mit noch nicht gemeldeter Verschiebung) – dann nichts
        // entfernen (das sähe wie Löschen aus), sondern vollständig neu scannen.
        let broken = upserted
            .into_iter()
            .any(|l| self.local.depth(l, root).is_none());
        self.need_scan = false;
        if broken {
            self.mark_untrusted();
        }
        let moving: BTreeSet<LocalId> = self
            .inflight_local
            .values()
            .filter_map(|op| match op {
                LocalOp::Move { local, .. } => Some(*local),
                _ => None,
            })
            .collect();
        let local = &self.local;
        self.st.local_temps.retain(|l, (tp, tn)| {
            moving.contains(l)
                || local
                    .get(*l)
                    .is_some_and(|e| e.parent == *tp && e.name == *tn)
        });
        self.rebind();
    }

    /// Hält die Verknüpfung lokal ↔ Server stabil, wenn sich die lokale Identität ändert.
    ///
    /// 1. Hat eine verknüpfte ID jetzt eine andere Art (Inode-Wiederverwendung), wird die Verknüpfung gelöst.
    /// 2. Ist die verknüpfte ID verschwunden und liegt am selben Ort ein neues, unverknüpftes Objekt
    ///    gleicher Art, gilt es als dasselbe Objekt. Das ist das übliche „Atomic Save“ vieler Programme
    ///    (neue Datei schreiben, dann über das Original umbenennen) und wird als Inhaltsänderung behandelt,
    ///    nicht als Löschen + Neuanlegen.
    fn rebind(&mut self) {
        let candidates: Vec<NodeId> = self.orphans.iter().copied().collect();
        for n in candidates {
            let Some(s) = self.st.synced.get(n) else {
                continue;
            };
            if let Some(le) = self.local.get(s.local)
                && le.kind != s.kind
            {
                self.s_update(n, |e| e.local = LocalId::GONE);
            }
        }
        // Nur Knoten ohne lokales Objekt bleiben Kandidaten.
        let synced = &self.st.synced;
        let local = &self.local;
        self.orphans
            .retain(|n| synced.get(*n).is_some_and(|s| !local.contains(s.local)));
        loop {
            let mut changed = false;
            let candidates: Vec<NodeId> = self.orphans.iter().copied().collect();
            for n in candidates {
                let Some(s) = self.st.synced.get(n).cloned() else {
                    self.orphans.remove(&n);
                    continue;
                };
                if self.local.contains(s.local) {
                    self.orphans.remove(&n);
                    continue;
                }
                let Some(lp) = self.local_of_node(s.parent) else {
                    continue;
                };
                let candidate = self.local.lookup(lp, &s.name).find(|o| {
                    self.st.synced.node_of(*o).is_none()
                        && self.local.get(*o).is_some_and(|e| e.kind == s.kind)
                });
                if let Some(o) = candidate
                    && self.s_update(n, |e| e.local = o)
                {
                    self.orphans.remove(&n);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // Ergebnisse
    // ------------------------------------------------------------------------------------------

    pub fn on_local_result(&mut self, id: OpId, result: LocalResult) {
        let Some(op) = self.inflight_local.remove(&id) else {
            return;
        };
        self.mark_local_op(&op);
        match result {
            LocalResult::Done { id: new_id, fp } => self.apply_local_done(op, new_id, fp),
            LocalResult::Precondition | LocalResult::Error => self.need_scan = true,
        }
    }

    /// Lokaler Elternordner bekannt? Ergebnisse können verspätet eintreffen, wenn ein Scan den
    /// Ordner schon als gelöscht gemeldet hat. Dann wird nichts eingetragen; der nächste Scan zeigt,
    /// was wirklich auf der Platte liegt (L bleibt lieber veraltet als unvollständig).
    fn local_parent_known(&mut self, parent: LocalId) -> bool {
        let known = Some(parent) == self.local_root || self.local.contains(parent);
        if !known {
            self.need_scan = true;
        }
        known
    }

    fn apply_local_done(&mut self, op: LocalOp, new_id: LocalId, fp: Option<crate::Fingerprint>) {
        match op {
            LocalOp::CreateDir {
                parent,
                name,
                node,
                node_parent,
            } => {
                // Hat ein Scan das neue Objekt schon gesehen, ist sein Eintrag neuer als das Ergebnis.
                if !self.local.contains(new_id) {
                    if !self.local_parent_known(parent) {
                        return;
                    }
                    self.l_insert(
                        new_id,
                        LocalEntry {
                            parent,
                            name: name.clone(),
                            kind: Kind::Dir,
                            fp: None,
                            content: None,
                        },
                    );
                }
                let rev = self.st.remote.get(node).map_or(Rev(0), |r| r.rev);
                self.link_if_free(
                    node,
                    SyncedEntry {
                        parent: node_parent,
                        name,
                        kind: Kind::Dir,
                        content: None,
                        rev,
                        local: new_id,
                        fp: None,
                    },
                );
            }
            LocalOp::Download {
                parent,
                name,
                node,
                node_parent,
                rev,
                content,
            } => {
                if !self.local.contains(new_id) {
                    if !self.local_parent_known(parent) {
                        return;
                    }
                    self.l_insert(
                        new_id,
                        LocalEntry {
                            parent,
                            name: name.clone(),
                            kind: Kind::File,
                            fp,
                            content: Some(content),
                        },
                    );
                }
                self.link_if_free(
                    node,
                    SyncedEntry {
                        parent: node_parent,
                        name,
                        kind: Kind::File,
                        content: Some(content),
                        rev,
                        local: new_id,
                        fp,
                    },
                );
            }
            LocalOp::Replace {
                local,
                node,
                rev,
                content,
                ..
            } => {
                if self.local.contains(new_id) {
                    self.l_remove(local); // ein Scan hat das Ergebnis schon gesehen
                } else if let Some(e) = self.l_remove(local) {
                    self.l_insert(
                        new_id,
                        LocalEntry {
                            fp,
                            content: Some(content),
                            ..e
                        },
                    );
                } else {
                    self.need_scan = true;
                }
                if self.st.synced.get(node).is_some_and(|s| s.local == local) {
                    self.s_update(node, |s| {
                        s.local = new_id;
                        s.content = Some(content);
                        s.rev = rev;
                        s.fp = fp;
                    });
                }
            }
            LocalOp::Move {
                local,
                from_parent,
                from_name,
                parent,
                name,
                synced_to,
            } => {
                if self.st.local_temps.get(&local) != Some(&(parent, name.clone())) {
                    self.st.local_temps.remove(&local);
                }
                // L nur nachziehen, wenn es noch den Stand vor der Verschiebung zeigt. Zeigt es etwas
                // anderes, hat ein Scan schon Neueres gesehen.
                let before = self
                    .local
                    .get(local)
                    .is_some_and(|e| e.parent == from_parent && e.name == from_name);
                if before {
                    let known = Some(parent) == self.local_root || self.local.contains(parent);
                    if !known || parent == local || self.local.is_within(parent, local) {
                        // Auf veraltetem Stand nicht abbildbar (anderes Ergebnis steht noch aus).
                        self.mark_untrusted();
                    } else {
                        self.l_update(local, |e| {
                            e.parent = parent;
                            e.name = name;
                        });
                    }
                }
                if let Some((p, nm)) = synced_to
                    && let Some(n) = self.st.synced.node_of(local)
                {
                    self.s_update(n, |s| {
                        s.parent = p;
                        s.name = nm;
                    });
                }
            }
            LocalOp::DeleteFile { local, node, .. } => {
                self.l_remove(local);
                if self.st.synced.get(node).is_some_and(|s| s.local == local) {
                    self.s_remove(node);
                }
            }
            LocalOp::DeleteDir { local, node, .. } => {
                if self.local.has_children(local) {
                    // Der Ordner war beim Löschen leer, L kennt aber noch Kinder: L ist veraltet
                    // (die Kinder wurden inzwischen verschoben). Erst der Scan sagt, wo sie sind;
                    // als gelöscht dürfen sie keinesfalls gelten.
                    self.mark_untrusted();
                    return;
                }
                self.l_remove(local);
                if self.st.synced.get(node).is_some_and(|s| s.local == local) {
                    self.s_remove(node);
                }
            }
        }
    }

    pub fn on_remote_result(&mut self, id: OpId, result: RemoteResult) {
        let Some(op) = self.st.outbox.get(&id).cloned() else {
            return;
        };
        self.mark_remote_op(&op);
        if result == RemoteResult::Transient {
            // Unbekannt, ob ausgeführt: später mit derselben ID erneut senden (Server dedupliziert).
            self.sent.remove(&id);
            return;
        }
        self.st.outbox.remove(&id);
        self.sent.remove(&id);
        match (op, result) {
            (
                RemoteOp::CreateDir {
                    parent,
                    name,
                    source,
                    origin,
                },
                RemoteResult::Created { node, rev, seq },
            ) => {
                self.link_created(
                    node,
                    parent,
                    name,
                    origin,
                    Kind::Dir,
                    None,
                    rev,
                    source,
                    None,
                );
                self.mark_pending(node, seq);
            }
            (
                RemoteOp::CreateFile {
                    parent,
                    name,
                    content,
                    source,
                    fp,
                    origin,
                },
                RemoteResult::Created { node, rev, seq },
            ) => {
                self.link_created(
                    node,
                    parent,
                    name,
                    origin,
                    Kind::File,
                    Some(content),
                    rev,
                    source,
                    Some(fp),
                );
                self.mark_pending(node, seq);
            }
            (
                RemoteOp::Upload {
                    node,
                    content,
                    source,
                    fp,
                    ..
                },
                RemoteResult::Updated { rev, seq },
            ) => {
                // Server und lokale Quelle hatten danach denselben Inhalt. Wurde die lokale Datei
                // inzwischen per „Atomic Save“ ersetzt (neue lokale ID), gilt das trotzdem; nur der
                // Fingerprint gehört dann nicht zur aktuellen Datei.
                if let Some(s) = self.st.synced.get(node) {
                    let same_local = s.local == source;
                    self.s_update(node, |s| {
                        s.content = Some(content);
                        s.rev = rev;
                        if same_local {
                            s.fp = Some(fp);
                        }
                    });
                }
                self.mark_pending(node, seq);
            }
            (
                RemoteOp::Upload {
                    node,
                    content,
                    source,
                    fp,
                    ..
                },
                RemoteResult::Conflict {
                    node: copy,
                    rev,
                    seq,
                },
            ) => {
                // Der Server hatte inzwischen neueren Inhalt und hat unseren als Konfliktkopie abgelegt.
                // Die lokale Datei gehört jetzt zur Kopie; das Original wird neu heruntergeladen.
                if let Some(s) = self.st.synced.get(node).cloned()
                    && s.local == source
                {
                    self.s_remove(node);
                    self.link_if_free(
                        copy,
                        SyncedEntry {
                            parent: s.parent,
                            name: s.name,
                            kind: Kind::File,
                            content: Some(content),
                            rev,
                            local: source,
                            fp: Some(fp),
                        },
                    );
                }
                self.mark_pending(copy, seq);
            }
            (
                RemoteOp::Move {
                    node, parent, name, ..
                },
                RemoteResult::Moved { seq },
            ) => {
                self.s_update(node, |s| {
                    s.parent = parent;
                    s.name = name;
                });
                self.mark_pending(node, seq);
            }
            (
                RemoteOp::DeleteFile { node, .. } | RemoteOp::DeleteDir { node, .. },
                RemoteResult::Deleted { seq },
            ) => {
                self.s_remove(node);
                self.mark_pending(node, seq);
            }
            (_, RemoteResult::SourceChanged) => self.need_scan = true,
            (_, _) => self.need_fetch = true,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn link_created(
        &mut self,
        node: NodeId,
        parent: NodeId,
        name: Name,
        origin: Origin,
        kind: Kind,
        content: Option<xlrx_proto::FileContent>,
        rev: Rev,
        source: LocalId,
        fp: Option<crate::Fingerprint>,
    ) {
        let (sp, sn) = match origin {
            Origin::New => (parent, name),
            Origin::ConflictCopy {
                local_parent,
                local_name,
                replaces,
            } => {
                if let Some(old) = replaces
                    && self.st.synced.get(old).is_some_and(|s| s.local == source)
                {
                    self.s_remove(old);
                }
                (local_parent, local_name)
            }
        };
        self.link_if_free(
            node,
            SyncedEntry {
                parent: sp,
                name: sn,
                kind,
                content,
                rev,
                local: source,
                fp,
            },
        );
    }

    /// Verknüpft nur, wenn weder Knoten noch lokale ID schon verknüpft sind.
    ///
    /// Gibt es die lokale ID nicht mehr (z.B. „Atomic Save“, während das Ergebnis unterwegs war),
    /// wird sofort das Neu-Verknüpfen versucht. Sonst sähe der Knoten lokal gelöscht aus.
    fn link_if_free(&mut self, node: NodeId, e: SyncedEntry) {
        if !self.st.synced.contains(node) && self.st.synced.node_of(e.local).is_none() {
            let absent = !self.local.contains(e.local);
            if self.s_insert(node, e) && absent && self.local_root.is_some() {
                self.orphans.insert(node);
                self.rebind();
            }
        }
    }

    fn mark_pending(&mut self, node: NodeId, seq: Seq) {
        if self.st.cursor.is_none_or(|c| seq > c) {
            self.st.pending.insert(node, seq);
        }
    }

    // ------------------------------------------------------------------------------------------
    // Planung
    // ------------------------------------------------------------------------------------------

    /// Plant die nächsten Operationen. Kann ohne Ergebnis aufgerufen werden; liefert dann nichts Neues.
    /// Der Zustand muss danach persistiert werden, bevor die Operationen ausgeführt werden.
    pub fn plan(&mut self) -> Vec<Op> {
        let mut cx = Ctx::default();
        for (id, op) in &self.st.outbox {
            mark_remote_busy(op, &self.st.synced, &mut cx);
            if !self.sent.contains(id) {
                cx.ops.push(Op::Remote(*id, op.clone()));
            }
        }
        for op in cx.ops.iter() {
            if let Op::Remote(id, _) = op {
                self.sent.insert(*id);
            }
        }
        for op in self.inflight_local.values() {
            mark_local_busy(op, &self.st.synced, &mut cx);
        }
        if self.local_root.is_none() || self.st.cursor.is_none() || self.local_untrusted {
            return cx.ops;
        }
        let resent = cx.ops.len();
        let mutations = self.st.synced.mutations();

        // Arbeitsliste: alles (nach Start/vollständigem Scan) oder nur Geändertes und Wartendes.
        let mut initial: Vec<Key> = Vec::new();
        if std::mem::take(&mut self.full) {
            initial.extend(self.st.synced.ids().into_iter().map(Key::Node));
            initial.extend(self.st.remote.ids().into_iter().map(Key::Node));
            initial.extend(self.local.ids().into_iter().map(Key::Local));
        }
        initial.extend(std::mem::take(&mut self.dirty));
        initial.extend(std::mem::take(&mut self.waiting));
        let mut work: BTreeSet<(u8, usize, u64)> =
            initial.into_iter().map(|k| self.order(k)).collect();
        let mut evaluated: BTreeMap<Key, u8> = BTreeMap::new();
        let mut deferred: BTreeSet<Key> = BTreeSet::new();
        while let Some(w) = work.pop_first() {
            let k = unorder(w);
            let count = evaluated.entry(k).or_insert(0);
            if *count >= 3 {
                // Mehrfach in diesem Durchlauf geändert: beim nächsten Mal weiter.
                deferred.insert(k);
                continue;
            }
            *count += 1;
            self.evaluate(k, &mut cx);
            if self.is_settled(k, &cx) {
                self.waiting.remove(&k);
            } else {
                self.waiting.insert(k);
            }
            for d in std::mem::take(&mut self.dirty) {
                if evaluated.get(&d).is_some_and(|c| *c >= 3) {
                    deferred.insert(d);
                } else {
                    work.insert(self.order(d));
                }
            }
        }
        self.dirty.extend(deferred);

        // Sicherheitsnetz: Nichts geplant, nichts verändert, nichts unterwegs – aber Regeln warten
        // aufeinander. Dann löst eine sichere Aktion den Zyklus auf, statt für immer zu hängen.
        let idle = cx.ops.len() == resent
            && self.st.synced.mutations() == mutations
            && self.st.outbox.is_empty()
            && self.inflight_local.is_empty()
            && self.st.pending.is_empty()
            && !self.need_scan
            && !self.need_fetch;
        if idle && !cx.stuck.is_empty() {
            self.stalled += 1;
        } else {
            self.stalled = 0;
        }
        if self.stalled >= STALL_LIMIT
            && let Some(b) = cx.stuck.first().copied()
        {
            self.stalled = 0;
            self.st.breakers_used += 1;
            self.st.last_breaker = Some(format!("{b:?}"));
            match b {
                Breaker::TempLocal(o) => self.yield_local_temp(o, &mut cx),
                Breaker::TempRemote(m) => self.yield_remote_temp(m, &mut cx),
                Breaker::Unlink(n) => {
                    self.s_remove(n);
                    // (s_remove markiert die Nachbarn für die nächste Planung)
                }
            }
        }
        cx.ops
    }

    /// Prüft einen Schlüssel der Arbeitsliste.
    fn evaluate(&mut self, k: Key, cx: &mut Ctx) {
        match k {
            Key::Node(n) => {
                if cx.busy_n.contains(&n) || self.st.pending.contains_key(&n) {
                    return;
                }
                if self.st.synced.contains(n) {
                    self.plan_synced(n, cx);
                } else if self.st.remote.contains(n) {
                    self.plan_remote_new(n, cx);
                }
            }
            Key::Local(l) => {
                if Some(l) == self.local_root || cx.busy_l.contains(&l) {
                    return;
                }
                if let Some(n) = self.st.synced.node_of(l) {
                    self.evaluate(Key::Node(n), cx);
                } else if self.local.contains(l) {
                    self.plan_local_new(l, cx);
                }
            }
        }
    }

    /// Ist für diesen Schlüssel nichts mehr zu tun (oder läuft gerade eine Operation)?
    fn is_settled(&self, k: Key, cx: &Ctx) -> bool {
        match k {
            Key::Node(n) => {
                if cx.busy_n.contains(&n) || self.st.pending.contains_key(&n) {
                    return true;
                }
                let Some(s) = self.st.synced.get(n) else {
                    return !self.st.remote.contains(n);
                };
                if cx.busy_l.contains(&s.local) {
                    return true;
                }
                let (Some(r), Some(le)) = (self.st.remote.get(n), self.local.get(s.local)) else {
                    return false;
                };
                let file = s.kind == Kind::File;
                r.parent == s.parent
                    && r.name == s.name
                    && (!file || (r.content == s.content && r.rev == s.rev))
                    && self.loc_of(s.local) == Some((s.parent, s.name.clone()))
                    && (!file || (le.content == s.content && le.fp == s.fp))
                    && !self.st.local_temps.contains_key(&s.local)
                    && temp_home(&s.name).is_none()
            }
            Key::Local(l) => {
                if Some(l) == self.local_root || cx.busy_l.contains(&l) {
                    return true;
                }
                match self.st.synced.node_of(l) {
                    Some(n) => self.is_settled(Key::Node(n), cx),
                    None => self
                        .local
                        .get(l)
                        .is_none_or(|e| e.name.as_str().starts_with(DOWNLOAD_TEMP_PREFIX)),
                }
            }
        }
    }

    /// Reihenfolge der Arbeitsliste: Knoten vor lokalen Objekten, flache vor tiefen.
    fn order(&self, k: Key) -> (u8, usize, u64) {
        match k {
            Key::Node(n) => {
                let d = self.st.remote.depth(n, self.root()).unwrap_or(0);
                (0, d, n.0)
            }
            Key::Local(l) => {
                let d = self
                    .local_root
                    .and_then(|r| self.local.depth(l, r))
                    .unwrap_or(0);
                (1, d, l.0)
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // Änderungen an den Bäumen – markieren betroffene Nachbarn für die inkrementelle Planung
    // ------------------------------------------------------------------------------------------

    /// Alle an einer Operation Beteiligten neu prüfen (nach Erfolg wie nach Fehlschlag).
    fn mark_local_op(&mut self, op: &LocalOp) {
        match op {
            LocalOp::CreateDir {
                parent,
                node,
                node_parent,
                ..
            }
            | LocalOp::Download {
                parent,
                node,
                node_parent,
                ..
            } => {
                self.dirty.extend([
                    Key::Local(*parent),
                    Key::Node(*node),
                    Key::Node(*node_parent),
                ]);
            }
            LocalOp::Replace { local, node, .. }
            | LocalOp::DeleteFile { local, node, .. }
            | LocalOp::DeleteDir { local, node, .. } => {
                self.dirty.extend([Key::Local(*local), Key::Node(*node)]);
            }
            LocalOp::Move {
                local,
                from_parent,
                parent,
                ..
            } => {
                self.dirty.extend([
                    Key::Local(*local),
                    Key::Local(*from_parent),
                    Key::Local(*parent),
                ]);
            }
        }
    }

    fn mark_remote_op(&mut self, op: &RemoteOp) {
        match op {
            RemoteOp::CreateDir {
                parent,
                source,
                origin,
                ..
            }
            | RemoteOp::CreateFile {
                parent,
                source,
                origin,
                ..
            } => {
                self.dirty.extend([Key::Node(*parent), Key::Local(*source)]);
                if let Origin::ConflictCopy {
                    replaces: Some(r),
                    local_parent,
                    ..
                } = origin
                {
                    self.dirty.extend([Key::Node(*r), Key::Node(*local_parent)]);
                }
            }
            RemoteOp::Upload { node, source, .. } => {
                self.dirty.extend([Key::Node(*node), Key::Local(*source)]);
            }
            RemoteOp::Move { node, parent, .. } => {
                self.dirty.extend([Key::Node(*node), Key::Node(*parent)]);
                if let Some(p) = self.st.remote.get(*node).map(|e| e.parent) {
                    self.dirty.insert(Key::Node(p));
                }
            }
            RemoteOp::DeleteFile { node, .. } | RemoteOp::DeleteDir { node, .. } => {
                self.dirty.insert(Key::Node(*node));
                if let Some(p) = self.st.remote.get(*node).map(|e| e.parent) {
                    self.dirty.insert(Key::Node(p));
                }
            }
        }
    }

    /// Ordner, dessen Existenz oder Verknüpfung sich geändert hat: auch alle Kinder neu prüfen.
    fn mark_node_dir(&mut self, n: NodeId) {
        self.dirty.insert(Key::Node(n));
        let kids: Vec<NodeId> = self.st.remote.children(n).collect();
        self.dirty.extend(kids.into_iter().map(Key::Node));
        if let Some(l) = self.st.synced.get(n).map(|s| s.local) {
            self.mark_local_dir(l);
        }
    }

    fn mark_local_dir(&mut self, l: LocalId) {
        self.dirty.insert(Key::Local(l));
        let kids: Vec<LocalId> = self.local.children(l).collect();
        self.dirty.extend(kids.into_iter().map(Key::Local));
    }

    fn s_changed(&mut self, n: NodeId, old: Option<SyncedEntry>, new: Option<SyncedEntry>) {
        self.dirty.insert(Key::Node(n));
        let structural = old.as_ref().map(|e| (e.local, e.parent))
            != new.as_ref().map(|e| (e.local, e.parent))
            || old.is_none() != new.is_none();
        for e in [old, new].into_iter().flatten() {
            self.dirty.insert(Key::Node(e.parent));
            self.dirty.insert(Key::Local(e.local));
            if let Some(lp) = self.local.get(e.local).map(|le| le.parent) {
                self.dirty.insert(Key::Local(lp));
            }
            if e.kind == Kind::Dir && structural {
                self.mark_local_dir(e.local);
                let kids: Vec<NodeId> = self.st.remote.children(n).collect();
                self.dirty.extend(kids.into_iter().map(Key::Node));
            }
        }
    }

    fn s_insert(&mut self, n: NodeId, e: SyncedEntry) -> bool {
        let old = self.st.synced.get(n).cloned();
        let ok = self.st.synced.insert(n, e);
        if ok {
            let new = self.st.synced.get(n).cloned();
            self.s_changed(n, old, new);
        }
        ok
    }

    fn s_remove(&mut self, n: NodeId) -> Option<SyncedEntry> {
        let old = self.st.synced.remove(n);
        if let Some(o) = &old {
            self.s_changed(n, Some(o.clone()), None);
        }
        old
    }

    fn s_update(&mut self, n: NodeId, f: impl FnOnce(&mut SyncedEntry)) -> bool {
        let old = self.st.synced.get(n).cloned();
        let ok = self.st.synced.update(n, f);
        if ok {
            let new = self.st.synced.get(n).cloned();
            self.s_changed(n, old, new);
        }
        ok
    }

    fn l_changed(&mut self, l: LocalId, old: Option<LocalEntry>, new: Option<LocalEntry>) {
        self.dirty.insert(Key::Local(l));
        if let Some(n) = self.st.synced.node_of(l) {
            self.dirty.insert(Key::Node(n));
        }
        let existence = old.is_none() != new.is_none();
        let was_dir = [&old, &new]
            .into_iter()
            .flatten()
            .any(|e| e.kind == Kind::Dir);
        for e in [old, new].into_iter().flatten() {
            self.dirty.insert(Key::Local(e.parent));
            if let Some(pn) = self.st.synced.node_of(e.parent) {
                self.dirty.insert(Key::Node(pn));
            }
        }
        if was_dir && existence {
            self.mark_local_dir(l);
        }
    }

    fn l_insert(&mut self, l: LocalId, e: LocalEntry) {
        let old = self.local.get(l).cloned();
        if old.as_ref() == Some(&e) {
            return;
        }
        self.local.insert(l, e.clone());
        self.l_changed(l, old, Some(e));
    }

    fn l_remove(&mut self, l: LocalId) -> Option<LocalEntry> {
        let old = self.local.remove(l);
        if let Some(o) = &old {
            self.l_changed(l, Some(o.clone()), None);
        }
        old
    }

    fn l_update(&mut self, l: LocalId, f: impl FnOnce(&mut LocalEntry)) -> bool {
        let Some(mut e) = self.local.get(l).cloned() else {
            return false;
        };
        f(&mut e);
        self.l_insert(l, e);
        true
    }

    fn r_changed(&mut self, n: NodeId, old: Option<RemoteEntry>, new: Option<RemoteEntry>) {
        self.dirty.insert(Key::Node(n));
        let existence = old.is_none() != new.is_none();
        let is_dir = [&old, &new]
            .into_iter()
            .flatten()
            .any(|e| e.kind == Kind::Dir);
        for e in [old, new].into_iter().flatten() {
            self.dirty.insert(Key::Node(e.parent));
        }
        if is_dir && existence {
            self.mark_node_dir(n);
        }
    }

    fn r_insert(&mut self, n: NodeId, e: RemoteEntry) {
        let old = self.st.remote.get(n).cloned();
        if old.as_ref() == Some(&e) {
            return;
        }
        self.st.remote.insert(n, e.clone());
        self.r_changed(n, old, Some(e));
    }

    fn r_remove(&mut self, n: NodeId) {
        if let Some(old) = self.st.remote.remove(n) {
            self.r_changed(n, Some(old), None);
        }
    }

    /// Nur für Tests/Simulator: Findet eine vollständige Planung Arbeit, die die inkrementelle
    /// Planung übersehen hat? Aufzurufen direkt nach einer Planung ohne Ergebnis.
    pub fn verify_incremental(&self) -> Result<(), String> {
        let mut probe = self.clone();
        probe.full = true;
        probe.stalled = 0;
        let before = probe.st.synced.clone();
        let ops: Vec<Op> = probe
            .plan()
            .into_iter()
            .filter(|op| match op {
                Op::Remote(id, _) => !self.st.outbox.contains_key(id),
                Op::Local(..) => true,
            })
            .collect();
        if !ops.is_empty() {
            return Err(format!("vollständige Planung findet noch Arbeit: {ops:?}"));
        }
        if !probe.st.synced.same_entries(&before) {
            return Err("vollständige Planung ändert noch den vereinbarten Stand".into());
        }
        Ok(())
    }

    fn plan_synced(&mut self, n: NodeId, cx: &mut Ctx) {
        let Some(s) = self.st.synced.get(n).cloned() else {
            return;
        };
        if cx.busy_l.contains(&s.local) {
            return;
        }
        let r = self.st.remote.get(n).cloned();
        let l = self.local.get(s.local).cloned();
        match (r, l) {
            (None, None) => {
                self.s_remove(n);
            }
            (None, Some(le)) => self.plan_remote_gone(n, &s, &le, cx),
            (Some(re), None) => self.plan_local_gone(n, &s, &re, cx),
            (Some(re), Some(le)) => self.plan_both(n, s, &re, &le, cx),
        }
    }

    /// Auf dem Server gelöscht, lokal vorhanden.
    fn plan_remote_gone(&mut self, n: NodeId, s: &SyncedEntry, le: &LocalEntry, cx: &mut Ctx) {
        if self.local_differs(s, le) {
            // Lokale Änderung gewinnt gegen das Löschen: Verknüpfung lösen, das Objekt wird neu hochgeladen.
            // (Zuerst prüfen: Eine lokale Änderung darf nie einem neuen Server-Knoten zugeschlagen werden.)
            self.s_remove(n);
            return;
        }
        // „Atomic Save“ auf dem Server: Am selben Ort liegt jetzt ein neuer Knoten gleicher Art.
        // Dann ist das (unveränderte) lokale Objekt dessen Vorgänger: neu verknüpfen statt löschen
        // und neu laden. Der neue Inhalt kommt danach als gewöhnliche Server-Änderung.
        if let Some(m) = self.eff_occupant(s.parent, &s.name, Some(n))
            && !self.st.synced.contains(m)
            && !self.st.pending.contains_key(&m)
            && !cx.busy_n.contains(&m)
            && let Some(rm) = self.st.remote.get(m).cloned()
            && rm.kind == s.kind
            && rm.name == s.name
        {
            self.s_remove(n);
            self.s_insert(
                m,
                SyncedEntry {
                    rev: rm.rev,
                    ..s.clone()
                },
            );
            if s.kind == Kind::Dir {
                // Die vereinbarten Kinder gehören jetzt zum neuen Ordner. Sonst sähe es so aus, als
                // hätte man sie lokal hineinverschoben, und auf dem Server gelöschte Kinder kämen zurück.
                for c in self.st.synced.children(n) {
                    self.s_update(c, |e| e.parent = m);
                }
            }
            return;
        }
        match s.kind {
            Kind::File => {
                let (Some(fp), Some(content)) = (le.fp, le.content) else {
                    self.need_scan = true;
                    return;
                };
                self.emit_local(
                    cx,
                    LocalOp::DeleteFile {
                        local: s.local,
                        parent: le.parent,
                        name: le.name.clone(),
                        expect: Expected { fp, content },
                        node: n,
                    },
                );
            }
            Kind::Dir => {
                // Kinder entscheiden: Was gelöscht wird oder wegzieht, abwarten. Was bleiben muss
                // (neu, lokal hineinverschoben), holt den Ordner auf den Server zurück.
                let mut deleting = false; // Kinder, die gelöscht werden (oder sich lösen)
                let mut leaving = false; // Kinder, die der Server woanders hin verschoben hat
                let mut keep = false; // Kinder, die hier bleiben müssen
                for k in self.local.children(s.local) {
                    match self.st.synced.node_of(k) {
                        None => keep = true,
                        Some(m) => {
                            let s_loc = self.st.synced.get(m).map(|x| (x.parent, x.name.clone()));
                            match self.eff_remote_loc(m) {
                                None => deleting = true,
                                Some(r) if Some(&r) != s_loc.as_ref() => leaving = true,
                                Some(_) => keep = true, // lokal hineinverschoben
                            }
                        }
                    }
                }
                // Erst die Löschungen abwarten, damit unveränderte Kinder nicht mit zurückkommen.
                // Wegziehende Kinder blockieren nicht: Ihr Ziel kann davon abhängen, dass dieser
                // Ordner verschwindet oder neu angelegt wird.
                if deleting || (leaving && !keep) {
                    cx.stuck.push(Breaker::Unlink(n));
                    return;
                }
                if keep {
                    // Es liegt noch etwas darin, das bleiben muss: Ordner auf dem Server neu anlegen.
                    self.s_remove(n);
                } else {
                    self.emit_local(
                        cx,
                        LocalOp::DeleteDir {
                            local: s.local,
                            parent: le.parent,
                            name: le.name.clone(),
                            node: n,
                        },
                    );
                }
            }
        }
    }

    /// Lokal gelöscht, auf dem Server vorhanden.
    fn plan_local_gone(&mut self, n: NodeId, s: &SyncedEntry, re: &RemoteEntry, cx: &mut Ctx) {
        if remote_differs(s, re) {
            // Änderung auf dem Server gewinnt gegen das lokale Löschen: neu herunterladen.
            self.s_remove(n);
            return;
        }
        match s.kind {
            Kind::File => self.emit_remote(
                cx,
                RemoteOp::DeleteFile {
                    node: n,
                    base_rev: re.rev,
                    parent: re.parent,
                    name: re.name.clone(),
                },
            ),
            Kind::Dir => {
                // Spiegelbildlich: Was auf dem Server neu oder geändert darin liegt, holt den Ordner
                // lokal zurück. Unveränderte Kinder werden gelöscht oder ziehen weg: abwarten.
                let mut deleting = false; // lokal gelöscht, auf dem Server unverändert
                let mut leaving = false; // lokal woanders hin verschoben
                let mut keep = false; // muss hier bleiben (neu oder auf dem Server geändert)
                for m in self.eff_children(n) {
                    match self.st.synced.get(m) {
                        None => keep = true,
                        Some(sm) => {
                            let r = self.st.remote.get(m);
                            let moved = self.eff_remote_loc(m).as_ref()
                                != Some(&(sm.parent, sm.name.clone()));
                            let edited = sm.kind == Kind::File
                                && !self.st.pending.contains_key(&m)
                                && r.is_some_and(|r| r.content != sm.content);
                            if moved || edited || self.push_would_cycle(m) {
                                // geändert, oder die lokale Verschiebung kann nicht hochgeladen
                                // werden (Zyklus) und das Kind kehrt hierher zurück
                                keep = true;
                            } else if self.local.contains(sm.local) {
                                leaving = true;
                            } else {
                                deleting = true;
                            }
                        }
                    }
                }
                if deleting || (leaving && !keep) {
                    cx.stuck.push(Breaker::Unlink(n));
                    return;
                }
                if keep {
                    // Auf dem Server liegt darin etwas, das bleiben muss: Ordner lokal wiederherstellen.
                    self.s_remove(n);
                } else {
                    self.emit_remote(
                        cx,
                        RemoteOp::DeleteDir {
                            node: n,
                            parent: re.parent,
                            name: re.name.clone(),
                        },
                    );
                }
            }
        }
    }

    /// Auf beiden Seiten vorhanden.
    fn plan_both(
        &mut self,
        n: NodeId,
        mut s: SyncedEntry,
        re: &RemoteEntry,
        le: &LocalEntry,
        cx: &mut Ctx,
    ) {
        if s.kind == Kind::File {
            // Gleicher Inhalt, aber neuer Fingerprint (z.B. `touch`) bzw. neue Revision (gleicher
            // Inhalt erneut hochgeladen): nur S auffrischen, damit Vorbedingungen aktuell bleiben.
            let fresh_fp = le.content == s.content && le.fp != s.fp;
            let fresh_rev = re.content == s.content && re.rev != s.rev;
            if fresh_fp || fresh_rev {
                self.s_update(n, |e| {
                    if fresh_fp {
                        e.fp = le.fp;
                    }
                    if fresh_rev {
                        e.rev = re.rev;
                    }
                });
                if let Some(updated) = self.st.synced.get(n) {
                    s = updated.clone();
                }
            }
        }

        let r_loc = (re.parent, re.name.clone());
        let s_loc = (s.parent, s.name.clone());
        let l_loc = self.loc_of(s.local);
        let r_moved = r_loc != s_loc;
        let l_moved = l_loc.as_ref() != Some(&s_loc);
        if r_moved || l_moved {
            if r_moved && l_moved && l_loc.as_ref() == Some(&r_loc) {
                self.s_update(n, |e| {
                    e.parent = r_loc.0;
                    e.name = r_loc.1.clone();
                });
            } else if r_moved {
                self.local_move_to(n, s.local, le, &r_loc, cx);
            } else {
                self.remote_move_to_local(n, s.local, le, l_loc, &r_loc, cx);
            }
            return; // Inhalt erst, wenn der Ort geklärt ist
        }
        // Liegt das Objekt noch auf einem eigenen Ausweichnamen, obwohl es nirgendwohin mehr muss
        // (z.B. weil der Server die Verschiebung zurückgenommen hat): zurück an den echten Ort.
        if self.st.local_temps.contains_key(&s.local) && self.local_loc(le).as_ref() != Some(&r_loc)
        {
            self.local_move_to(n, s.local, le, &r_loc, cx);
            return;
        }

        if s.kind != Kind::File {
            self.cleanup_temp_name(s.local, le, cx);
            return;
        }
        let (Some(lfp), Some(lc)) = (le.fp, le.content) else {
            self.need_scan = true;
            return;
        };
        let Some(rc) = re.content else { return };
        let remote_changed = Some(rc) != s.content;
        let local_changed = Some(lc) != s.content;
        match (remote_changed, local_changed) {
            (false, false) => self.cleanup_temp_name(s.local, le, cx),
            (true, false) => self.emit_local(
                cx,
                LocalOp::Replace {
                    local: s.local,
                    parent: le.parent,
                    name: le.name.clone(),
                    expect: Expected {
                        fp: lfp,
                        content: lc,
                    },
                    node: n,
                    rev: re.rev,
                    content: rc,
                },
            ),
            (false, true) => self.emit_remote(
                cx,
                RemoteOp::Upload {
                    node: n,
                    base_rev: s.rev,
                    content: lc,
                    source: s.local,
                    fp: lfp,
                },
            ),
            (true, true) if rc == lc => {
                self.s_update(n, |e| {
                    e.content = Some(rc);
                    e.rev = re.rev;
                    e.fp = Some(lfp);
                });
            }
            (true, true) => {
                // Echter Konflikt: Unseren Inhalt als Kopie sichern, danach das Original herunterladen.
                let name = self.conflict_name(s.parent, Some(le.parent), &s.name);
                self.emit_remote(
                    cx,
                    RemoteOp::CreateFile {
                        parent: s.parent,
                        name,
                        content: lc,
                        source: s.local,
                        fp: lfp,
                        origin: Origin::ConflictCopy {
                            local_parent: s.parent,
                            local_name: s.name.clone(),
                            replaces: Some(n),
                        },
                    },
                );
            }
        }
    }

    /// Lokales Objekt an den Server-Ort verschieben (Server-Änderung übernehmen oder Server gewinnt).
    fn local_move_to(
        &mut self,
        n: NodeId,
        l: LocalId,
        le: &LocalEntry,
        target: &(NodeId, Name),
        cx: &mut Ctx,
    ) {
        let Some(lp) = self.local_of_node(target.0) else {
            return; // Zielordner existiert lokal (noch) nicht
        };
        if lp == l || self.local.is_within(lp, l) {
            return; // Ziel liegt lokal innerhalb des Objekts; löst sich über die anderen Regeln auf
        }
        match self.local_occupant(lp, &target.1, Some(l)) {
            None => self.emit_local(
                cx,
                LocalOp::Move {
                    local: l,
                    from_parent: le.parent,
                    from_name: le.name.clone(),
                    parent: lp,
                    name: target.1.clone(),
                    synced_to: Some(target.clone()),
                },
            ),
            Some(o) => {
                let Some(m) = self.st.synced.node_of(o) else {
                    return; // unverknüpftes Objekt: dessen Regel weicht aus
                };
                if cx.busy_l.contains(&o) || cx.busy_n.contains(&m) {
                    return;
                }
                // Groß-/Kleinschreibungsvariante auf dem Server (z.B. per SMB „b“ → „a“ neben „A“):
                // o bleibt, wo es ist, lokal gibt es aber nur einen Namen für beide. Wie bei neuen
                // Knoten weicht der Ankömmling auf dem Server auf einen Konfliktnamen aus.
                if let Some(mt) = self.eff_remote_loc(m)
                    && mt.0 == target.0
                    && mt.1 != target.1
                    && self.loc_of(o).as_ref() == Some(&mt)
                {
                    let name = self.conflict_name(target.0, Some(lp), &target.1);
                    self.emit_remote(
                        cx,
                        RemoteOp::Move {
                            node: n,
                            from_parent: target.0,
                            from_name: target.1.clone(),
                            parent: target.0,
                            name,
                        },
                    );
                    return;
                }
                // Kann o selbst nicht weg, weil sein Ziel lokal in ihm liegt, muss es zuerst Platz machen.
                let o_target_nested = self
                    .eff_remote_loc(m)
                    .and_then(|t| self.local_of_node(t.0))
                    .is_some_and(|tl| self.local.is_within(tl, o));
                if self.local_cycle(o, l)
                    || self.deleted_remotely_with_content(o)
                    || o_target_nested
                {
                    // Tausch-Zyklus, oder der Platzhalter ist ein gelöschter Ordner, dessen Inhalt
                    // erst wegziehen muss (womöglich hierher): Platzhalter weicht aus.
                    self.yield_local_temp(o, cx);
                } else {
                    cx.stuck.push(Breaker::TempLocal(o));
                }
            }
        }
    }

    /// Lokale Ortsänderung auf den Server übertragen.
    fn remote_move_to_local(
        &mut self,
        n: NodeId,
        l: LocalId,
        le: &LocalEntry,
        l_loc: Option<(NodeId, Name)>,
        r_loc: &(NodeId, Name),
        cx: &mut Ctx,
    ) {
        let Some((p, name)) = l_loc else {
            return; // lokaler Elternordner noch nicht auf dem Server
        };
        if !self.eff_exists(p) {
            return; // Elternordner auf dem Server gelöscht; dessen Regel stellt ihn wieder her
        }
        if p == n || self.eff_is_within(p, n) {
            // Würde auf dem Server einen Zyklus erzeugen: Server gewinnt.
            self.local_move_to(n, l, le, r_loc, cx);
            return;
        }
        match self.eff_occupant(p, &name, Some(n)) {
            None => self.emit_remote(
                cx,
                RemoteOp::Move {
                    node: n,
                    from_parent: r_loc.0,
                    from_name: r_loc.1.clone(),
                    parent: p,
                    name,
                },
            ),
            Some(m) => {
                if cx.busy_n.contains(&m) || self.st.pending.contains_key(&m) {
                    return;
                }
                let Some(sm) = self.st.synced.get(m).cloned() else {
                    // Auf dem Server neu angelegtes Objekt hat den Namen: lokal ausweichen.
                    self.yield_local_name(l, le, p, cx);
                    return;
                };
                let s_loc = (sm.parent, sm.name.clone());
                let m_moved_in = self.eff_remote_loc(m).as_ref() != Some(&s_loc);
                let m_in_place = self.loc_of(sm.local).as_ref() == Some(&s_loc);
                let m_blocked = self.local.contains(sm.local) && self.loc_of(sm.local).is_none();
                let m_deleted_locally = !self.local.contains(sm.local);
                if m_moved_in || m_in_place || self.push_would_cycle(m) {
                    self.yield_local_name(l, le, p, cx);
                } else if m_blocked
                    || self.remote_cycle(m, n)
                    || (m_deleted_locally && self.eff_is_within(n, m))
                    || self.deleted_locally_with_content(m)
                {
                    // Zyklus, oder der Platzhalter ist ein lokal gelöschter Ordner, dessen Inhalt
                    // auf dem Server erst wegziehen muss: Platzhalter weicht auf dem Server aus.
                    self.yield_remote_temp(m, cx);
                } else {
                    cx.stuck.push(Breaker::TempRemote(m));
                }
                // sonst: m verlässt den Namen auf dem Server bald (lokal verschoben/gelöscht) → warten
            }
        }
    }

    /// Auf dem Server vorhanden, (noch) nicht verknüpft.
    fn plan_remote_new(&mut self, n: NodeId, cx: &mut Ctx) {
        let Some(re) = self.st.remote.get(n).cloned() else {
            return;
        };
        let Some(lp) = self.local_of_node(re.parent) else {
            return;
        };
        match self.local_occupant(lp, &re.name, None) {
            None => match re.kind {
                Kind::Dir => self.emit_local(
                    cx,
                    LocalOp::CreateDir {
                        parent: lp,
                        name: re.name.clone(),
                        node: n,
                        node_parent: re.parent,
                    },
                ),
                Kind::File => {
                    let Some(content) = re.content else { return };
                    self.emit_local(
                        cx,
                        LocalOp::Download {
                            parent: lp,
                            name: re.name.clone(),
                            node: n,
                            node_parent: re.parent,
                            rev: re.rev,
                            content,
                        },
                    );
                }
            },
            Some(o) => {
                let Some(m) = self.st.synced.node_of(o) else {
                    return; // unverknüpftes lokales Objekt: wird verknüpft oder weicht aus
                };
                if cx.busy_l.contains(&o)
                    || cx.busy_n.contains(&m)
                    || self.st.pending.contains_key(&m)
                {
                    return;
                }
                let Some(target) = self.eff_remote_loc(m) else {
                    // m wurde auf dem Server gelöscht; die lokale Löschung folgt. Muss dafür erst
                    // Inhalt wegziehen (womöglich in n), weicht der Ordner aus.
                    if self.deleted_remotely_with_content(o) {
                        self.yield_local_temp(o, cx);
                    }
                    return;
                };
                let o_loc = self.loc_of(o);
                if Some(&target) != o_loc.as_ref() {
                    // m zieht lokal noch weg (Server hat es verschoben). Fehlt dafür der Zielordner
                    // (z.B. weil er genau hier entstehen soll), weicht m vorübergehend aus, sonst
                    // warten beide aufeinander. Hat dagegen der Nutzer m lokal hierher verschoben,
                    // weicht m über seine eigene Regel aus.
                    let remote_moved = self
                        .st
                        .synced
                        .get(m)
                        .is_some_and(|sm| (sm.parent, sm.name.clone()) != target);
                    let blocked = match self.local_of_node(target.0) {
                        None => true,                            // Zielordner fehlt noch
                        Some(tl) => self.local.is_within(tl, o), // Ziel liegt in o selbst
                    };
                    if remote_moved && blocked {
                        self.yield_local_temp(o, cx);
                    } else if remote_moved {
                        cx.stuck.push(Breaker::TempLocal(o));
                    }
                    return;
                }
                // m bleibt: Namen kollidieren nur lokal (Groß-/Kleinschreibung). Auf dem Server umbenennen.
                if target.1 != re.name {
                    let name = self.conflict_name(re.parent, Some(lp), &re.name);
                    self.emit_remote(
                        cx,
                        RemoteOp::Move {
                            node: n,
                            from_parent: re.parent,
                            from_name: re.name.clone(),
                            parent: re.parent,
                            name,
                        },
                    );
                }
            }
        }
    }

    /// Lokal vorhanden, (noch) nicht verknüpft.
    fn plan_local_new(&mut self, l: LocalId, cx: &mut Ctx) {
        let Some(le) = self.local.get(l).cloned() else {
            return;
        };
        if le.name.as_str().starts_with(DOWNLOAD_TEMP_PREFIX) {
            return; // halbfertiger Download des Ausführenden, nie hochladen
        }
        if temp_home(&le.name).is_some() {
            // Ein (nicht mehr verknüpftes) Objekt auf einem Ausweichnamen: erst zurückbenennen,
            // damit der temporäre Name nie auf den Server gelangt.
            self.st.local_temps.remove(&l);
            self.cleanup_temp_name(l, &le, cx);
            return;
        }
        let Some(p) = self.local_parent_node(le.parent) else {
            return; // Elternordner zuerst
        };
        if !self.eff_exists(p) {
            return;
        }
        if le.kind == Kind::File && (le.content.is_none() || le.fp.is_none()) {
            self.need_scan = true;
            return;
        }
        match self.eff_occupant(p, &le.name, None) {
            None => self.emit_create(cx, p, le.name.clone(), l, &le, Origin::New),
            Some(m) => {
                if cx.busy_n.contains(&m) || self.st.pending.contains_key(&m) {
                    return;
                }
                if let Some(sm) = self.st.synced.get(m).cloned() {
                    let s_loc = (sm.parent, sm.name.clone());
                    let m_moved_in = self.eff_remote_loc(m).as_ref() != Some(&s_loc);
                    let m_in_place = self.loc_of(sm.local).as_ref() == Some(&s_loc);
                    if m_moved_in || m_in_place || self.push_would_cycle(m) {
                        self.emit_conflict_create(cx, p, l, &le);
                    } else if (self.local.contains(sm.local) && self.loc_of(sm.local).is_none())
                        || self.deleted_locally_with_content(m)
                    {
                        // m liegt lokal in einem noch nicht hochgeladenen Ordner und blockiert den Namen,
                        // den dieser Ordner (oder ein Vorfahr) braucht, oder m ist ein lokal gelöschter
                        // Ordner, dessen Inhalt auf dem Server erst wegziehen muss: m weicht kurz aus.
                        self.yield_remote_temp(m, cx);
                    }
                    // sonst: m verlässt den Namen bald → warten
                } else if let Some(rm) = self.st.remote.get(m).cloned() {
                    let same_local_key = self.local.key(&rm.name) == self.local.key(&le.name);
                    let same = rm.kind == le.kind
                        && same_local_key
                        && (rm.kind == Kind::Dir || rm.content == le.content);
                    if same {
                        // Beide Seiten haben dasselbe angelegt (z.B. Ersteinrichtung): nur verknüpfen.
                        self.s_insert(
                            m,
                            SyncedEntry {
                                parent: p,
                                name: le.name.clone(),
                                kind: rm.kind,
                                content: rm.content,
                                rev: rm.rev,
                                local: l,
                                fp: le.fp,
                            },
                        );
                    } else {
                        self.emit_conflict_create(cx, p, l, &le);
                    }
                }
            }
        }
    }

    fn emit_create(
        &mut self,
        cx: &mut Ctx,
        parent: NodeId,
        name: Name,
        l: LocalId,
        le: &LocalEntry,
        origin: Origin,
    ) {
        match le.kind {
            Kind::Dir => self.emit_remote(
                cx,
                RemoteOp::CreateDir {
                    parent,
                    name,
                    source: l,
                    origin,
                },
            ),
            Kind::File => {
                let (Some(content), Some(fp)) = (le.content, le.fp) else {
                    self.need_scan = true;
                    return;
                };
                self.emit_remote(
                    cx,
                    RemoteOp::CreateFile {
                        parent,
                        name,
                        content,
                        source: l,
                        fp,
                        origin,
                    },
                );
            }
        }
    }

    fn emit_conflict_create(&mut self, cx: &mut Ctx, p: NodeId, l: LocalId, le: &LocalEntry) {
        let name = self.conflict_name(p, Some(le.parent), &le.name);
        let origin = Origin::ConflictCopy {
            local_parent: p,
            local_name: le.name.clone(),
            replaces: None,
        };
        self.emit_create(cx, p, name, l, le, origin);
    }

    /// Ein vollständig synchronisiertes Objekt trägt noch einen temporären Ausweichnamen
    /// (z.B. nach Absturz oder gleichzeitigem Löschen): auf den Heimatnamen zurückbenennen.
    /// Die Umbenennung ist lokal und wird danach wie jede Nutzer-Umbenennung hochgeladen.
    fn cleanup_temp_name(&mut self, l: LocalId, le: &LocalEntry, cx: &mut Ctx) {
        if self.st.local_temps.contains_key(&l) {
            return;
        }
        let Some(home) = temp_home(&le.name) else {
            return;
        };
        let Some(p) = self.local_parent_node(le.parent) else {
            return;
        };
        let name = if self.name_free(Some(p), Some(le.parent), &home) {
            home
        } else {
            self.conflict_name(p, Some(le.parent), &home)
        };
        self.emit_local(
            cx,
            LocalOp::Move {
                local: l,
                from_parent: le.parent,
                from_name: le.name.clone(),
                parent: le.parent,
                name,
                synced_to: None,
            },
        );
    }

    /// Lokales Objekt weicht auf einen Konfliktnamen aus (rein lokal; danach wird der neue Name hochgeladen).
    fn yield_local_name(&mut self, l: LocalId, le: &LocalEntry, p: NodeId, cx: &mut Ctx) {
        let name = self.conflict_name(p, Some(le.parent), &le.name);
        self.emit_local(
            cx,
            LocalOp::Move {
                local: l,
                from_parent: le.parent,
                from_name: le.name.clone(),
                parent: le.parent,
                name,
                synced_to: None,
            },
        );
    }

    /// Bricht einen lokalen Tausch-Zyklus: `o` vorübergehend auf einen freien Namen schieben.
    fn yield_local_temp(&mut self, o: LocalId, cx: &mut Ctx) {
        let Some(oe) = self.local.get(o).cloned() else {
            return;
        };
        let name = self.temp_name(None, Some(oe.parent), &oe.name);
        self.st.local_temps.insert(o, (oe.parent, name.clone()));
        self.emit_local(
            cx,
            LocalOp::Move {
                local: o,
                from_parent: oe.parent,
                from_name: oe.name.clone(),
                parent: oe.parent,
                name,
                synced_to: None,
            },
        );
    }

    /// Bricht einen Tausch-Zyklus auf dem Server: `m` vorübergehend auf einen freien Namen schieben.
    fn yield_remote_temp(&mut self, m: NodeId, cx: &mut Ctx) {
        let Some((p, home)) = self.eff_remote_loc(m) else {
            return;
        };
        let name = self.temp_name(Some(p), None, &home);
        self.emit_remote(
            cx,
            RemoteOp::Move {
                node: m,
                from_parent: p,
                from_name: home,
                parent: p,
                name,
            },
        );
    }

    /// Lokaler Ordner, dessen Knoten auf dem Server gelöscht ist, der aber noch Inhalt hat, der erst
    /// wegziehen muss. Steht er dabei im Weg, kann er Teil eines Warte-Zyklus sein.
    fn deleted_remotely_with_content(&self, o: LocalId) -> bool {
        let Some(m) = self.st.synced.node_of(o) else {
            return false;
        };
        self.eff_remote_loc(m).is_none()
            && self.local.get(o).is_some_and(|e| e.kind == Kind::Dir)
            && self.local.has_children(o)
    }

    /// Spiegelbild: Server-Ordner, der lokal gelöscht ist, aber auf dem Server noch Inhalt hat.
    fn deleted_locally_with_content(&self, m: NodeId) -> bool {
        let Some(sm) = self.st.synced.get(m) else {
            return false;
        };
        sm.kind == Kind::Dir && !self.local.contains(sm.local) && !self.eff_children(m).is_empty()
    }

    /// Würde das Hochladen der lokalen Ortsänderung von `m` auf dem Server einen Zyklus erzeugen?
    /// Dann gewinnt der Server, und `m` bleibt (bzw. kehrt zurück) an seinem Server-Ort.
    fn push_would_cycle(&self, m: NodeId) -> bool {
        let Some(sm) = self.st.synced.get(m) else {
            return false;
        };
        let Some((p, _)) = self.loc_of(sm.local) else {
            return false;
        };
        p == m || self.eff_is_within(p, m)
    }

    /// Folgt der Kette „o will an einen Ort, an dem schon jemand liegt, der ebenfalls weg will …“.
    /// `true`, wenn sie bei `l` ankommt (lokaler Tausch-Zyklus).
    fn local_cycle(&self, o: LocalId, l: LocalId) -> bool {
        let mut cur = o;
        for _ in 0..MAX_CHAIN {
            let Some(m) = self.st.synced.node_of(cur) else {
                return false;
            };
            let Some(target) = self.eff_remote_loc(m) else {
                return false;
            };
            let next = match self.local_of_node(target.0) {
                Some(tl) => {
                    let Some(ce) = self.local.get(cur) else {
                        return false;
                    };
                    if ce.parent == tl && self.local.key(&ce.name) == self.local.key(&target.1) {
                        return false; // cur ist schon am Ziel
                    }
                    self.local_occupant(tl, &target.1, Some(cur))
                }
                None => {
                    // Der Zielordner fehlt lokal und muss erst entstehen (ggf. samt fehlender
                    // Vorfahren): Wer belegt den Platz des obersten fehlenden Ordners?
                    let mut t = target.0;
                    let mut found = None;
                    for _ in 0..MAX_CHAIN {
                        let Some((tp, tn)) = self.eff_remote_loc(t) else {
                            return false;
                        };
                        if let Some(tpl) = self.local_of_node(tp) {
                            found = Some(self.local_occupant(tpl, &tn, None));
                            break;
                        }
                        t = tp;
                    }
                    match found {
                        Some(occ) => occ,
                        None => return false,
                    }
                }
            };
            match next {
                None => return false,
                Some(next) if next == l => return true,
                Some(next) => cur = next,
            }
        }
        false
    }

    /// Wie [`Self::local_cycle`], aber für gewünschte Verschiebungen auf dem Server.
    fn remote_cycle(&self, m: NodeId, n: NodeId) -> bool {
        let mut cur = m;
        for _ in 0..MAX_CHAIN {
            let Some(sc) = self.st.synced.get(cur) else {
                return false;
            };
            let Some((p, name)) = self.loc_of(sc.local) else {
                return false;
            };
            match self.eff_occupant(p, &name, Some(cur)) {
                None => return false,
                Some(next) if next == n => return true,
                Some(next) => cur = next,
            }
        }
        false
    }

    // ------------------------------------------------------------------------------------------
    // Hilfsfunktionen
    // ------------------------------------------------------------------------------------------

    fn new_op_id(&mut self) -> OpId {
        let id = OpId(self.st.next_op);
        self.st.next_op += 1;
        id
    }

    fn emit_remote(&mut self, cx: &mut Ctx, op: RemoteOp) {
        let id = self.new_op_id();
        mark_remote_busy(&op, &self.st.synced, cx);
        self.st.outbox.insert(id, op.clone());
        self.sent.insert(id);
        cx.ops.push(Op::Remote(id, op));
    }

    fn emit_local(&mut self, cx: &mut Ctx, op: LocalOp) {
        let id = self.new_op_id();
        mark_local_busy(&op, &self.st.synced, cx);
        self.inflight_local.insert(id, op.clone());
        cx.ops.push(Op::Local(id, op));
    }

    /// Ort eines lokalen Objekts im Knoten-Raum. `None`, wenn der lokale Elternordner noch unverknüpft ist.
    fn local_loc(&self, le: &LocalEntry) -> Option<(NodeId, Name)> {
        self.local_parent_node(le.parent)
            .map(|p| (p, le.name.clone()))
    }

    /// Ort eines lokalen Objekts für den Abgleich. Liegt es auf einem eigenen temporären
    /// Ausweichnamen, zählt sein vereinbarter Ort (das Ausweichen ist keine Nutzeränderung).
    fn loc_of(&self, l: LocalId) -> Option<(NodeId, Name)> {
        let le = self.local.get(l)?;
        if let Some((tp, tn)) = self.st.local_temps.get(&l)
            && le.parent == *tp
            && le.name == *tn
            && let Some(n) = self.st.synced.node_of(l)
            && let Some(s) = self.st.synced.get(n)
        {
            return Some((s.parent, s.name.clone()));
        }
        self.local_loc(le)
    }

    fn local_parent_node(&self, lp: LocalId) -> Option<NodeId> {
        if Some(lp) == self.local_root {
            Some(self.root())
        } else {
            self.st.synced.node_of(lp)
        }
    }

    /// Lokaler Ordner zu einem Knoten, falls er lokal existiert.
    fn local_of_node(&self, n: NodeId) -> Option<LocalId> {
        if n == self.root() {
            return self.local_root;
        }
        let s = self.st.synced.get(n)?;
        self.local
            .get(s.local)
            .is_some_and(|e| e.kind == Kind::Dir)
            .then_some(s.local)
    }

    fn local_occupant(&self, lp: LocalId, name: &Name, except: Option<LocalId>) -> Option<LocalId> {
        self.local.lookup(lp, name).find(|o| Some(*o) != except)
    }

    fn local_differs(&self, s: &SyncedEntry, le: &LocalEntry) -> bool {
        self.loc_of(s.local) != Some((s.parent, s.name.clone()))
            || (s.kind == Kind::File && le.content != s.content)
    }

    /// Server-Ort eines Knotens unter Berücksichtigung eigener, in R noch nicht sichtbarer Operationen.
    fn eff_remote_loc(&self, n: NodeId) -> Option<(NodeId, Name)> {
        if self.st.pending.contains_key(&n) {
            return self.st.synced.get(n).map(|s| (s.parent, s.name.clone()));
        }
        self.st.remote.get(n).map(|r| (r.parent, r.name.clone()))
    }

    fn eff_exists(&self, n: NodeId) -> bool {
        n == self.root() || self.eff_remote_loc(n).is_some()
    }

    fn eff_children(&self, p: NodeId) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .st
            .remote
            .children(p)
            .filter(|c| !self.st.pending.contains_key(c))
            .collect();
        for c in self.st.pending.keys() {
            if self.st.synced.get(*c).is_some_and(|s| s.parent == p) {
                v.push(*c);
            }
        }
        v
    }

    fn eff_occupant(&self, p: NodeId, name: &Name, except: Option<NodeId>) -> Option<NodeId> {
        let found = self
            .st
            .remote
            .lookup(p, name)
            .find(|c| Some(*c) != except && !self.st.pending.contains_key(c));
        if found.is_some() {
            return found;
        }
        let key = self.st.remote.key(name);
        self.st.pending.keys().copied().find(|c| {
            Some(*c) != except
                && self
                    .st
                    .synced
                    .get(*c)
                    .is_some_and(|s| s.parent == p && self.st.remote.key(&s.name) == key)
        })
    }

    /// Liegt `id` auf dem Server (effektiv) gleich `ancestor` oder darunter?
    fn eff_is_within(&self, id: NodeId, ancestor: NodeId) -> bool {
        let mut cur = id;
        for _ in 0..=(self.st.remote.len() + self.st.pending.len() + 1) {
            if cur == ancestor {
                return true;
            }
            if cur == self.root() {
                return false;
            }
            match self.eff_remote_loc(cur) {
                Some((p, _)) => cur = p,
                None => return false,
            }
        }
        // Zyklus: vorsichtshalber „ja“ (verhindert eine Verschiebung)
        true
    }

    /// Freier Konfliktname „Name (Konflikt Gerät N).ext“ – frei auf dem Server und lokal.
    fn conflict_name(
        &mut self,
        remote_parent: NodeId,
        local_parent: Option<LocalId>,
        base: &Name,
    ) -> Name {
        loop {
            self.st.name_counter += 1;
            let suffix = format!("Konflikt {} {}", self.device(), self.st.name_counter);
            let cand = base.with_suffix(&suffix);
            if self.name_free(Some(remote_parent), local_parent, &cand) {
                return cand;
            }
        }
    }

    /// Temporärer Ausweichname. Er trägt den Heimatnamen in sich („.xlrx-tmp-Gerät-7~Bericht.txt“),
    /// damit jedes Gerät ein dort hängengebliebenes Objekt zurückbenennen kann.
    fn temp_name(
        &mut self,
        remote_parent: Option<NodeId>,
        local_parent: Option<LocalId>,
        home: &Name,
    ) -> Name {
        loop {
            self.st.name_counter += 1;
            let prefix = format!(
                "{TEMP_PREFIX}{}-{}~",
                self.temp_device(),
                self.st.name_counter
            );
            let mut raw = format!("{prefix}{}", home.as_str());
            let mut cut = raw.len().min(xlrx_proto::name::MAX_NAME_BYTES);
            while !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            raw.truncate(cut);
            let Ok(cand) = Name::new(&raw) else { continue };
            if self.name_free(remote_parent, local_parent, &cand) {
                return cand;
            }
        }
    }

    fn name_free(&self, rp: Option<NodeId>, lp: Option<LocalId>, name: &Name) -> bool {
        rp.is_none_or(|p| self.eff_occupant(p, name, None).is_none())
            && lp.is_none_or(|p| self.local_occupant(p, name, None).is_none())
    }

    fn device(&self) -> String {
        self.st
            .config
            .device
            .chars()
            .map(|c| if c == '/' || c == '\0' { '-' } else { c })
            .take(40)
            .collect()
    }

    /// Gerätename für temporäre Namen: ohne „~“, das dort Präfix und Heimatnamen trennt.
    fn temp_device(&self) -> String {
        self.device().replace('~', "-")
    }

    /// Lesbare Zusammenfassung des Zustands (für Fehlersuche im Simulator).
    pub fn debug_summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "cursor={:?} pending={:?}",
            self.st.cursor, self.st.pending
        );
        let _ = writeln!(out, "outbox={:?}", self.st.outbox);
        let _ = writeln!(out, "inflight={:?}", self.inflight_local);
        for (n, s) in self.st.synced.iter() {
            let _ = writeln!(
                out,
                "S {n:?}: {:?}/{:?} {:?} local={:?} | R={:?} | L={:?}",
                s.parent,
                s.name,
                s.content,
                s.local,
                self.st
                    .remote
                    .get(n)
                    .map(|r| (r.parent, r.name.clone(), r.content)),
                self.local
                    .get(s.local)
                    .map(|l| (l.parent, l.name.clone(), l.content)),
            );
        }
        for (n, r) in self.st.remote.iter() {
            if !self.st.synced.contains(n) {
                let _ = writeln!(
                    out,
                    "R-only {n:?}: {:?}/{:?} {:?}",
                    r.parent, r.name, r.content
                );
            }
        }
        for (l, e) in self.local.iter() {
            if self.st.synced.node_of(l).is_none() {
                let _ = writeln!(
                    out,
                    "L-only {l:?}: {:?}/{:?} {:?}",
                    e.parent, e.name, e.content
                );
            }
        }
        out
    }

    // ------------------------------------------------------------------------------------------
    // Prüfungen
    // ------------------------------------------------------------------------------------------

    /// Prüft innere Konsistenz. Im Test führt ein Fehler zum Abbruch.
    pub fn check_invariants(&self) -> Result<(), String> {
        self.st.synced.check()?;
        let root = self.root();
        for (n, _) in self.st.remote.iter() {
            if self.st.remote.depth(n, root).is_none() {
                return Err(format!("R: {n:?} nicht von der Wurzel erreichbar"));
            }
        }
        if let Some(lr) = self.local_root
            && !self.local_untrusted
        {
            for (l, _) in self.local.iter() {
                if self.local.depth(l, lr).is_none() {
                    return Err(format!("L: {l:?} nicht von der Wurzel erreichbar"));
                }
            }
        }
        for id in self.inflight_local.keys() {
            if self.st.outbox.contains_key(id) {
                return Err(format!("{id:?} ist zugleich lokal und entfernt in Arbeit"));
            }
        }
        Ok(())
    }
}

fn unorder(w: (u8, usize, u64)) -> Key {
    if w.0 == 0 {
        Key::Node(NodeId(w.2))
    } else {
        Key::Local(LocalId(w.2))
    }
}

fn remote_differs(s: &SyncedEntry, re: &RemoteEntry) -> bool {
    re.parent != s.parent || re.name != s.name || (s.kind == Kind::File && re.content != s.content)
}

fn mark_remote_busy(op: &RemoteOp, synced: &Synced, cx: &mut Ctx) {
    match op {
        RemoteOp::CreateDir { source, origin, .. }
        | RemoteOp::CreateFile { source, origin, .. } => {
            cx.busy_l.insert(*source);
            if let Origin::ConflictCopy {
                replaces: Some(r), ..
            } = origin
            {
                cx.busy_n.insert(*r);
            }
        }
        RemoteOp::Upload { node, source, .. } => {
            cx.busy_n.insert(*node);
            cx.busy_l.insert(*source);
        }
        RemoteOp::Move { node, .. }
        | RemoteOp::DeleteFile { node, .. }
        | RemoteOp::DeleteDir { node, .. } => {
            cx.busy_n.insert(*node);
            if let Some(s) = synced.get(*node) {
                cx.busy_l.insert(s.local);
            }
        }
    }
}

fn mark_local_busy(op: &LocalOp, synced: &Synced, cx: &mut Ctx) {
    match op {
        LocalOp::CreateDir { node, .. } | LocalOp::Download { node, .. } => {
            cx.busy_n.insert(*node);
        }
        LocalOp::Replace { local, node, .. }
        | LocalOp::DeleteFile { local, node, .. }
        | LocalOp::DeleteDir { local, node, .. } => {
            cx.busy_n.insert(*node);
            cx.busy_l.insert(*local);
        }
        LocalOp::Move { local, .. } => {
            cx.busy_l.insert(*local);
            if let Some(n) = synced.node_of(*local) {
                cx.busy_n.insert(n);
            }
        }
    }
}
