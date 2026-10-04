//! Run control: random user actions, sync steps, network faults and crashes, followed by a
//! settle phase and checks for convergence and data preservation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

use xlrx_proto::{ContentHash, FileContent, Kind, Name, NodeId, Seq};
use xlrx_sync::{
    Config, Engine, LocalEntry, LocalId, LocalObservation, LocalOp, LocalResult, Op, RemoteOp,
    RemoteResult, State,
};

use crate::fs::SimFs;
use crate::rng::Rng;
use crate::server::SimServer;
use crate::trace::TraceHash;

/// Settings for a simulation run.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub clients: usize,
    /// Steps with user actions and faults; the settle phase follows afterwards.
    pub steps: usize,
    pub case_insensitive_local: bool,
    /// Probabilities in per mille.
    pub p_user_client: u32,
    pub p_user_server: u32,
    pub p_crash: u32,
    pub p_drop_request: u32,
    pub p_drop_response: u32,
    /// Crash between executing an operation and processing its result.
    pub p_crash_after_effect: u32,
    /// Result arrives late (after further scans, fetches and planning passes), as with
    /// operations executed concurrently in the real client.
    pub p_defer_result: u32,
    /// Share of server user actions (create, move) that check names exactly instead of
    /// case-insensitively (SMB/shell access). Produces variants like "A" next to "a".
    pub p_server_exact_names: u32,
    /// Resolution of local timestamps (1 = exact, larger = coarse, as with exFAT).
    /// All contents then also have the same size (worst case for fingerprints).
    pub mtime_granularity: i64,
    /// Strict: if the engine's safety net has to intervene, the run counts as a failure
    /// (this reveals gaps in the rules, even if the data is safe).
    pub strict_rules: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            clients: 2,
            steps: 400,
            case_insensitive_local: false,
            p_user_client: 250,
            p_user_server: 60,
            p_crash: 15,
            p_drop_request: 60,
            p_drop_response: 60,
            p_crash_after_effect: 30,
            p_defer_result: 100,
            p_server_exact_names: 0,
            mtime_granularity: 1,
            strict_rules: false,
        }
    }
}

#[derive(Debug)]
pub struct SimFailure {
    pub seed: u64,
    pub reason: String,
    pub trace: String,
}

impl std::fmt::Display for SimFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Seed {}: {}", self.seed, self.reason)?;
        write!(f, "{}", self.trace)
    }
}

#[derive(Debug, Default, Clone)]
pub struct SimStats {
    pub user_ops: usize,
    pub sync_ops: usize,
    pub crashes: usize,
    pub conflicts: usize,
    /// How often the engine's safety net had to break a wait cycle.
    pub breakers: u64,
    /// Hash over every trace line of the run (see `tests/golden/`): equal for the same seed as long
    /// as the behaviour of engine and simulator does not change.
    pub trace_hash: u64,
}

struct Client {
    idx: usize,
    device: String,
    fs: SimFs,
    engine: Engine,
    persisted: State,
    queue: VecDeque<Op>,
    /// Executed operations whose result has not been processed yet.
    results: VecDeque<Deferred>,
}

enum Deferred {
    Local(xlrx_sync::OpId, LocalResult),
    Remote(xlrx_sync::OpId, RemoteResult),
}

struct Sim {
    seed: u64,
    cfg: SimConfig,
    rng: Rng,
    server: SimServer,
    clients: Vec<Client>,
    clock: i64,
    next_tag: u64,
    /// All contents ever written by users.
    written: BTreeSet<u64>,
    /// Contents a user deliberately overwrote or deleted (where they saw them).
    removed: BTreeSet<u64>,
    trace: VecDeque<String>,
    hash: TraceHash,
    stats: SimStats,
}

const NAMES: &[&str] = &["a", "b", "c", "Bericht.txt", "x y.pdf", "Ä", "d"];
const NAMES_CI: &[&str] = &["a", "A", "b", "Bericht.txt", "bericht.TXT", "Ä", "ä"];
const TRACE_LEN: usize = 400;

fn content(tag: u64) -> FileContent {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&tag.to_le_bytes());
    FileContent {
        hash: ContentHash(h),
        size: tag % 997 + 1,
    }
}

