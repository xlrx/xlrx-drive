//! The sync engine: three-way reconciliation between the server (R), the synced state (S) and the
//! local disk (L).
//!
//! The rules are described in `docs/adr/0001-sync-engine.md`. Summary:
//!
//! - For each node, **location** (parent + name) and **content** are compared separately.
//! - Only one side changed → apply the change to the other side.
//! - Both sides changed identically → only update S.
//! - Content changed differently on both sides → conflict copy; nothing is overwritten.
//! - Location changed differently on both sides → the server wins (move locally).
//! - Deletion versus change → **the change wins** (on both sides).
//! - Directories are only deleted when they are empty; anything inside that must survive brings
//!   the directory back.
//!
//! Safety nets: every operation carries preconditions (fingerprint, revision, empty directory,
//! free name), server operations are idempotent (outbox with [`OpId`]), and local content is only
//! replaced or deleted when it provably matches the synced state.

use std::collections::{BTreeMap, BTreeSet};

use xlrx_proto::{Kind, Name, NodeId, Rev, Seq};

use crate::ops::{Expected, LocalOp, LocalResult, Op, Origin, RemoteOp, RemoteResult};
use crate::synced::Synced;
use crate::tree::Tree;
use crate::types::{
    Config, LocalEntry, LocalId, LocalObservation, OpId, RemoteChange, RemoteEntry, SyncedEntry,
};

/// Longest chain that is followed when searching for swap cycles.
const MAX_CHAIN: usize = 64;

// Prefixes of temporary yield names and of download temporaries (defined with the other naming
// rules in `xlrx_proto::name`, re-exported by this crate).
use xlrx_proto::name::{DOWNLOAD_TEMP_PREFIX, TEMP_PREFIX};

/// Home name of a temporary yield name (the part after the first "~"; the device name in the
/// prefix never contains "~"). If the home name was truncated when yielding, the truncated
/// name is restored.
fn temp_home(name: &Name) -> Option<Name> {
    let rest = name.as_str().strip_prefix(TEMP_PREFIX)?;
    let home = rest.split_once('~').map(|(_, h)| h).unwrap_or("");
    Some(Name::new(home).unwrap_or_else(|_| Name::new("Wiederhergestellt").expect("gültiger Name")))
}

/// Persisted state. After a crash, the engine is rebuilt from it
/// ([`Engine::from_state`]); the local tree is rebuilt by a scan.
/// (In the client it is stored in structured form in SQLite; in the simulator, as a copy.)
#[derive(Clone, Debug)]
pub struct State {
    pub config: Config,
    /// Journal position up to which R mirrors the server. `None` before the first fetch.
    pub cursor: Option<Seq>,
    pub remote: Tree<NodeId, RemoteEntry>,
    pub synced: Synced,
    /// Server operations that were or will be sent. Removed only once their result is processed.
    pub outbox: BTreeMap<OpId, RemoteOp>,
    /// Nodes whose S state is newer than R due to our own operations. Until R has reached this
    /// sequence, S counts as their server state, and they are not re-planned.
    pub pending: BTreeMap<NodeId, Seq>,
    /// Local yield renames (swap cycles): object → (directory, temporary name).
    /// While the object is there, its synced location is used for reconciliation, so that yielding
    /// never counts as a user change. Persisted before execution.
    pub local_temps: BTreeMap<LocalId, (LocalId, Name)>,
    pub next_op: u64,
    pub name_counter: u64,
    /// How often the safety net had to break a wait cycle (diagnostics).
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
    /// Places where a rule is waiting, each with a safe action that resolves the wait.
    stuck: Vec<Breaker>,
}

/// Safe actions used to break an unforeseen wait cycle.
/// None of them can lose data: they only rename, or keep something instead of deleting it.
#[derive(Clone, Copy, Debug)]
enum Breaker {
    /// Temporarily move a local object to a free name.
    TempLocal(LocalId),
    /// Temporarily move a server object to a free name.
    TempRemote(NodeId),
    /// Unlink: the directory is kept and recreated instead of being deleted.
    Unlink(NodeId),
}

/// Key for incremental planning.
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
    /// Consecutive planning runs without progress while the state is fresh.
    stalled: u32,
    /// Incremental planning: objects whose situation has changed since the last planning run.
    dirty: BTreeSet<Key>,
    /// Objects that had to wait in the last planning run; they are re-checked every time.
    waiting: BTreeSet<Key>,
    /// Linked nodes whose local object is missing (candidates for rebinding).
    orphans: BTreeSet<NodeId>,
    /// The next planning run checks everything (after startup, a full scan or a full server state).
    full: bool,
    /// The local tree has become inconsistent (partial scan with unknown ancestors, late result
    /// based on an outdated state). Nothing is planned until the next full scan:
    /// an incomplete picture must never be read as "deleted locally".
    local_untrusted: bool,
}

