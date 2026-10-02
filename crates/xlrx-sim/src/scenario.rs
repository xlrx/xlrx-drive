//! Hand-written scenarios using paths instead of randomness, for readable behavior tests.
//!
//! ```ignore
//! let mut w = World::new(2, false);
//! w.client_write(0, "/Bericht.txt", 1);
//! w.sync();
//! assert_eq!(w.server_file("/Bericht.txt"), Some(1));
//! ```

use std::collections::BTreeMap;

use xlrx_proto::{ContentHash, FileContent, Kind, Name, NodeId, Seq};
use xlrx_sync::{Config, Engine, LocalId, LocalResult, Op, OpId, RemoteOp, RemoteResult, State};

use crate::driver;
use crate::fs::SimFs;
use crate::server::SimServer;

/// Content from a number (tag); `tag_of` is the inverse.
pub fn content(tag: u64) -> FileContent {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&tag.to_le_bytes());
    FileContent {
        hash: ContentHash(h),
        size: tag % 997 + 1,
    }
}

pub fn tag_of(c: &FileContent) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&c.hash.0[..8]);
    u64::from_le_bytes(b)
}

/// Counts transfers to check efficiency (e.g. initial setup without downloads).
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfers {
    pub downloads: usize,
    pub uploads: usize,
}

pub struct WorldClient {
    pub fs: SimFs,
    pub engine: Engine,
    persisted: State,
    device: String,
}

pub struct World {
    pub server: SimServer,
    pub clients: Vec<WorldClient>,
    clock: i64,
    pub transfers: Transfers,
}

/// Contents of a tree: path → `None` (folder) or `Some(tag)` (file).
pub type Listing = BTreeMap<String, Option<u64>>;

/// Result of an operation that was executed but not yet reported to the engine.
#[derive(Debug)]
pub enum Outcome {
    Local(OpId, LocalResult),
    Remote(OpId, RemoteResult),
}

fn split(path: &str) -> Vec<Name> {
    path.split('/')
        .filter(|p| !p.is_empty())
        .map(|p| Name::new(p).expect("gültiger Name im Testpfad"))
        .collect()
}