fn tag_of(c: &FileContent) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&c.hash.0[..8]);
    u64::from_le_bytes(b)
}

/// Performs a complete simulation run.
pub fn run(seed: u64, cfg: &SimConfig) -> Result<SimStats, SimFailure> {
    let mut sim = Sim::new(seed, cfg.clone());
    sim.run()
}

impl Sim {
    fn new(seed: u64, cfg: SimConfig) -> Self {
        let server = SimServer::new();
        let clients = (0..cfg.clients)
            .map(|idx| {
                let device = format!("Gerät{idx}");
                let config = Config {
                    remote_root: server.root,
                    device: device.clone(),
                    local_case_insensitive: cfg.case_insensitive_local,
                    max_unconfirmed_deletes: usize::MAX,
                };
                let engine = Engine::new(config);
                let persisted = engine.state().clone();
                let mut fs = SimFs::new(cfg.case_insensitive_local, 1_000_000 * (idx as u64 + 1));
                fs.granularity = cfg.mtime_granularity.max(1);
                Client {
                    idx,
                    device,
                    fs,
                    engine,
                    persisted,
                    queue: VecDeque::new(),
                    results: VecDeque::new(),
                }
            })
            .collect();
        Self {
            seed,
            rng: Rng::new(seed),
            cfg,
            server,
            clients,
            clock: 1,
            next_tag: 1,
            written: BTreeSet::new(),
            removed: BTreeSet::new(),
            trace: VecDeque::new(),
            hash: TraceHash::default(),
            stats: SimStats::default(),
        }
    }

    /// Records a trace line: in the ring buffer shown on failure, and in the run's hash.
    fn log(&mut self, s: String) {
        self.hash.line(&s);
        self.log_detail(s);
    }

    /// Records a line only for the failure report (diagnostic dumps that are not behaviour).
    fn log_detail(&mut self, s: String) {
        if self.trace.len() >= TRACE_LEN {
            self.trace.pop_front();
        }
        self.trace.push_back(s);
    }

    fn fail(&self, reason: String) -> SimFailure {
        let mut trace = String::new();
        for t in &self.trace {
            let _ = writeln!(trace, "  {t}");
        }
        for c in &self.clients {
            let _ = writeln!(
                trace,
                "--- Zustand C{} ---\n{}",
                c.idx,
                c.engine.debug_summary()
            );
        }
        SimFailure {
            seed: self.seed,
            reason,
            trace,
        }
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn new_content(&mut self) -> FileContent {
        let t = self.next_tag;
        self.next_tag += 1;
        self.written.insert(t);
        let mut c = content(t);
        if self.cfg.mtime_granularity > 1 {
            // Worst case: changes without a size change, so the fingerprint then differs only in
            // the (coarse) timestamps.
            c.size = 64;
        }
        c
    }

    fn names(&self) -> &'static [&'static str] {
        if self.cfg.case_insensitive_local {
            NAMES_CI
        } else {
            NAMES
        }
    }

    fn random_name(&mut self) -> Name {
        let names = self.names();
        let n = names[self.rng.below(names.len())];
        Name::new(n).unwrap_or_else(|_| Name::new("x").expect("gültiger Name"))
    }

    fn run(&mut self) -> Result<SimStats, SimFailure> {
        for step in 0..self.cfg.steps {
            let roll = self.rng.below(1000) as u32;
            if roll < self.cfg.p_user_client {
                let c = self.rng.below(self.clients.len());
                self.user_op_client(c);
            } else if roll < self.cfg.p_user_client + self.cfg.p_user_server {
                self.user_op_server();
            } else if roll < self.cfg.p_user_client + self.cfg.p_user_server + self.cfg.p_crash {
                let c = self.rng.below(self.clients.len());
                self.crash(c, step);
            } else {
                let c = self.rng.below(self.clients.len());
                self.client_step(c, true)?;
            }
        }
        self.settle()?;
        self.verify()?;
        self.stats.breakers = self
            .clients
            .iter()
            .map(|c| c.engine.state().breakers_used)
            .sum();
        if self.cfg.strict_rules && self.stats.breakers > 0 {
            return Err(self.fail("Sicherheitsnetz wurde benötigt (Regellücke)".into()));
        }
        self.stats.trace_hash = self.hash.value();
        Ok(self.stats.clone())
    }