/// Number of planning runs without progress (with fresh server and local state) that must
/// pass before the safety net steps in.
const STALL_LIMIT: u32 = 3;

impl Engine {
    pub fn new(config: Config) -> Self {
        Self::from_state(State::new(config))
    }

    /// Restart from persisted state. Local operations that were still running are considered lost;
    /// a scan determines which of them actually happened. Server operations from the outbox are
    /// resent with the same ID.
    pub fn from_state(st: State) -> Self {
        let fold = st.config.local_case_insensitive;
        Self {
            st,
            local: Tree::new_local(fold),
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

    /// Does the engine already have a local state (a full scan since startup)?
    pub fn has_local_tree(&self) -> bool {
        self.local_root.is_some()
    }

    pub fn wants_scan(&self) -> bool {
        self.need_scan
    }

    /// Only a full scan ([`Self::on_local_snapshot`]) can make progress.
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

    /// No outstanding operations.
    pub fn is_idle(&self) -> bool {
        self.st.outbox.is_empty() && self.inflight_local.is_empty()
    }

    fn root(&self) -> NodeId {
        self.st.config.remote_root
    }

    // ------------------------------------------------------------------------------------------
    // Inputs
    // ------------------------------------------------------------------------------------------

    /// Changes from the server journal since the last cursor.
    pub fn on_remote_changes(&mut self, changes: Vec<RemoteChange>, cursor: Seq) {
        let root = self.root();
        let mut inserted = Vec::new();
        let mut deleted = Vec::new();
        // First apply all new states, then process deletions: a node that was moved out of a
        // deleted directory in the same batch must not be deleted along with it.
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
            // Deleted or moved out of view: remove it together with its (remaining) subtree.
            for d in self.st.remote.descendants(n) {
                self.r_remove(d);
            }
            self.r_remove(n);
        }
        // A node whose parent node is missing after the whole batch is not visible.
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

    /// Full server state (first fetch, or after a truncated journal).
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

    /// Full scan of the local sync directory.
    pub fn on_local_snapshot(&mut self, root: LocalId, observations: Vec<LocalObservation>) {
        let mut t = Tree::new_local(self.st.config.local_case_insensitive);
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

    /// Individual local changes (e.g. from FSEvents and a rescan of the affected directories).
    /// A full scan must have happened beforehand.
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
        // Consistency: every reported object must be reachable from the root. Otherwise a report
        // is missing (unknown ancestor, cycle with a move not yet reported) – in that case remove
        // nothing (that would look like a deletion), but do a full rescan instead.
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

    /// Keeps the local ↔ server link stable when the local identity changes.
    ///
    /// 1. If a linked ID now has a different kind (inode reuse), the link is removed.
    /// 2. If the linked ID has disappeared and a new, unlinked object of the same kind is at the
    ///    same location, it is considered the same object. This is the common "atomic save" of many
    ///    programs (write a new file, then rename it over the original) and is treated as a content
    ///    change, not as deletion + creation.
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
        // Only nodes without a local object remain candidates.
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
    // Results
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

    /// Is the local parent directory known? Results can arrive late, after a scan has already
    /// reported the directory as deleted. In that case nothing is recorded; the next scan shows
    /// what is really on disk (L would rather stay outdated than become incomplete).
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
                // If a scan has already seen the new object, its entry is newer than the result.
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
                    self.l_remove(local); // a scan has already seen the result
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
                // Only update L if it still shows the state before the move. If it shows something
                // else, a scan has already seen something newer.
                let before = self
                    .local
                    .get(local)
                    .is_some_and(|e| e.parent == from_parent && e.name == from_name);
                if before {
                    let known = Some(parent) == self.local_root || self.local.contains(parent);
                    if !known || parent == local || self.local.is_within(parent, local) {
                        // Cannot apply to the outdated state (another result is still pending).
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
                    // The directory was empty when it was deleted, but L still has children: L is
                    // outdated (the children have since been moved). Only the scan tells where they
                    // are; under no circumstances may they be considered deleted.
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
            // Unknown whether executed: resend later with the same ID (the server deduplicates).
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
                // Server and local source had the same content afterwards. If the local file
                // has since been replaced by an "atomic save" (new local ID), this still holds;
                // only the fingerprint then does not belong to the current file.
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
                // The server had newer content in the meantime and stored ours as a conflict copy.
                // The local file now belongs to the copy; the original is downloaded again.
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

    /// Links only if neither the node nor the local ID is already linked.
    ///
    /// If the local ID no longer exists (e.g. an "atomic save" while the result was in transit),
    /// rebinding is attempted immediately. Otherwise the node would look deleted locally.
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
    // Planning
    // ------------------------------------------------------------------------------------------

    /// Plans the next operations. May be called without a new result; it then returns nothing new.
    /// The state must be persisted afterwards, before the operations are executed.
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

        // Work list: everything (after startup/full scan) or only what has changed or is waiting.
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
                // Changed several times in this pass: continue next time.
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

        // Safety net: nothing planned, nothing changed, nothing in flight – yet rules are waiting
        // on each other. Then a safe action breaks the cycle instead of hanging forever.
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
                    // (s_remove marks the neighbors for the next planning run)
                }
            }
        }
        cx.ops
    }

    /// Evaluates one key of the work list.
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

    /// Is there nothing left to do for this key (or is an operation currently running)?
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

    /// Work list order: nodes before local objects, shallow before deep.
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
    // Tree mutations – mark affected neighbors for incremental planning
    // ------------------------------------------------------------------------------------------

    /// Re-check everything involved in an operation (after success as well as after failure).
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

    /// A directory whose existence or link has changed: re-check all of its children as well.
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

    /// Tests/simulator only: does a full planning run find work that incremental planning has
    /// missed? To be called right after a planning run that produced nothing.
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

    /// Deleted on the server, present locally.
    fn plan_remote_gone(&mut self, n: NodeId, s: &SyncedEntry, le: &LocalEntry, cx: &mut Ctx) {
        if self.local_differs(s, le) {
            // The local change wins over the deletion: unlink; the object is uploaded again.
            // (Check this first: a local change must never be attributed to a new server node.)
            self.s_remove(n);
            return;
        }
        // "Atomic save" on the server: a new node of the same kind is now at the same location.
        // Then the (unchanged) local object is its predecessor: rebind instead of deleting and
        // re-downloading. The new content then arrives as an ordinary server change.
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
                // The synced children now belong to the new directory. Otherwise they would appear
                // to have been moved in locally, and children deleted on the server would return.
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
                // The children decide: wait for whatever is being deleted or moving away. Anything
                // that must stay (new, moved in locally) brings the directory back to the server.
                let mut deleting = false; // children being deleted (or unlinked)
                let mut leaving = false; // children the server has moved elsewhere
                let mut keep = false; // children that must stay here
                for k in self.local.children(s.local) {
                    match self.st.synced.node_of(k) {
                        None => keep = true,
                        Some(m) => {
                            let s_loc = self.st.synced.get(m).map(|x| (x.parent, x.name.clone()));
                            match self.eff_remote_loc(m) {
                                None => deleting = true,
                                Some(r) if Some(&r) != s_loc.as_ref() => leaving = true,
                                Some(_) => keep = true, // moved in locally
                            }
                        }
                    }
                }
                // Wait for the deletions first, so that unchanged children do not come back too.
                // Children moving away do not block: their target may depend on this directory
                // disappearing or being recreated.
                if deleting || (leaving && !keep) {
                    cx.stuck.push(Breaker::Unlink(n));
                    return;
                }
                if keep {
                    // Something inside it must stay: recreate the directory on the server.
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

    /// Deleted locally, present on the server.
    fn plan_local_gone(&mut self, n: NodeId, s: &SyncedEntry, re: &RemoteEntry, cx: &mut Ctx) {
        if remote_differs(s, re) {
            // The change on the server wins over the local deletion: download again.
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
                // Mirror image: whatever inside it is new or changed on the server brings the
                // directory back locally. Unchanged children get deleted or move away: wait.
                let mut deleting = false; // deleted locally, unchanged on the server
                let mut leaving = false; // moved elsewhere locally
                let mut keep = false; // must stay here (new, or changed on the server)
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
                                // changed, or the local move cannot be uploaded (cycle)
                                // and the child returns here
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
                    // Something inside it on the server must stay: restore the directory locally.
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

    /// Present on both sides.
    fn plan_both(
        &mut self,
        n: NodeId,
        mut s: SyncedEntry,
        re: &RemoteEntry,
        le: &LocalEntry,
        cx: &mut Ctx,
    ) {
        if s.kind == Kind::File {
            // Same content but a new fingerprint (e.g. `touch`) or a new revision (same content
            // uploaded again): only refresh S, so that preconditions stay up to date.
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
            return; // content only once the location is settled
        }
        // The object is still on one of our own yield names although it no longer needs to go
        // anywhere (e.g. because the server reverted the move): move it back to its real location.
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
                // Real conflict: save our content as a copy, then download the original.
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

    /// Move the local object to the server location (apply a server change, or the server wins).
    fn local_move_to(
        &mut self,
        n: NodeId,
        l: LocalId,
        le: &LocalEntry,
        target: &(NodeId, Name),
        cx: &mut Ctx,
    ) {
        let Some(lp) = self.local_of_node(target.0) else {
            return; // target directory does not exist locally (yet)
        };
        if lp == l || self.local.is_within(lp, l) {
            return; // target lies locally inside the object; resolved by the other rules
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
                    return; // unlinked object: its own rule yields
                };
                if cx.busy_l.contains(&o) || cx.busy_n.contains(&m) {
                    return;
                }
                // Case variant on the server (e.g. via SMB "b" → "a" next to "A"): o stays where it
                // is, but locally there is only one name for both. As with new nodes, the newcomer
                // yields to a conflict name on the server.
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
                // If o's target lies inside o locally, o cannot leave and must make room first.
                let o_target_nested = self
                    .eff_remote_loc(m)
                    .and_then(|t| self.local_of_node(t.0))
                    .is_some_and(|tl| self.local.is_within(tl, o));
                if self.local_cycle(o, l)
                    || self.deleted_remotely_with_content(o)
                    || o_target_nested
                {
                    // Swap cycle, or the occupant is a deleted directory whose content must first
                    // move away (possibly to here): the occupant yields.
                    self.yield_local_temp(o, cx);
                } else {
                    cx.stuck.push(Breaker::TempLocal(o));
                }
            }
        }
    }

    /// Push a local location change to the server.
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
            return; // local parent directory not on the server yet
        };
        if !self.eff_exists(p) {
            return; // parent directory deleted on the server; its rule restores it
        }
        if p == n || self.eff_is_within(p, n) {
            // Would create a cycle on the server: the server wins.
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
                    // An object newly created on the server holds the name: yield locally.
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
                    // Cycle, or the occupant is a locally deleted directory whose content
                    // must first move away on the server: the occupant yields on the server.
                    self.yield_remote_temp(m, cx);
                } else {
                    cx.stuck.push(Breaker::TempRemote(m));
                }
                // otherwise: m soon vacates the name on the server (moved/deleted locally) → wait
            }
        }
    }

