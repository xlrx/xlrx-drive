//! Random simulation runs on every `cargo test`.
//!
//! Count via `XLRX_SIM_SEEDS` (default 200 per variant). CI uses more; at night, hundreds of
//! thousands of seeds run via `cargo run --release -p xlrx-sim`.

use xlrx_sim::{SimConfig, run};

fn seeds() -> u64 {
    std::env::var("XLRX_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

fn check(cfg: SimConfig, offset: u64) {
    let mut failures = Vec::new();
    for seed in offset..offset + seeds() {
        if let Err(f) = run(seed, &cfg) {
            failures.push(format!(
                "Seed {}: {}",
                f.seed,
                f.reason.lines().next().unwrap_or("")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} fehlgeschlagene Läufe (mit `xlrx-sim --seed N --trace` ansehen):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn zwei_clients() {
    check(
        SimConfig {
            strict_rules: true,
            ..SimConfig::default()
        },
        0,
    );
}

#[test]
fn drei_clients() {
    check(
        SimConfig {
            clients: 3,
            strict_rules: true,
            ..SimConfig::default()
        },
        1_000_000,
    );
}

#[test]
fn ohne_gross_klein_unterscheidung() {
    check(
        SimConfig {
            case_insensitive_local: true,
            strict_rules: true,
            ..SimConfig::default()
        },
        2_000_000,
    );
}

#[test]
fn lange_laeufe() {
    check(
        SimConfig {
            steps: 1200,
            clients: 3,
            strict_rules: true,
            ..SimConfig::default()
        },
        3_000_000,
    );
}

#[test]
fn namensvarianten_auf_dem_server() {
    // SMB/shell access on the NAS: "A" and "a" in the same folder, Mac is case-insensitive.
    check(
        SimConfig {
            case_insensitive_local: true,
            p_server_exact_names: 400,
            strict_rules: true,
            ..SimConfig::default()
        },
        4_000_000,
    );
}

#[test]
fn grobe_zeitstempel_und_spaete_ergebnisse() {
    check(
        SimConfig {
            mtime_granularity: 8,
            p_defer_result: 300,
            strict_rules: true,
            ..SimConfig::default()
        },
        5_000_000,
    );
}