    // ------------------------------------------------------------------ User actions

    fn user_op_client(&mut self, ci: usize) {
        self.stats.user_ops += 1;
        let clock = self.tick();
        let all: Vec<u64> = self.clients[ci].fs.inodes.keys().copied().collect();
        let mut dirs: Vec<u64> = all
            .iter()
            .copied()
            .filter(|i| self.clients[ci].fs.is_dir(*i))
            .collect();
        dirs.push(self.clients[ci].fs.root);
        let files: Vec<u64> = all
            .iter()
            .copied()
            .filter(|i| !self.clients[ci].fs.is_dir(*i))
            .collect();
        let kind = self.rng.below(100);
        let dir = *self.rng.pick(&dirs).unwrap_or(&self.clients[ci].fs.root);
        let name = self.random_name();
        let msg = if kind < 25 {
            let c = self.new_content();
            let r = self.clients[ci]
                .fs
                .create(dir, &name, Kind::File, Some(c), clock);
            if r.is_err() {
                self.removed.insert(tag_of(&c));
            }
            format!(
                "U{ci} neue Datei {:?}/{name:?} {:?} → {r:?}",
                self.clients[ci].fs.path(dir),
                tag_of(&c)
            )
        } else if kind < 35 {
            let r = self.clients[ci]
                .fs
                .create(dir, &name, Kind::Dir, None, clock);
            format!(
                "U{ci} mkdir {:?}/{name:?} → {r:?}",
                self.clients[ci].fs.path(dir)
            )
        } else if kind < 55 {
            let Some(&f) = self.rng.pick(&files) else {
                return;
            };
            let c = self.new_content();
            if let Some(old) = self.clients[ci].fs.inodes.get(&f).and_then(|i| i.content) {
                self.removed.insert(tag_of(&old));
            }
            let _ = self.clients[ci].fs.write(f, c, clock);
            format!(
                "U{ci} schreibt {:?} {}",
                self.clients[ci].fs.path(f),
                tag_of(&c)
            )
        } else if kind < 65 {
            // Atomic save: write a new file, rename it over the original.
            let Some(&f) = self.rng.pick(&files) else {
                return;
            };
            let Some(orig) = self.clients[ci].fs.inodes.get(&f).cloned() else {
                return;
            };
            let c = self.new_content();
            let tmp = Name::new(&format!(".tmp{clock}")).expect("gültig");
            let Ok(t) = self.clients[ci]
                .fs
                .create(orig.parent, &tmp, Kind::File, Some(c), clock)
            else {
                return;
            };
            match self.clients[ci]
                .fs
                .rename(t, orig.parent, &orig.name, true, clock)
            {
                Ok(Some(old)) => {
                    if let Some(oc) = old.content {
                        self.removed.insert(tag_of(&oc));
                    }
                }
                _ => {
                    let _ = self.clients[ci].fs.unlink(t);
                    self.removed.insert(tag_of(&c));
                }
            }
            format!(
                "U{ci} atomic-save {:?} {}",
                self.clients[ci].fs.path(f).or(Some(orig.name.to_string())),
                tag_of(&c)
            )
        } else if kind < 82 {
            let Some(&x) = self.rng.pick(&all) else {
                return;
            };
            let replace = self.rng.chance(200);
            let target_name = if self.rng.chance(300) {
                self.clients[ci]
                    .fs
                    .inodes
                    .get(&x)
                    .map(|i| i.name.clone())
                    .unwrap_or(name)
            } else {
                name
            };
            let from = self.clients[ci].fs.path(x);
            match self.clients[ci]
                .fs
                .rename(x, dir, &target_name, replace, clock)
            {
                Ok(Some(old)) => {
                    if let Some(oc) = old.content {
                        self.removed.insert(tag_of(&oc));
                    }
                    format!(
                        "U{ci} mv(ersetzt) {from:?} → {:?}",
                        self.clients[ci].fs.path(x)
                    )
                }
                r => format!(
                    "U{ci} mv {from:?} → {:?}/{target_name:?} {:?}",
                    self.clients[ci].fs.path(dir),
                    r.map(|_| ())
                ),
            }
        } else if kind < 92 {
            let Some(&f) = self.rng.pick(&files) else {
                return;
            };
            let p = self.clients[ci].fs.path(f);
            if let Ok(i) = self.clients[ci].fs.unlink(f)
                && let Some(c) = i.content
            {
                self.removed.insert(tag_of(&c));
            }
            format!("U{ci} rm {p:?}")
        } else {
            let cands: Vec<u64> = dirs
                .iter()
                .copied()
                .filter(|d| *d != self.clients[ci].fs.root)
                .collect();
            let Some(&d) = self.rng.pick(&cands) else {
                return;
            };
            let p = self.clients[ci].fs.path(d);
            for i in self.clients[ci].fs.rm_rf(d) {
                if let Some(c) = i.content {
                    self.removed.insert(tag_of(&c));
                }
            }
            format!("U{ci} rm -rf {p:?}")
        };
        self.log(msg);
    }

