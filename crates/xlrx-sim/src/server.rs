//! Simulated server: node tree, journal with sequence numbers, idempotency per (client, OpId)
//! and the same conflict rules as the real server.

use std::collections::BTreeMap;

use xlrx_proto::{FileContent, Kind, Name, NodeId, Rev, Seq};
use xlrx_sync::{OpId, Reject, RemoteChange, RemoteEntry, RemoteOp, RemoteResult};

#[derive(Clone, Debug)]
pub struct SNode {
    pub parent: NodeId,
    pub name: Name,
    pub kind: Kind,
    pub content: Option<FileContent>,
    pub rev: Rev,
    pub alive: bool,
}

#[derive(Clone, Debug)]
pub struct SimServer {
    pub nodes: BTreeMap<NodeId, SNode>,
    pub root: NodeId,
    seq: u64,
    next_node: u64,
    /// Compact journal: node → sequence number of its last change.
    journal: BTreeMap<NodeId, u64>,
    dedup: BTreeMap<(usize, OpId), RemoteResult>,
    /// Exact name check instead of a case-insensitive one (as with SMB/shell access to the NAS
    /// file system). This produces variants like "A" and "a" in the same folder.
    pub exact_names: bool,
}

impl SimServer {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            root: NodeId(1),
            seq: 0,
            next_node: 2,
            journal: BTreeMap::new(),
            dedup: BTreeMap::new(),
            exact_names: false,
        }
    }

    pub fn seq(&self) -> Seq {
        Seq(self.seq)
    }

    fn bump(&mut self, nodes: &[NodeId]) -> Seq {
        self.seq += 1;
        for n in nodes {
            self.journal.insert(*n, self.seq);
        }
        Seq(self.seq)
    }

    pub fn alive(&self, n: NodeId) -> bool {
        n == self.root || self.nodes.get(&n).is_some_and(|x| x.alive)
    }

    pub fn is_dir(&self, n: NodeId) -> bool {
        n == self.root
            || self
                .nodes
                .get(&n)
                .is_some_and(|x| x.alive && x.kind == Kind::Dir)
    }

    pub fn children(&self, p: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, x)| x.alive && x.parent == p)
            .map(|(k, _)| *k)
            .collect()
    }

    /// The server enforces names that are unique regardless of case
    /// (except with [`Self::exact_names`]).
    pub fn occupant(&self, p: NodeId, name: &Name, except: Option<NodeId>) -> Option<NodeId> {
        let k = name.fold_key();
        let exact = self.exact_names;
        self.nodes
            .iter()
            .find(|(id, x)| {
                x.alive
                    && x.parent == p
                    && Some(**id) != except
                    && if exact {
                        x.name == *name
                    } else {
                        x.name.fold_key() == k
                    }
            })
            .map(|(id, _)| *id)
    }

    /// Is the node alive and located exactly at `parent`/`name`?
    fn located(&self, n: NodeId, parent: NodeId, name: &Name) -> bool {
        self.nodes
            .get(&n)
            .is_some_and(|x| x.alive && x.parent == parent && x.name == *name)
    }

    fn within(&self, n: NodeId, anc: NodeId) -> bool {
        let mut cur = n;
        loop {
            if cur == anc {
                return true;
            }
            if cur == self.root {
                return false;
            }
            match self.nodes.get(&cur) {
                Some(x) => cur = x.parent,
                None => return false,
            }
        }
    }

    /// Changes since `cursor` (current state of each changed node) and the new cursor.
    pub fn changes_since(&self, cursor: Seq) -> (Vec<RemoteChange>, Seq) {
        let changes = self
            .journal
            .iter()
            .filter(|(_, s)| **s > cursor.0)
            .map(|(n, _)| RemoteChange {
                node: *n,
                state: self.nodes.get(n).filter(|x| x.alive).map(|x| RemoteEntry {
                    parent: x.parent,
                    name: x.name.clone(),
                    kind: x.kind,
                    content: x.content,
                    rev: x.rev,
                }),
            })
            .collect();
        (changes, Seq(self.seq))
    }

    pub fn content_of(&self, n: NodeId) -> Option<FileContent> {
        self.nodes
            .get(&n)
            .filter(|x| x.alive)
            .and_then(|x| x.content)
    }

    fn free_name(&self, p: NodeId, base: &Name, label: &str) -> Name {
        let mut k = 1;
        loop {
            let cand = base.with_suffix(&format!("{label} {k}"));
            if self.occupant(p, &cand, None).is_none() {
                return cand;
            }
            k += 1;
        }
    }

    // --- Mutations (also used by "server users" such as the web UI or SMB) ---

    pub fn create(
        &mut self,
        p: NodeId,
        name: &Name,
        kind: Kind,
        content: Option<FileContent>,
    ) -> Result<(NodeId, Rev, Seq), Reject> {
        if !self.is_dir(p) {
            return Err(Reject::ParentGone);
        }
        if self.occupant(p, name, None).is_some() {
            return Err(Reject::NameTaken);
        }
        let id = NodeId(self.next_node);
        self.next_node += 1;
        let rev = Rev(self.seq + 1);
        self.nodes.insert(
            id,
            SNode {
                parent: p,
                name: name.clone(),
                kind,
                content,
                rev,
                alive: true,
            },
        );
        let seq = self.bump(&[id]);
        Ok((id, rev, seq))
    }

    pub fn write(&mut self, n: NodeId, content: FileContent) -> Result<(Rev, Seq), Reject> {
        if !self.alive(n) || self.is_dir(n) {
            return Err(Reject::NodeGone);
        }
        let rev = Rev(self.seq + 1);
        if let Some(x) = self.nodes.get_mut(&n) {
            x.content = Some(content);
            x.rev = rev;
        }
        let seq = self.bump(&[n]);
        Ok((rev, seq))
    }

    pub fn mv(&mut self, n: NodeId, p: NodeId, name: &Name) -> Result<Seq, Reject> {
        if n == self.root || !self.alive(n) {
            return Err(Reject::NodeGone);
        }
        if !self.is_dir(p) {
            return Err(Reject::ParentGone);
        }
        if self.within(p, n) {
            return Err(Reject::WouldCycle);
        }
        if self.occupant(p, name, Some(n)).is_some() {
            return Err(Reject::NameTaken);
        }
        if let Some(x) = self.nodes.get_mut(&n) {
            x.parent = p;
            x.name = name.clone();
        }
        Ok(self.bump(&[n]))
    }

    /// Deletes recursively (to the trash). Returns the contents of all deleted files.
    pub fn delete_tree(&mut self, n: NodeId) -> Vec<FileContent> {
        let mut stack = vec![n];
        let mut all = Vec::new();
        let mut contents = Vec::new();
        while let Some(cur) = stack.pop() {
            stack.extend(self.children(cur));
            all.push(cur);
        }
        for id in &all {
            if let Some(x) = self.nodes.get_mut(id) {
                x.alive = false;
                if let Some(c) = x.content {
                    contents.push(c);
                }
            }
        }
        self.bump(&all);
        contents
    }

    // --- API for clients (idempotent) ---

    /// Result of an already executed operation (idempotency). The real client queries this
    /// before uploading content: a repeated operation no longer needs the source file.
    pub fn known_result(&self, client: usize, op_id: OpId) -> Option<RemoteResult> {
        self.dedup.get(&(client, op_id)).cloned()
    }

    pub fn apply(
        &mut self,
        client: usize,
        op_id: OpId,
        op: &RemoteOp,
        device: &str,
    ) -> RemoteResult {
        if let Some(r) = self.dedup.get(&(client, op_id)) {
            return r.clone();
        }
        let r = self.execute(op, device);
        self.dedup.insert((client, op_id), r.clone());
        r
    }

    fn execute(&mut self, op: &RemoteOp, device: &str) -> RemoteResult {
        let res = match op {
            RemoteOp::CreateDir { parent, name, .. } => self
                .create(*parent, name, Kind::Dir, None)
                .map(|(node, rev, seq)| RemoteResult::Created { node, rev, seq }),
            RemoteOp::CreateFile {
                parent,
                name,
                content,
                ..
            } => self
                .create(*parent, name, Kind::File, Some(*content))
                .map(|(node, rev, seq)| RemoteResult::Created { node, rev, seq }),
            RemoteOp::Upload {
                node,
                base_rev,
                content,
                ..
            } => match self.nodes.get(node).cloned() {
                Some(x) if x.alive && x.kind == Kind::File => {
                    if x.rev == *base_rev {
                        self.write(*node, *content)
                            .map(|(rev, seq)| RemoteResult::Updated { rev, seq })
                    } else {
                        // Conflict: overwrite nothing, store the uploaded content next to it.
                        let name = self.free_name(x.parent, &x.name, &format!("Konflikt {device}"));
                        self.create(x.parent, &name, Kind::File, Some(*content))
                            .map(|(node, rev, seq)| RemoteResult::Conflict { node, rev, seq })
                    }
                }
                _ => Err(Reject::NodeGone),
            },
            RemoteOp::Move {
                node,
                from_parent,
                from_name,
                parent,
                name,
            } => {
                if self.alive(*node) && !self.located(*node, *from_parent, from_name) {
                    Err(Reject::Moved)
                } else {
                    self.mv(*node, *parent, name)
                        .map(|seq| RemoteResult::Moved { seq })
                }
            }
            RemoteOp::DeleteFile {
                node,
                base_rev,
                parent,
                name,
            } => match self.nodes.get(node) {
                Some(x) if x.alive && x.kind == Kind::File => {
                    if x.rev != *base_rev {
                        Err(Reject::RevMismatch)
                    } else if !self.located(*node, *parent, name) {
                        Err(Reject::Moved)
                    } else {
                        self.delete_tree(*node);
                        Ok(RemoteResult::Deleted { seq: self.seq() })
                    }
                }
                _ => Err(Reject::NodeGone),
            },
            RemoteOp::DeleteDir { node, parent, name } => {
                if !self.is_dir(*node) || *node == self.root {
                    Err(Reject::NodeGone)
                } else if !self.located(*node, *parent, name) {
                    Err(Reject::Moved)
                } else if !self.children(*node).is_empty() {
                    Err(Reject::NotEmpty)
                } else {
                    self.delete_tree(*node);
                    Ok(RemoteResult::Deleted { seq: self.seq() })
                }
            }
        };
        res.unwrap_or_else(RemoteResult::Rejected)
    }

    /// Path → (kind, content) of all live nodes.
    pub fn listing(&self) -> BTreeMap<String, (Kind, Option<FileContent>)> {
        let mut out = BTreeMap::new();
        for (id, x) in &self.nodes {
            if x.alive
                && let Some(p) = self.path(*id)
            {
                out.insert(p, (x.kind, x.content));
            }
        }
        out
    }

    pub fn path(&self, n: NodeId) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = n;
        while cur != self.root {
            let x = self.nodes.get(&cur)?;
            parts.push(x.name.as_str().to_owned());
            cur = x.parent;
        }
        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }
}

impl Default for SimServer {
    fn default() -> Self {
        Self::new()
    }
}
