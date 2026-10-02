//! Regression tests for bugs found by an adversarial review of the sync engine, as well as for
//! results that arrive late. Each test describes the sequence that previously led to data loss,
//! an infinite loop or a failure to converge.

use std::sync::mpsc;
use std::time::Duration;

use xlrx_proto::Name;
use xlrx_sim::scenario::{World, content};
use xlrx_sync::{LocalOp, Op, RemoteOp};

fn tags(w: &World) -> Vec<u64> {
    w.server_listing().values().filter_map(|c| *c).collect()
}

/// Runs `f` with a time limit: an infinite loop should make the test fail, not hang.
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("Zeitlimit überschritten (Endlosschleife?)")
}

/// Server: f deleted and recreated with new content. Locally: f only renamed.
/// Previously the local file was assigned to the new server node and then overwritten with its
/// content.
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

/// Deleted locally, moved on the server at the same time (before the client knows about it).
/// The delete must not hit the moved file.
#[test]
fn loeschen_trifft_keine_auf_dem_server_verschobene_datei() {
    let mut w = World::new(1, false);
    w.client_write(0, "/f", 1);
    w.server_mkdir("/Archiv");
    w.sync();
    w.client_rm(0, "/f");
    let ops = w.plan_client(0); // DeleteFile /f
    w.server_mv("/f", "/Archiv/f"); // at the same time, in the web UI
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

/// Renamed locally, moved on the server at the same time: the server wins.
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

/// Overlong "extension": previously the conflict name was the same and invalid for every
/// counter, so the search for a free name never ended.
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

/// A server user deletes a folder with its contents and creates an empty one with the same name.
/// Previously the deleted content came back.
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

/// As above, but the file in the folder was just edited locally: the edit wins.
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

/// Device name containing "~", the separator in temporary yield names.
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

/// Via SMB, "b" is renamed to "a" on the NAS while "A" exists. The Mac knows only one of the two
/// names. Previously the sync never settled, and the local change to b was never uploaded.
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

// ------------------------------------------------------------------ late results

/// A download has finished, but before its result is processed, the user deletes the folder
/// and a scan reports this. The result must not make L inconsistent.
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
        w.deliver(0, o); // checks the invariants
    }
    w.sync();
    w.assert_converged();
    // The user never saw b: when in doubt, it is kept.
    assert_eq!(w.server_file("/D/b"), Some(1), "{:?}", w.server_listing());
}

/// An upload is running while the application saves again via "atomic save" (new inode).
/// Previously this produced an unnecessary conflict copy.
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

/// A new file is being created and meanwhile replaced via "atomic save". Previously the new
/// server node was deleted and another one created (versions and shares were lost).
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
    // Plan immediately, without a new full scan (as after a partial FSEvents scan):
    // the node must not be considered deleted locally.
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

// ------------------------------------------------------------------ coarse timestamps

/// Coarse timestamps: a same-size change within the same timestamp interval doesn't change the
/// fingerprint. Replacing must re-check the content, otherwise the change would be lost.
#[test]
fn ersetzen_prueft_inhalt_bei_gleichem_fingerprint() {
    let mut w = World::new(1, false);
    w.clients[0].fs.granularity = 1_000_000_000; // all timestamps in the same interval
    w.client_write(0, "/f", 1);
    w.sync();
    w.server_write("/f", 2);
    let ops = w.plan_client(0);
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::Local(_, LocalOp::Replace { .. })))
    );
    // Same size as content 1 (see `content`), same timestamp interval.
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

/// The same for deleting.
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