    fn user_op_server(&mut self) {
        self.stats.user_ops += 1;
        let alive: Vec<NodeId> = self
            .server
            .nodes
            .iter()
            .filter(|(_, x)| x.alive)
            .map(|(k, _)| *k)
            .collect();
        let mut dirs: Vec<NodeId> = alive
            .iter()
            .copied()
            .filter(|n| self.server.is_dir(*n))
            .collect();
        dirs.push(self.server.root);
        let files: Vec<NodeId> = alive
            .iter()
            .copied()
            .filter(|n| !self.server.is_dir(*n))
            .collect();
        let dir = *self.rng.pick(&dirs).unwrap_or(&self.server.root);
        let name = self.random_name();
        let kind = self.rng.below(100);
        self.server.exact_names = self.rng.chance(self.cfg.p_server_exact_names);
        let msg = if kind < 30 {
            let c = self.new_content();
            let r = self.server.create(dir, &name, Kind::File, Some(c));
            if r.is_err() {
                self.removed.insert(tag_of(&c));
            }
            format!(
                "S neue Datei {:?}/{name:?} {} → {r:?}",
                self.server.path(dir),
                tag_of(&c)
            )
        } else if kind < 40 {
            let r = self.server.create(dir, &name, Kind::Dir, None);
            format!("S mkdir {:?}/{name:?} → {r:?}", self.server.path(dir))
        } else if kind < 60 {
            let Some(&f) = self.rng.pick(&files) else {
                return;
            };
            let c = self.new_content();
            if let Some(old) = self.server.content_of(f) {
                self.removed.insert(tag_of(&old));
            }
            let _ = self.server.write(f, c);
            format!("S schreibt {:?} {}", self.server.path(f), tag_of(&c))
        } else if kind < 80 {
            let Some(&x) = self.rng.pick(&alive) else {
                return;
            };
            let from = self.server.path(x);
            let r = self.server.mv(x, dir, &name);
            format!("S mv {from:?} → {:?}/{name:?} {r:?}", self.server.path(dir))
        } else if kind < 90 {
            let Some(&f) = self.rng.pick(&files) else {
                return;
            };
            let p = self.server.path(f);
            for c in self.server.delete_tree(f) {
                self.removed.insert(tag_of(&c));
            }
            format!("S rm {p:?}")
        } else {
            let cands: Vec<NodeId> = dirs
                .iter()
                .copied()
                .filter(|d| *d != self.server.root)
                .collect();
            let Some(&d) = self.rng.pick(&cands) else {
                return;
            };
            let p = self.server.path(d);
            for c in self.server.delete_tree(d) {
                self.removed.insert(tag_of(&c));
            }
            format!("S rm -rf {p:?}")
        };
        self.server.exact_names = false;
        self.log(msg);
    }

    // ------------------------------------------------------------------ Client steps

    fn crash(&mut self, ci: usize, step: usize) {
        self.stats.crashes += 1;
        let c = &mut self.clients[ci];
        c.engine = Engine::from_state(c.persisted.clone());
        c.queue.clear();
        c.results.clear();
        self.log(format!("C{ci} ABSTURZ (Schritt {step})"));
    }

    fn persist(&mut self, ci: usize) -> Result<(), SimFailure> {
        if let Err(e) = self.clients[ci].engine.check_invariants() {
            return Err(self.fail(format!("Invariante verletzt bei C{ci}: {e}")));
        }
        self.clients[ci].persisted = self.clients[ci].engine.state().clone();
        Ok(())
    }

