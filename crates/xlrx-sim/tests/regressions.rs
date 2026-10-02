//! Regressionstests für Fehler, die ein adversarialer Review der Sync-Engine gefunden hat,
//! sowie für verspätet eintreffende Ergebnisse. Jeder Test beschreibt den Ablauf, der vorher
//! zu Datenverlust, Endlosschleife oder fehlender Konvergenz führte.

use std::sync::mpsc;
use std::time::Duration;

use xlrx_proto::Name;
use xlrx_sim::scenario::{World, content};
use xlrx_sync::{LocalOp, Op, RemoteOp};

fn tags(w: &World) -> Vec<u64> {
    w.server_listing().values().filter_map(|c| *c).collect()
}

/// Führt `f` mit Zeitlimit aus: Eine Endlosschleife soll den Test scheitern lassen, nicht hängen.
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("Zeitlimit überschritten (Endlosschleife?)")
}

/// Server: f gelöscht und mit neuem Inhalt neu angelegt. Lokal: f nur umbenannt.
/// Vorher wurde die lokale Datei dem neuen Server-Knoten zugeschlagen und dann mit dessen
/// Inhalt überschrieben.
#[test]
fn server_loescht_und_legt_neu_an_waehrend_lokal_umbenannt_wird() {
    let mut w = World::new(1, false);
    w.client_write(0, "/f", 1);
    w.sync();
    w.server_rm("/f");
    w.server_write("/f", 2);
    w.client_mv(0, "/f", "/g");
    w.sync();
    w.assert_converged();
    assert_eq!(w.server_file("/g"), Some(1), "{:?}", w.server_listing());
    assert_eq!(w.server_file("/f"), Some(2), "{:?}", w.server_listing());
}

/// Lokal gelöscht, gleichzeitig auf dem Server verschoben (ohne dass der Client es schon weiß).
/// Das Löschen darf die verschobene Datei nicht treffen.
#[test]
fn loeschen_trifft_keine_auf_dem_server_verschobene_datei() {
    let mut w = World::new(1, false);
    w.client_write(0, "/f", 1);
    w.server_mkdir("/Archiv");
    w.sync();
    w.client_rm(0, "/f");
    let ops = w.plan_client(0); // DeleteFile /f
    w.server_mv("/f", "/Archiv/f"); // gleichzeitig in der Web-UI
    for op in ops {
        let o = w.execute(0, op);
        w.deliver(0, o);
    }
    w.sync();
    w.assert_converged();
    assert_eq!(
        w.server_file("/Archiv/f"),
        Some(1),
        "{:?}",
        w.server_listing()
    );
}

/// Lokal umbenannt, gleichzeitig auf dem Server verschoben: Der Server gewinnt.
#[test]
fn verschieben_ueberschreibt_keine_fremde_verschiebung() {
    let mut w = World::new(1, false);
    w.client_write(0, "/f", 1);
    w.server_mkdir("/Archiv");
    w.sync();
    w.client_mv(0, "/f", "/g");
    let ops = w.plan_client(0); // Move /f → /g
    w.server_mv("/f", "/Archiv/f");
    for op in ops {
        let o = w.execute(0, op);
        w.deliver(0, o);
    }
    w.sync();
    w.assert_converged();
    assert_eq!(
        w.server_file("/Archiv/f"),
        Some(1),
        "{:?}",
        w.server_listing()
    );
}

/// Überlange „Endung“: Vorher war der Konfliktname für jeden Zähler gleich und ungültig,
/// die Suche nach einem freien Namen lief endlos.
#[test]
fn mehrere_konflikte_mit_ueberlangem_namen() {
    let listing = within(20, || {
        let long = format!("/1. {}", "x".repeat(240));
        let mut w = World::new(3, false);
        w.client_write(0, &long, 1);
        w.sync();
        w.client_write(0, &long, 2);
        w.client_write(1, &long, 3);
        w.client_write(2, &long, 4);
        w.sync();
        w.assert_converged();
        w.server_listing()
    });
    let t: Vec<u64> = listing.values().filter_map(|c| *c).collect();
    for tag in [2, 3, 4] {
        assert!(t.contains(&tag), "Fassung {tag} fehlt: {listing:?}");
    }
    assert!(listing.keys().all(|k| Name::new(&k[1..]).is_ok()));
}

/// Server-Nutzer löscht einen Ordner samt Inhalt und legt einen leeren gleichnamigen an.
/// Vorher kam der gelöschte Inhalt zurück.
#[test]
fn server_ersetzt_ordner_durch_leeren_gleichnamigen() {
    let mut w = World::new(1, false);
    w.client_write(0, "/D/c", 1);
    w.sync();
    w.server_rm("/D");
    w.server_mkdir("/D");
    w.sync();
    w.assert_converged();
    assert_eq!(w.server_file("/D/c"), None, "{:?}", w.server_listing());
    assert!(w.server_listing().contains_key("/D"));
}

