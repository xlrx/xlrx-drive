//! Zufällige Simulationsläufe bei jedem `cargo test`.
//!
//! Anzahl über `XLRX_SIM_SEEDS` (Standard 200 je Variante). Die CI nutzt mehr; nachts laufen
//! hunderttausende Seeds über `cargo run --release -p xlrx-sim`.

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
    // Zugriff per SMB/Shell auf dem NAS: „A“ und „a“ im selben Ordner, Mac ohne Unterscheidung.
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