    fn fetch(&mut self, ci: usize) -> Result<(), SimFailure> {
        let cursor = self.clients[ci].engine.state().cursor.unwrap_or(Seq(0));
        let (changes, new_cursor) = self.server.changes_since(cursor);
        self.log(format!(
            "C{ci} holt {} Änderungen bis {new_cursor:?}",
            changes.len()
        ));
        self.clients[ci]
            .engine
            .on_remote_changes(changes, new_cursor);
        self.persist(ci)
    }

    /// Scan in one of three forms, as used by the real client:
    /// full, as a change list against the engine's state, or as a rescan of a single
    /// folder (FSEvents). The partial rescan honors the client's contract:
    /// moved objects are reported at their new location together with their ancestors, and only
    /// what no longer exists is reported as deleted.
    fn scan(&mut self, ci: usize) -> Result<(), SimFailure> {
        self.scan_with(ci, true)
    }

    /// `partial`: partial rescans allowed. Not in the settle phase, since every change should be
    /// seen there (as in the client, to which FSEvents reports every change).
    fn scan_with(&mut self, ci: usize, partial: bool) -> Result<(), SimFailure> {
        let mode = if self.clients[ci].engine.wants_full_scan() {
            0
        } else if partial {
            self.rng.below(10)
        } else {
            self.rng.below(8)
        };
        let now = self.tick();
        let c = &mut self.clients[ci];
        let root = LocalId(c.fs.root);
        let observed = c.fs.scan(now);
        if mode < 4 {
            c.engine.on_local_snapshot(root, observed);
            self.log(format!("C{ci} scannt (vollständig)"));
            return self.persist(ci);
        }
        let fs_now: BTreeMap<LocalId, LocalEntry> =
            observed.into_iter().map(|o| (o.id, o.entry)).collect();
        let known = c.engine.local_tree();
        // Which objects does this scan cover?
        let scope: BTreeSet<LocalId> = if mode < 8 {
            known
                .ids()
                .into_iter()
                .chain(fs_now.keys().copied())
                .collect()
        } else {
            let dirs: Vec<u64> =
                c.fs.inodes
                    .iter()
                    .filter(|(_, i)| i.kind == Kind::Dir)
                    .map(|(k, _)| *k)
                    .collect();
            let d = if dirs.is_empty() {
                c.fs.root
            } else {
                dirs[self.rng.below(dirs.len())]
            };
            let c = &self.clients[ci];
            let known = c.engine.local_tree();
            let mut sc: BTreeSet<LocalId> = known.descendants(LocalId(d)).into_iter().collect();
            sc.insert(LocalId(d));
            for (id, e) in &fs_now {
                if c.fs.within(id.0, d) || e.parent.0 == d {
                    sc.insert(*id);
                }
            }
            sc
        };
        let c = &self.clients[ci];
        let known = c.engine.local_tree();
        let mut upserts = BTreeMap::new();
        let mut removed = Vec::new();
        for id in &scope {
            if *id == root {
                continue;
            }
            match fs_now.get(id) {
                None => {
                    if known.contains(*id) {
                        removed.push(*id);
                    }
                }
                Some(e) => {
                    if known.get(*id) != Some(e) {
                        upserts.insert(*id, e.clone());
                        // Also report ancestors the engine doesn't know (in this state). The
                        // client knows the full path of the rescanned folder, hence all ancestors.
                        let mut p = e.parent;
                        while p != root {
                            let Some(pe) = fs_now.get(&p) else { break };
                            if known.get(p) != Some(pe) {
                                upserts.insert(p, pe.clone());
                            }
                            p = pe.parent;
                        }
                    }
                }
            }
        }
        let n_up = upserts.len();
        let n_rm = removed.len();
        let ups = upserts
            .into_iter()
            .map(|(id, entry)| LocalObservation { id, entry })
            .collect();
        self.clients[ci].engine.on_local_changes(ups, removed);
        self.log(format!(
            "C{ci} scannt ({}, {n_up} geändert, {n_rm} entfernt)",
            if mode < 8 {
                "Änderungen"
            } else {
                "Teil-Rescan"
            }
        ));
        self.persist(ci)
    }

