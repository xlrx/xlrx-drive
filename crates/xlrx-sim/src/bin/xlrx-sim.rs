//! Command-line interface for long simulation runs.
//!
//! ```text
//! cargo run --release -p xlrx-sim -- --from 0 --count 100000 --clients 3
//! cargo run --release -p xlrx-sim -- --seed 4711 --trace     # inspect a single failure in detail
//! cargo run --release -p xlrx-sim -- --ci --exact-names 300   # server-side name variants (SMB)
//! cargo run --release -p xlrx-sim -- --coarse 8 --defer 300   # coarse timestamps, late results
//! ```

use xlrx_sim::{SimConfig, run};

fn main() {
    let mut cfg = SimConfig::default();
    let mut from = 0u64;
    let mut count = 1000u64;
    let mut trace = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        match a.as_str() {
            "--from" => from = val(),
            "--count" => count = val(),
            "--seed" => {
                from = val();
                count = 1;
            }
            "--clients" => cfg.clients = val() as usize,
            "--steps" => cfg.steps = val() as usize,
            "--ci" => cfg.case_insensitive_local = true,
            "--trace" => trace = true,
            "--strict" => cfg.strict_rules = true,
            "--defer" => cfg.p_defer_result = val() as u32,
            "--exact-names" => cfg.p_server_exact_names = val() as u32,
            "--coarse" => cfg.mtime_granularity = val().max(1) as i64,
            other => {
                eprintln!("Unbekannte Option {other}");
                std::process::exit(2);
            }
        }
    }
    let mut failures = 0;
    let mut total = xlrx_sim::SimStats::default();
    for seed in from..from + count {
        match run(seed, &cfg) {
            Ok(s) => {
                total.user_ops += s.user_ops;
                total.sync_ops += s.sync_ops;
                total.crashes += s.crashes;
                total.conflicts += s.conflicts;
                total.breakers += s.breakers;
                if s.breakers > 0 {
                    println!(
                        "Hinweis Seed {seed}: Sicherheitsnetz {}× genutzt",
                        s.breakers
                    );
                }
            }
            Err(f) => {
                failures += 1;
                if trace {
                    println!("{f}");
                } else {
                    println!(
                        "FEHLER Seed {}: {}",
                        f.seed,
                        f.reason.lines().next().unwrap_or("")
                    );
                }
            }
        }
    }
    println!(
        "{count} Läufe, {failures} Fehler · {} Nutzeraktionen, {} Sync-Operationen, {} Abstürze, {} Server-Konflikte, Sicherheitsnetz {}×",
        total.user_ops, total.sync_ops, total.crashes, total.conflicts, total.breakers
    );
    if failures > 0 {
        std::process::exit(1);
    }
}