impl World {
    pub fn new(clients: usize, case_insensitive: bool) -> Self {
        let server = SimServer::new();
        let clients = (0..clients)
            .map(|i| {
                let device = format!("Gerät{i}");
                let engine = Engine::new(Config {
                    remote_root: server.root,
                    device: device.clone(),
                    local_case_insensitive: case_insensitive,
                    max_unconfirmed_deletes: usize::MAX,
                });
                let persisted = engine.state().clone();
                WorldClient {
                    fs: SimFs::new(case_insensitive, 1_000_000 * (i as u64 + 1)),
                    engine,
                    persisted,
                    device,
                }
            })
            .collect();
        Self {
            server,
            clients,
            clock: 1,
            transfers: Transfers::default(),
        }
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    /// Changes the device name (only meaningful before the first sync).
    pub fn set_device(&mut self, c: usize, device: &str) {
        let cl = &mut self.clients[c];
        let config = Config {
            device: device.to_owned(),
            ..cl.engine.state().config.clone()
        };
        cl.engine = Engine::new(config);
        cl.persisted = cl.engine.state().clone();
        cl.device = device.to_owned();
    }

    // ------------------------------------------------------------ Client file system

    fn client_dir(&mut self, c: usize, parts: &[Name], create: bool) -> Option<u64> {
        let clock = self.tick();
        let fs = &mut self.clients[c].fs;
        let mut cur = fs.root;
        for p in parts {
            cur = match fs.lookup(cur, p) {
                Some(x) => x,
                None if create => fs.create(cur, p, Kind::Dir, None, clock).ok()?,
                None => return None,
            };
        }
        Some(cur)
    }

    fn client_lookup(&mut self, c: usize, path: &str) -> Option<u64> {
        let parts = split(path);
        let (last, dir) = parts.split_last()?;
        let d = self.client_dir(c, dir, false)?;
        self.clients[c].fs.lookup(d, last)
    }

    /// Writes a file (creates it along with its folders, or changes its content in place).
    pub fn client_write(&mut self, c: usize, path: &str, tag: u64) {
        let parts = split(path);
        let (last, dir) = parts.split_last().expect("Pfad");
        let d = self.client_dir(c, dir, true).expect("Ordner");
        let clock = self.tick();
        let fs = &mut self.clients[c].fs;
        match fs.lookup(d, last) {
            Some(f) => fs.write(f, content(tag), clock).expect("schreiben"),
            None => {
                fs.create(d, last, Kind::File, Some(content(tag)), clock)
                    .expect("anlegen");
            }
        }
    }

    /// "Atomic save": write a new file and rename it over the original (new inode).
    pub fn client_atomic_save(&mut self, c: usize, path: &str, tag: u64) {
        let f = self.client_lookup(c, path).expect("Datei existiert");
        let clock = self.tick();
        let fs = &mut self.clients[c].fs;
        let orig = fs.inodes[&f].clone();
        let tmp = Name::new(".~tmp").expect("Name");
        let t = fs
            .create(orig.parent, &tmp, Kind::File, Some(content(tag)), clock)
            .expect("tmp");
        fs.rename(t, orig.parent, &orig.name, true, clock)
            .expect("ersetzen");
    }

    pub fn client_mkdir(&mut self, c: usize, path: &str) {
        self.client_dir(c, &split(path), true).expect("Ordner");
    }

    pub fn client_mv(&mut self, c: usize, from: &str, to: &str) {
        let x = self.client_lookup(c, from).expect("Quelle existiert");
        let parts = split(to);
        let (last, dir) = parts.split_last().expect("Ziel");
        let d = self.client_dir(c, dir, true).expect("Zielordner");
        let clock = self.tick();
        self.clients[c]
            .fs
            .rename(x, d, last, false, clock)
            .expect("verschieben");
    }

    pub fn client_rm(&mut self, c: usize, path: &str) {
        let x = self.client_lookup(c, path).expect("existiert");
        self.clients[c].fs.rm_rf(x);
    }

    // ------------------------------------------------------------ Server (e.g. web UI)

    fn server_dir(&mut self, parts: &[Name], create: bool) -> Option<NodeId> {
        let mut cur = self.server.root;
        for p in parts {
            cur = match self.server.occupant(cur, p, None) {
                Some(x) => x,
                None if create => self.server.create(cur, p, Kind::Dir, None).ok()?.0,
                None => return None,
            };
        }
        Some(cur)
    }

    pub fn server_node(&mut self, path: &str) -> Option<NodeId> {
        let parts = split(path);
        let (last, dir) = parts.split_last()?;
        let d = self.server_dir(dir, false)?;
        self.server.occupant(d, last, None)
    }

    pub fn server_write(&mut self, path: &str, tag: u64) {
        let parts = split(path);
        let (last, dir) = parts.split_last().expect("Pfad");
        let d = self.server_dir(dir, true).expect("Ordner");
        match self.server.occupant(d, last, None) {
            Some(n) => {
                self.server.write(n, content(tag)).expect("schreiben");
            }
            None => {
                self.server
                    .create(d, last, Kind::File, Some(content(tag)))
                    .expect("anlegen");
            }
        }
    }

    pub fn server_mkdir(&mut self, path: &str) {
        self.server_dir(&split(path), true).expect("Ordner");
    }

    pub fn server_mv(&mut self, from: &str, to: &str) {
        let n = self.server_node(from).expect("Quelle");
        let parts = split(to);
        let (last, dir) = parts.split_last().expect("Ziel");
        let d = self.server_dir(dir, true).expect("Zielordner");
        self.server.mv(n, d, last).expect("verschieben");
    }

    pub fn server_rm(&mut self, path: &str) {
        let n = self.server_node(path).expect("existiert");
        self.server.delete_tree(n);
    }

    // ------------------------------------------------------------ Sync

    /// Fetch and run a full scan, without planning.
    pub fn scan_client(&mut self, c: usize) {
        let cursor = self.clients[c].engine.state().cursor.unwrap_or(Seq(0));
        let (changes, new_cursor) = self.server.changes_since(cursor);
        self.clients[c]
            .engine
            .on_remote_changes(changes, new_cursor);
        let root = LocalId(self.clients[c].fs.root);
        let now = self.tick();
        let snap = self.clients[c].fs.scan(now);
        self.clients[c].engine.on_local_snapshot(root, snap);
        self.check(c);
    }

    /// Fetch, scan and plan without executing (for scenarios with events in between).
    pub fn plan_client(&mut self, c: usize) -> Vec<Op> {
        self.scan_client(c);
        let ops = self.clients[c].engine.plan();
        self.clients[c].persisted = self.clients[c].engine.state().clone();
        ops
    }

    /// Executes an operation but does not report the result yet (see [`Self::deliver`]).
    pub fn execute(&mut self, c: usize, op: Op) -> Outcome {
        match op {
            Op::Local(id, lop) => {
                if matches!(
                    lop,
                    xlrx_sync::LocalOp::Download { .. } | xlrx_sync::LocalOp::Replace { .. }
                ) {
                    self.transfers.downloads += 1;
                }
                let clock = self.tick();
                Outcome::Local(id, driver::exec_local(&mut self.clients[c].fs, &lop, clock))
            }
            Op::Remote(id, rop) => {
                if matches!(rop, RemoteOp::CreateFile { .. } | RemoteOp::Upload { .. }) {
                    self.transfers.uploads += 1;
                }
                let known = self.server.known_result(c, id).is_some();
                let res = if known || driver::source_ok(&self.clients[c].fs, &rop) {
                    let device = self.clients[c].device.clone();
                    self.server.apply(c, id, &rop, &device)
                } else {
                    RemoteResult::SourceChanged
                };
                Outcome::Remote(id, res)
            }
        }
    }

    /// Reports a result to the engine (possibly late).
    pub fn deliver(&mut self, c: usize, o: Outcome) {
        match o {
            Outcome::Local(id, r) => self.clients[c].engine.on_local_result(id, r),
            Outcome::Remote(id, r) => self.clients[c].engine.on_remote_result(id, r),
        }
        self.clients[c].persisted = self.clients[c].engine.state().clone();
        self.check(c);
    }

    fn check(&self, c: usize) {
        if let Err(e) = self.clients[c].engine.check_invariants() {
            panic!("Invariante verletzt bei Client {c}: {e}");
        }
    }

    /// One client: fetch, scan, plan, execute, until there is nothing left to do.
    pub fn sync_client(&mut self, c: usize) {
        for _ in 0..500 {
            let mut ops = self.plan_client(c);
            if ops.is_empty() {
                ops = self.clients[c].engine.plan();
                self.clients[c].persisted = self.clients[c].engine.state().clone();
            }
            if ops.is_empty() {
                return;
            }
            for op in ops {
                let o = self.execute(c, op);
                self.deliver(c, o);
            }
        }
        panic!("Client {c} kommt nicht zur Ruhe");
    }

    /// Sync all clients in turn until nothing changes any more.
    pub fn sync(&mut self) {
        for _ in 0..20 {
            let before = self.server.seq();
            for c in 0..self.clients.len() {
                self.sync_client(c);
            }
            if self.server.seq() == before {
                return;
            }
        }
        panic!("Sync kommt nicht zur Ruhe");
    }

    /// Plans and executes all operations, but crashes before any result is processed.
    /// (Afterwards the server and file system have changed, but the engine knows nothing of it.)
    pub fn execute_then_crash(&mut self, c: usize) {
        let ops = self.plan_client(c);
        for op in ops {
            let _ = self.execute(c, op);
        }
        self.crash(c);
    }

    /// Simulates a crash: the engine restarts from the last persisted state.
    pub fn crash(&mut self, c: usize) {
        let cl = &mut self.clients[c];
        cl.engine = Engine::from_state(cl.persisted.clone());
    }

    // ------------------------------------------------------------ Views

    pub fn server_listing(&self) -> Listing {
        self.server
            .listing()
            .into_iter()
            .map(|(p, (_, c))| (p, c.as_ref().map(tag_of)))
            .collect()
    }

    pub fn client_listing(&self, c: usize) -> Listing {
        self.clients[c]
            .fs
            .listing()
            .into_iter()
            .map(|(p, (_, c))| (p, c.as_ref().map(tag_of)))
            .collect()
    }

    pub fn server_file(&self, path: &str) -> Option<u64> {
        self.server_listing().get(path).copied().flatten()
    }

    /// Checks that all clients have exactly the server's state.
    pub fn assert_converged(&self) {
        let s = self.server_listing();
        for c in 0..self.clients.len() {
            assert_eq!(self.client_listing(c), s, "Client {c} weicht vom Server ab");
        }
    }
}