    fn plan(&mut self, ci: usize) -> Result<usize, SimFailure> {
        let before = self.clients[ci].engine.state().breakers_used;
        let mutations = self.clients[ci].engine.state().synced.mutations();
        let ops = self.clients[ci].engine.plan();
        if self.clients[ci].engine.state().breakers_used != before {
            let b = self.clients[ci].engine.state().last_breaker.clone();
            let summary = self.clients[ci].engine.debug_summary();
            self.log(format!("C{ci} SICHERHEITSNETZ {b:?}"));
            self.log_detail(format!("Zustand danach:\n{summary}"));
        }
        // Incremental planning must not miss anything: if it yields nothing, a full planning
        // pass must not find anything either.
        let fresh_ops = ops.iter().any(|op| matches!(op, Op::Local(..)))
            || ops.iter().any(|op| matches!(op, Op::Remote(..)));
        if !fresh_ops
            && self.clients[ci].engine.state().synced.mutations() == mutations
            && let Err(e) = self.clients[ci].engine.verify_incremental()
        {
            return Err(self.fail(format!(
                "Inkrementelle Planung unvollständig bei C{ci}: {e}"
            )));
        }
        let n = ops.len();
        for op in &ops {
            self.log(format!("C{ci} plant {op:?}"));
        }
        self.clients[ci].queue.extend(ops);
        self.persist(ci)?;
        Ok(n)
    }

    /// A random step of one client. `faults`: network faults and crashes allowed.
    fn client_step(&mut self, ci: usize, faults: bool) -> Result<(), SimFailure> {
        match self.rng.below(11) {
            0 | 1 => self.fetch(ci),
            2 | 3 => self.scan(ci),
            4 | 5 => self.plan(ci).map(|_| ()),
            6 => self.deliver_one(ci),
            _ => self.exec_one(ci, faults),
        }
    }

    /// Processes a random late result.
    fn deliver_one(&mut self, ci: usize) -> Result<(), SimFailure> {
        let len = self.clients[ci].results.len();
        if len == 0 {
            return Ok(());
        }
        let i = self.rng.below(len);
        let Some(d) = self.clients[ci].results.remove(i) else {
            return Ok(());
        };
        match d {
            Deferred::Local(id, r) => {
                self.log(format!("C{ci} verspätetes Ergebnis {id:?} {r:?}"));
                self.clients[ci].engine.on_local_result(id, r);
            }
            Deferred::Remote(id, r) => {
                self.log(format!("C{ci} verspätetes Ergebnis {id:?} {r:?}"));
                self.clients[ci].engine.on_remote_result(id, r);
            }
        }
        self.persist(ci)
    }

    fn exec_one(&mut self, ci: usize, faults: bool) -> Result<(), SimFailure> {
        let len = self.clients[ci].queue.len();
        if len == 0 {
            return Ok(());
        }
        // Operations run concurrently in the real client: random order.
        let i = if faults { self.rng.below(len) } else { 0 };
        let Some(op) = self.clients[ci].queue.remove(i) else {
            return Ok(());
        };
        self.stats.sync_ops += 1;
        match op {
            Op::Local(id, lop) => {
                let res = self.exec_local(ci, &lop);
                self.log(format!("C{ci} führt aus {id:?} {lop:?} → {res:?}"));
                if faults && self.rng.chance(self.cfg.p_crash_after_effect) {
                    self.crash(ci, 0);
                    return Ok(());
                }
                if faults && self.rng.chance(self.cfg.p_defer_result) {
                    self.clients[ci].results.push_back(Deferred::Local(id, res));
                    return Ok(());
                }
                self.clients[ci].engine.on_local_result(id, res);
                self.persist(ci)
            }
            Op::Remote(id, rop) => {
                let res = self.exec_remote(ci, id, &rop, faults);
                self.log(format!("C{ci} sendet {id:?} {rop:?} → {res:?}"));
                if matches!(res, RemoteResult::Conflict { .. }) {
                    self.stats.conflicts += 1;
                }
                if faults && self.rng.chance(self.cfg.p_crash_after_effect) {
                    self.crash(ci, 0);
                    return Ok(());
                }
                if faults && self.rng.chance(self.cfg.p_defer_result) {
                    self.clients[ci]
                        .results
                        .push_back(Deferred::Remote(id, res));
                    return Ok(());
                }
                self.clients[ci].engine.on_remote_result(id, res);
                self.persist(ci)
            }
        }
    }

