//! `xlrx-server` – server and administration commands.
//!
//! ```text
//! xlrx-server                         start the server (migrations run automatically)
//! xlrx-server migrate                 run migrations only
//! xlrx-server create-user NAME "Anzeigename" [--admin]
//!                                     create an account, print a setup link
//! xlrx-server invite NAME             new setup link (e.g. after expiry)
//! xlrx-server reset-factors NAME      reset second factors, new setup link
//! xlrx-server gen-secret              generate a new key for XLRX_SECRET_KEY_FILE
//! xlrx-server bench-argon2 [M_KIB T P]  measure a password check on this CPU
//! ```

use std::process::ExitCode;
use std::time::Instant;

use base64::Engine as _;
use tracing_subscriber::EnvFilter;
use xlrx_server::config::ArgonParams;
use xlrx_server::{AppState, Config, users};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Fehler: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("serve");
    match cmd {
        "gen-secret" => {
            let key: [u8; 32] = xlrx_server::auth::tokens::random_bytes();
            println!("{}", base64::engine::general_purpose::STANDARD.encode(key));
            Ok(())
        }
        "bench-argon2" => bench_argon2(&args[1..]),
        "serve" | "migrate" | "create-user" | "invite" | "reset-factors" => {
            let cfg = Config::from_env()?;
            let db = xlrx_server::connect(&cfg.database_url)
                .await
                .map_err(|e| format!("Datenbank: {e}"))?;
            xlrx_server::MIGRATOR
                .run(&db)
                .await
                .map_err(|e| format!("Migration: {e}"))?;
            if cmd == "migrate" {
                println!("Migrationen ausgeführt.");
                return Ok(());
            }
            let state = AppState::new(db.clone(), cfg.clone())?;
            match cmd {
                "serve" => serve(state).await,
                "create-user" => {
                    let (Some(name), Some(display)) = (args.get(1), args.get(2)) else {
                        return Err("Aufruf: create-user NAME \"Anzeigename\" [--admin]".into());
                    };
                    let admin = args.iter().any(|a| a == "--admin");
                    let u = users::create(&db, name, display, None, admin)
                        .await
                        .map_err(|e| format!("{e:?}"))?;
                    let token = users::create_invite(&db, u.id, None)
                        .await
                        .map_err(|e| format!("{e:?}"))?;
                    xlrx_server::audit::log(
                        &db,
                        None,
                        Some(u.id),
                        "user_created",
                        None,
                        serde_json::json!({"via": "cli", "is_admin": admin}),
                    )
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                    println!(
                        "Konto „{}“ angelegt{}.",
                        u.username,
                        if admin { " (Admin)" } else { "" }
                    );
                    println!(
                        "Einrichtungslink ({} h gültig):\n{}",
                        users::INVITE_HOURS,
                        users::setup_url(&state, &token)
                    );
                    Ok(())
                }
                "invite" | "reset-factors" => {
                    let Some(name) = args.get(1) else {
                        return Err(format!("Aufruf: {cmd} NAME"));
                    };
                    let u = users::by_folded(&db, &users::fold(name))
                        .await
                        .map_err(|e| format!("{e:?}"))?
                        .ok_or("Unbekanntes Konto")?;
                    if cmd == "reset-factors" {
                        xlrx_server::api::admin::reset_user_factors(&db, u.id)
                            .await
                            .map_err(|e| format!("{e:?}"))?;
                    }
                    let token = users::create_invite(&db, u.id, None)
                        .await
                        .map_err(|e| format!("{e:?}"))?;
                    xlrx_server::audit::log(
                        &db,
                        None,
                        Some(u.id),
                        if cmd == "invite" {
                            "invite_created"
                        } else {
                            "factors_reset"
                        },
                        None,
                        serde_json::json!({"via": "cli"}),
                    )
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                    println!(
                        "Einrichtungslink ({} h gültig):\n{}",
                        users::INVITE_HOURS,
                        users::setup_url(&state, &token)
                    );
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
        other => Err(format!("Unbekannter Befehl „{other}“")),
    }
}

async fn serve(state: AppState) -> Result<(), String> {
    let bind = state.cfg.bind;
    let db = state.db.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = xlrx_server::cleanup(&db).await {
                tracing::warn!(error = %e, "Aufräumen fehlgeschlagen");
            }
        }
    });
    // Reconciliation: once at startup (changes made while the server was not running), then
    // periodically as a safety net. The watcher (PLAN 4.4) reports changes in between.
    if state.cfg.data_dir.is_some() {
        // Changes a crash interrupted are completed or rolled back before anything else.
        xlrx_server::files::ops::recover(&state, None)
            .await
            .map_err(|e| format!("Offene Vorgänge: {e:?}"))?;
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tick.tick().await;
                xlrx_server::files::roots::scan_all(&st).await;
                if let Err(e) = xlrx_server::files::ops::housekeeping(&st).await {
                    tracing::warn!(error = ?e, "Aufräumen der Ablagen fehlgeschlagen");
                }
            }
        });
    } else {
        tracing::warn!("XLRX_DATA_DIR nicht gesetzt: keine Dateien, nur Konten");
    }
    let app = xlrx_server::router(state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("{bind}: {e}"))?;
    tracing::info!(%bind, "xlrx-server läuft");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .map_err(|e| e.to_string())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("Beende …");
}

/// Measures a password check in order to calibrate the parameters on the NAS (target ~250 ms).
fn bench_argon2(args: &[String]) -> Result<(), String> {
    let d = ArgonParams::default();
    let num = |i: usize, def: u32| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(def);
    let p = ArgonParams {
        m_kib: num(0, d.m_kib),
        t: num(1, d.t),
        p: num(2, d.p),
    };
    let pw = xlrx_server::auth::password::Passwords::new(p, None)?;
    let hash = pw.hash("Benchmark-Passwort-123")?;
    let n = 5;
    let start = Instant::now();
    for _ in 0..n {
        pw.verify(Some(&hash), "Benchmark-Passwort-123");
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
    println!(
        "argon2id m={} KiB t={} p={}: {ms:.0} ms je Prüfung (Ziel ~250 ms; XLRX_ARGON2_M_KIB/_T/_P)",
        p.m_kib, p.t, p.p
    );
    Ok(())
}