    /// Present on the server, not linked (yet).
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
                    return; // unlinked local object: gets linked or yields
                };
                if cx.busy_l.contains(&o)
                    || cx.busy_n.contains(&m)
                    || self.st.pending.contains_key(&m)
                {
                    return;
                }
                let Some(target) = self.eff_remote_loc(m) else {
                    // m was deleted on the server; the local deletion follows. If content must
                    // first move away for that (possibly into n), the directory yields.
                    if self.deleted_remotely_with_content(o) {
                        self.yield_local_temp(o, cx);
                    }
                    return;
                };
                let o_loc = self.loc_of(o);
                if Some(&target) != o_loc.as_ref() {
                    // m is still moving away locally (the server moved it). If the target directory
                    // for that is missing (e.g. because it is to be created right here), m yields
                    // temporarily; otherwise both wait for each other. If instead the user moved m
                    // here locally, m yields via its own rule.
                    let remote_moved = self
                        .st
                        .synced
                        .get(m)
                        .is_some_and(|sm| (sm.parent, sm.name.clone()) != target);
                    let blocked = match self.local_of_node(target.0) {
                        None => true,                            // target directory still missing
                        Some(tl) => self.local.is_within(tl, o), // target lies inside o itself
                    };
                    if remote_moved && blocked {
                        self.yield_local_temp(o, cx);
                    } else if remote_moved {
                        cx.stuck.push(Breaker::TempLocal(o));
                    }
                    return;
                }
                // m stays: the names only collide locally (letter case). Rename on the server.
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

    /// Present locally, not linked (yet).
    fn plan_local_new(&mut self, l: LocalId, cx: &mut Ctx) {
        let Some(le) = self.local.get(l).cloned() else {
            return;
        };
        if le.name.as_str().starts_with(DOWNLOAD_TEMP_PREFIX) {
            return; // the executor's half-finished download; never upload
        }
        if temp_home(&le.name).is_some() {
            // A (no longer linked) object on a yield name: rename it back first, so that the
            // temporary name never reaches the server.
            self.st.local_temps.remove(&l);
            self.cleanup_temp_name(l, &le, cx);
            return;
        }
        let Some(p) = self.local_parent_node(le.parent) else {
            return; // parent directory first
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
                        // m is in a local directory not yet uploaded and blocks the name that this
                        // directory (or an ancestor) needs, or m is a locally deleted directory
                        // whose content must first move away on the server: m yields briefly.
                        self.yield_remote_temp(m, cx);
                    }
                    // otherwise: m will soon vacate the name → wait
                } else if let Some(rm) = self.st.remote.get(m).cloned() {
                    let same_local_key = self.local.key(&rm.name) == self.local.key(&le.name);
                    let same = rm.kind == le.kind
                        && same_local_key
                        && (rm.kind == Kind::Dir || rm.content == le.content);
                    if same {
                        // Both sides created the same thing (e.g. initial setup): just link them.
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

    /// A fully synchronized object still carries a temporary yield name
    /// (e.g. after a crash or a concurrent deletion): rename it back to its home name.
    /// The rename is local and is then uploaded like any user rename.
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

    /// The local object yields to a conflict name (purely local; the new name is uploaded later).
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

    /// Breaks a local swap cycle: temporarily move `o` to a free name.
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

    /// Breaks a swap cycle on the server: temporarily move `m` to a free name.
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

    /// A local directory whose node has been deleted on the server, but which still has content
    /// that must move away first. If it is in the way, it can be part of a wait cycle.
    fn deleted_remotely_with_content(&self, o: LocalId) -> bool {
        let Some(m) = self.st.synced.node_of(o) else {
            return false;
        };
        self.eff_remote_loc(m).is_none()
            && self.local.get(o).is_some_and(|e| e.kind == Kind::Dir)
            && self.local.has_children(o)
    }

    /// Mirror image: a server directory deleted locally that still has content on the server.
    fn deleted_locally_with_content(&self, m: NodeId) -> bool {
        let Some(sm) = self.st.synced.get(m) else {
            return false;
        };
        sm.kind == Kind::Dir && !self.local.contains(sm.local) && !self.eff_children(m).is_empty()
    }

    /// Would uploading the local location change of `m` create a cycle on the server?
    /// Then the server wins, and `m` stays at (or returns to) its server location.
    fn push_would_cycle(&self, m: NodeId) -> bool {
        let Some(sm) = self.st.synced.get(m) else {
            return false;
        };
        let Some((p, _)) = self.loc_of(sm.local) else {
            return false;
        };
        p == m || self.eff_is_within(p, m)
    }

    /// Follows the chain "o wants to go to a place already taken by someone who also wants to
    /// leave …". `true` if it arrives at `l` (local swap cycle).
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
                        return false; // cur is already at its target
                    }
                    self.local_occupant(tl, &target.1, Some(cur))
                }
                None => {
                    // The target directory is missing locally and must be created first (possibly
                    // with missing ancestors): who occupies the place of the topmost missing one?
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

    /// Like [`Self::local_cycle`], but for desired moves on the server.
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
    // Helpers
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

    /// Location of a local object in node space. `None` if its local parent is not linked yet.
    fn local_loc(&self, le: &LocalEntry) -> Option<(NodeId, Name)> {
        self.local_parent_node(le.parent)
            .map(|p| (p, le.name.clone()))
    }

    /// Location of a local object for reconciliation. If it is on one of our own temporary
    /// yield names, its synced location counts (yielding is not a user change).
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

    /// Local directory for a node, if it exists locally.
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

    /// Server location of a node, taking into account our own operations not yet visible in R.
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

    /// Is `id` (effectively) equal to or below `ancestor` on the server?
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
        // Cycle: answer "yes" to be safe (prevents a move)
        true
    }

    /// Free conflict name `Name (Konflikt <device> N).ext` – free both on the server and locally.
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

    /// Temporary yield name. It embeds the home name (".xlrx-tmp-Device-7~Report.txt"),
    /// so that any device can rename back an object that got stuck there.
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

    /// Device name for temporary names: without "~", which separates prefix and home name there.
    fn temp_device(&self) -> String {
        self.device().replace('~', "-")
    }

    /// Human-readable summary of the state (for debugging in the simulator).
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
    // Checks
    // ------------------------------------------------------------------------------------------

    /// Checks internal consistency. In tests, an error aborts the run.
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
