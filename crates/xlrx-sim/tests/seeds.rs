//! Random simulation runs on every `cargo test`.
//!
//! Count via `XLRX_SIM_SEEDS` (default 200 per variant). CI uses more; at night, hundreds of
//! thousands of seeds run via `cargo run --release -p xlrx-sim`.
//!
//! **Trace-hash gate (ADR 0002):** every run's trace hash is compared with
//! `tests/golden/<variant>.txt` (1000 seeds per variant, one line `seed hash`). A refactor must
//! leave every hash unchanged; a deliberate change of behaviour rewrites the files with
//! `XLRX_SIM_BLESS=1 XLRX_SIM_SEEDS=1000 cargo test --release -p xlrx-sim --test seeds`, and the
//! commit says why.

use std::collections::BTreeMap;
use std::path::PathBuf;

use xlrx_sim::{SimConfig, run};

fn seeds() -> u64 {
    std::env::var("XLRX_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

fn golden_path(variant: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{variant}.txt"))
}

fn read_golden(variant: &str) -> BTreeMap<u64, u64> {
    let path = golden_path(variant);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} fehlt ({e}); mit XLRX_SIM_BLESS=1 erzeugen",
            path.display()
        )
    });
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let (seed, hash) = l.split_once(' ').expect("Zeile `seed hash`");
            (
                seed.parse().expect("Seed"),
                u64::from_str_radix(hash, 16).expect("Hash"),
            )
        })
        .collect()
}

fn write_golden(variant: &str, hashes: &BTreeMap<u64, u64>) {
    let mut text = format!(
        "# Trace-Hashes der Simulator-Variante `{variant}` (ADR 0002, Trace-Hash-Tor).\n\
         # Neu schreiben nur bei gewollter Verhaltensänderung: XLRX_SIM_BLESS=1 XLRX_SIM_SEEDS=1000\n\
         # cargo test --release -p xlrx-sim --test seeds\n"
    );
    for (seed, hash) in hashes {
        text.push_str(&format!("{seed} {hash:016x}\n"));
    }
    let path = golden_path(variant);
    std::fs::create_dir_all(path.parent().expect("Ordner")).expect("Ordner anlegen");
    std::fs::write(&path, text).expect("Golden-Datei schreiben");
}

fn check(variant: &str, cfg: SimConfig, offset: u64) {
    let mut failures = Vec::new();
    let mut hashes = BTreeMap::new();
    for seed in offset..offset + seeds() {
        match run(seed, &cfg) {
            Ok(stats) => {
                hashes.insert(seed, stats.trace_hash);
            }
            Err(f) => failures.push(format!(
                "Seed {}: {}",
                f.seed,
                f.reason.lines().next().unwrap_or("")
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} fehlgeschlagene Läufe (mit `xlrx-sim --seed N --trace` ansehen):\n{}",
        failures.len(),
        failures.join("\n")
    );
    if std::env::var_os("XLRX_SIM_BLESS").is_some() {
        write_golden(variant, &hashes);
        return;
    }
    let golden = read_golden(variant);
    let compared = hashes.keys().filter(|s| golden.contains_key(s)).count();
    assert!(
        compared > 0,
        "keine gemeinsamen Seeds mit {}",
        golden_path(variant).display()
    );
    let changed: Vec<String> = hashes
        .iter()
        .filter(|(seed, hash)| golden.get(seed).is_some_and(|g| g != *hash))
        .map(|(seed, hash)| format!("Seed {seed}: {:016x} statt {:016x}", hash, golden[seed]))
        .collect();
    assert!(
        changed.is_empty(),
        "Verhalten geändert: {} von {compared} Seeds mit anderem Trace-Hash (erste: {}). \
         Gewollt? Dann XLRX_SIM_BLESS=1 und im Commit begründen; vergleichen mit \
         `xlrx-sim --hashes --from N --count 1` vor und nach der Änderung.",
        changed.len(),
        changed
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ")
    );
}

#[test]
fn zwei_clients() {
    check(
        "zwei_clients",
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
        "drei_clients",
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
        "ohne_gross_klein_unterscheidung",
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
        "lange_laeufe",
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
        "namensvarianten_auf_dem_server",
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
        "grobe_zeitstempel_und_spaete_ergebnisse",
        SimConfig {
            mtime_granularity: 8,
            p_defer_result: 300,
            strict_rules: true,
            ..SimConfig::default()
        },
        5_000_000,
    );
}