/// Wie oben, aber lokal wurde die Datei im Ordner gerade bearbeitet: Die Änderung gewinnt.
#[test]
fn server_ersetzt_ordner_aber_lokale_aenderung_bleibt() {
    let mut w = World::new(1, false);
    w.client_write(0, "/D/c", 1);
    w.sync();
    w.server_rm("/D");
    w.server_mkdir("/D");
    w.client_write(0, "/D/c", 2);
    w.sync();
    w.assert_converged();
    assert_eq!(w.server_file("/D/c"), Some(2), "{:?}", w.server_listing());
}

#[test]
fn tausch_mit_ueberlangen_namen() {
    let a = format!("/a{}", "x".repeat(240));
    let b = format!("/b{}", "x".repeat(240));
    let mut w = World::new(2, false);
    w.client_write(0, &a, 1);
    w.client_write(0, &b, 2);
    w.sync();
    w.client_mv(0, &a, "/tmp");
    w.client_mv(0, &b, &a);
    w.client_mv(0, "/tmp", &b);
    w.sync();
    assert_eq!(w.server_file(&a), Some(2));
    assert_eq!(w.server_file(&b), Some(1));
    w.assert_converged();
}

/// Gerätename mit „~“, dem Trennzeichen in temporären Ausweichnamen.
#[test]
fn geraetename_mit_tilde() {
    for server_side in [false, true] {
        let mut w = World::new(2, false);
        w.set_device(0, "Max~Mac");
        w.set_device(1, "~");
        w.client_write(0, "/a", 1);
        w.client_write(0, "/b", 2);
        w.sync();
        if server_side {
            w.server_mv("/a", "/tmp");
            w.server_mv("/b", "/a");
            w.server_mv("/tmp", "/b");
        } else {
            w.client_mv(0, "/a", "/tmp");
            w.client_mv(0, "/b", "/a");
            w.client_mv(0, "/tmp", "/b");
        }
        w.sync();
        assert_eq!(w.server_file("/a"), Some(2), "{:?}", w.server_listing());
        assert_eq!(w.server_file("/b"), Some(1), "{:?}", w.server_listing());
        w.assert_converged();
    }
}

/// Per SMB wird auf dem NAS „b“ in „a“ umbenannt, während „A“ existiert. Der Mac kennt nur einen
/// der beiden Namen. Vorher kam der Sync nie zur Ruhe, und die lokale Änderung an b ging nie hoch.
#[test]
fn namensvariante_auf_dem_server_bei_mac_ohne_gross_klein() {
    let (converged, uploaded, listing) = within(20, || {
        let mut w = World::new(1, true);
        w.client_write(0, "/A", 1);
        w.client_write(0, "/b", 2);
        w.sync();
        let nb = w.server_node("/b").expect("b");
        w.server.exact_names = true;
        w.server
            .mv(nb, w.server.root, &Name::new("a").unwrap())
            .expect("SMB-Umbenennung");
        w.server.exact_names = false;
        w.server.write(nb, content(3)).expect("schreiben");
        w.client_write(0, "/b", 4);
        w.sync();
        let l = w.server_listing();
        (l == w.client_listing(0), tags(&w).contains(&4), l)
    });
    assert!(converged, "{listing:?}");
    assert!(uploaded, "lokale Änderung fehlt: {listing:?}");
    assert!(listing.values().any(|c| *c == Some(3)), "{listing:?}");
}

// ------------------------------------------------------------------ verspätete Ergebnisse

/// Ein Download ist fertig, aber bevor sein Ergebnis verarbeitet wird, löscht der Nutzer den
/// Ordner, und ein Scan meldet das. Das Ergebnis darf L nicht inkonsistent machen.
#[test]
fn spaetes_download_ergebnis_nach_geloeschtem_ordner() {
    let mut w = World::new(1, false);
    w.server_mkdir("/D");
    w.sync();
    w.server_write("/D/b", 1);
    let ops = w.plan_client(0);
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Local(_, LocalOp::Download { .. })))
    );
    let outcomes: Vec<_> = ops.into_iter().map(|op| w.execute(0, op)).collect();
    w.client_rm(0, "/D");
    w.scan_client(0);
    for o in outcomes {
        w.deliver(0, o); // prüft die Invarianten
    }
    w.sync();
    w.assert_converged();
    // Der Nutzer hat b nie gesehen: Im Zweifel bleibt es erhalten.
    assert_eq!(w.server_file("/D/b"), Some(1), "{:?}", w.server_listing());
}