    fn exec_local(&mut self, ci: usize, op: &LocalOp) -> LocalResult {
        let clock = self.tick();
        crate::driver::exec_local(&mut self.clients[ci].fs, op, clock)
    }

    fn exec_remote(
        &mut self,
        ci: usize,
        id: xlrx_sync::OpId,
        op: &RemoteOp,
        faults: bool,
    ) -> RemoteResult {
        if faults && self.rng.chance(self.cfg.p_drop_request) {
            return RemoteResult::Transient;
        }
        // As in the real protocol: the client first asks whether the server already knows the
        // operation; only a new operation needs the (unchanged) source file.
        let known = self.server.known_result(self.clients[ci].idx, id);
        if known.is_none() && !crate::driver::source_ok(&self.clients[ci].fs, op) {
            return RemoteResult::SourceChanged;
        }
        let device = self.clients[ci].device.clone();
        let res = self.server.apply(self.clients[ci].idx, id, op, &device);
        if faults && self.rng.chance(self.cfg.p_drop_response) {
            return RemoteResult::Transient;
        }
        res
    }

    // ------------------------------------------------------------------ Settle phase and checks

    /// No more user actions or faults: keep syncing until nothing happens any more.
    fn settle(&mut self) -> Result<(), SimFailure> {
        self.log("--- Ruhephase ---".into());
        for ci in 0..self.clients.len() {
            while !self.clients[ci].results.is_empty() {
                self.deliver_one(ci)?;
            }
        }
        let mut quiet = 0;
        for round in 0..80 {
            let seq_before = self.server.seq();
            let mut busy = false;
            for ci in 0..self.clients.len() {
                for _ in 0..400 {
                    self.fetch(ci)?;
                    self.scan_with(ci, false)?;
                    let mut n = self.plan(ci)?;
                    if n == 0 {
                        // State changes without an operation (linking) can enable new ones.
                        n = self.plan(ci)?;
                    }
                    if n == 0 && self.clients[ci].queue.is_empty() {
                        break;
                    }
                    busy = true;
                    while !self.clients[ci].queue.is_empty() {
                        self.exec_one(ci, false)?;
                    }
                }
                if !self.clients[ci].engine.is_idle() {
                    busy = true;
                }
            }
            if !busy && self.server.seq() == seq_before {
                quiet += 1;
                // Wait for several quiet rounds: the engine's safety net only kicks in once a
                // standstill persists across several planning passes.
                if quiet >= 3 {
                    self.log(format!("ruhig nach Runde {round}"));
                    return Ok(());
                }
            } else {
                quiet = 0;
            }
        }
        Err(self.fail("keine Konvergenz: Sync kommt nicht zur Ruhe".into()))
    }

    fn verify(&mut self) -> Result<(), SimFailure> {
        let server = self.server.listing();
        for ci in 0..self.clients.len() {
            let local = self.clients[ci].fs.listing();
            if local != server {
                let mut diff = String::new();
                let keys: BTreeSet<&String> = server.keys().chain(local.keys()).collect();
                for k in keys {
                    let (a, b) = (server.get(k), local.get(k));
                    if a != b {
                        let _ = writeln!(diff, "    {k}: Server {a:?} / C{ci} {b:?}");
                    }
                }
                return Err(self.fail(format!("C{ci} weicht vom Server ab:\n{diff}")));
            }
        }
        // Data preservation: every written content that no user removed must still exist.
        let present: BTreeSet<u64> = server
            .values()
            .filter_map(|(_, c)| c.as_ref().map(tag_of))
            .collect();
        let lost: Vec<u64> = self
            .written
            .iter()
            .filter(|t| !self.removed.contains(t) && !present.contains(t))
            .copied()
            .collect();
        if !lost.is_empty() {
            return Err(self.fail(format!("DATENVERLUST: Inhalte {lost:?} sind verschwunden")));
        }
        let leftovers: Vec<&String> = server.keys().filter(|k| k.contains(".xlrx-")).collect();
        if !leftovers.is_empty() {
            return Err(self.fail(format!("Temporäre Namen übrig geblieben: {leftovers:?}")));
        }
        Ok(())
    }
}