/// Upload läuft, währenddessen speichert das Programm erneut per „Atomic Save“ (neue Inode).
/// Vorher entstand eine unnötige Konfliktkopie.
#[test]
fn upload_ergebnis_nach_atomic_save() {
    let mut w = World::new(1, false);
    w.client_write(0, "/f", 1);
    w.sync();
    w.client_write(0, "/f", 2);
    let ops = w.plan_client(0); // Upload f=2
    let outcomes: Vec<_> = ops.into_iter().map(|op| w.execute(0, op)).collect();
    w.client_atomic_save(0, "/f", 3);
    w.scan_client(0);
    for o in outcomes {
        w.deliver(0, o);
    }
    w.sync();
    w.assert_converged();
    assert_eq!(w.server_file("/f"), Some(3));
    assert_eq!(
        w.server_listing().len(),
        1,
        "keine Konfliktkopie: {:?}",
        w.server_listing()
    );
}

/// Neue Datei wird angelegt, währenddessen per „Atomic Save“ ersetzt. Vorher wurde der neue
/// Server-Knoten gelöscht und ein weiterer angelegt (Versionen und Freigaben gingen verloren).
#[test]
fn anlegen_ergebnis_nach_atomic_save_behaelt_den_knoten() {
    let mut w = World::new(1, false);
    w.client_write(0, "/f", 1);
    let ops = w.plan_client(0); // CreateFile f
    let outcomes: Vec<_> = ops.into_iter().map(|op| w.execute(0, op)).collect();
    let node = w.server_node("/f");
    assert!(node.is_some());
    w.client_atomic_save(0, "/f", 2);
    w.scan_client(0);
    for o in outcomes {
        w.deliver(0, o);
    }
    // Sofort planen, ohne neuen vollständigen Scan (wie nach einem FSEvents-Teilscan):
    // Der Knoten darf nicht als lokal gelöscht gelten.
    let ops = w.clients[0].engine.plan();
    assert!(
        !ops.iter()
            .any(|op| matches!(op, Op::Remote(_, RemoteOp::DeleteFile { .. }))),
        "{ops:?}"
    );
    for op in ops {
        let o = w.execute(0, op);
        w.deliver(0, o);
    }
    w.sync();
    w.assert_converged();
    assert_eq!(w.server_file("/f"), Some(2));
    assert_eq!(w.server_node("/f"), node, "derselbe Server-Knoten");
}

// ------------------------------------------------------------------ grobe Zeitstempel

/// Grobe Zeitstempel: Eine Änderung gleicher Größe im selben Zeitstempel-Intervall ändert den
/// Fingerprint nicht. Das Ersetzen muss den Inhalt neu prüfen, sonst ginge die Änderung verloren.
#[test]
fn ersetzen_prueft_inhalt_bei_gleichem_fingerprint() {
    let mut w = World::new(1, false);
    w.clients[0].fs.granularity = 1_000_000_000; // alle Zeitstempel im selben Intervall
    w.client_write(0, "/f", 1);
    w.sync();
    w.server_write("/f", 2);
    let ops = w.plan_client(0);
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Local(_, LocalOp::Replace { .. })))
    );
    // Gleiche Größe wie Inhalt 1 (siehe `content`), gleiches Zeitstempel-Intervall.
    assert_eq!(content(998).size, content(1).size);
    w.client_write(0, "/f", 998);
    for op in ops {
        let o = w.execute(0, op);
        w.deliver(0, o);
    }
    w.sync();
    w.assert_converged();
    let t = tags(&w);
    assert!(
        t.contains(&998),
        "lokale Änderung verloren: {:?}",
        w.server_listing()
    );
    assert!(t.contains(&2), "{:?}", w.server_listing());
}

/// Dasselbe für das Löschen.
#[test]
fn loeschen_prueft_inhalt_bei_gleichem_fingerprint() {
    let mut w = World::new(1, false);
    w.clients[0].fs.granularity = 1_000_000_000;
    w.client_write(0, "/f", 1);
    w.sync();
    w.server_rm("/f");
    let ops = w.plan_client(0);
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Local(_, LocalOp::DeleteFile { .. })))
    );
    w.client_write(0, "/f", 998);
    for op in ops {
        let o = w.execute(0, op);
        w.deliver(0, o);
    }
    w.sync();
    w.assert_converged();
    assert_eq!(w.server_file("/f"), Some(998), "{:?}", w.server_listing());
}
