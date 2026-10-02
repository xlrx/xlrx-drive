//! Behavior tests of the sync engine as readable scenarios.
//! Each test describes a rule from `docs/adr/0001-sync-engine.md`.

use xlrx_sim::scenario::World;

fn files(w: &World) -> Vec<(String, Option<u64>)> {
    w.server_listing().into_iter().collect()
}

#[test]
fn neue_datei_kommt_auf_allen_geraeten_an() {
    let mut w = World::new(2, false);
    w.client_write(0, "/Projekte/Bericht.txt", 1);
    w.sync();
    assert_eq!(w.server_file("/Projekte/Bericht.txt"), Some(1));
    w.assert_converged();
}

#[test]
fn ersteinrichtung_verknuepft_gleiche_staende_ohne_uebertragung() {
    // Typical when migrating from Synology Drive: server and Mac already have the same files.
    let mut w = World::new(1, false);
    w.server_write("/a.txt", 1);
    w.server_write("/Ordner/b.txt", 2);
    w.client_write(0, "/a.txt", 1);
    w.client_write(0, "/Ordner/b.txt", 2);
    w.sync();
    w.assert_converged();
    assert_eq!(
        w.transfers.downloads, 0,
        "nichts darf heruntergeladen werden"
    );
    assert_eq!(w.transfers.uploads, 0, "nichts darf hochgeladen werden");
}

#[test]
fn gleichzeitiges_bearbeiten_erzeugt_konfliktkopie_statt_ueberschreiben() {
    let mut w = World::new(2, false);
    w.client_write(0, "/a.txt", 1);
    w.sync();
    w.client_write(0, "/a.txt", 2); // Mac 0 edits
    w.client_write(1, "/a.txt", 3); // Mac 1 edits at the same time
    w.sync();
    w.assert_converged();
    let contents: Vec<u64> = w.server_listing().values().filter_map(|c| *c).collect();
    assert!(
        contents.contains(&2) && contents.contains(&3),
        "beide Fassungen bleiben: {:?}",
        files(&w)
    );
    assert_eq!(
        w.server_listing().len(),
        2,
        "Original + eine Konfliktkopie: {:?}",
        files(&w)
    );
}

#[test]
fn bearbeiten_gewinnt_gegen_loeschen_auf_dem_server() {
    let mut w = World::new(1, false);
    w.client_write(0, "/a.txt", 1);
    w.sync();
    w.server_rm("/a.txt"); // someone deletes it in the web UI
    w.client_write(0, "/a.txt", 2); // meanwhile it is edited locally
    w.sync();
    assert_eq!(w.server_file("/a.txt"), Some(2));
    w.assert_converged();
}

#[test]
fn bearbeiten_gewinnt_gegen_lokales_loeschen() {
    let mut w = World::new(1, false);
    w.client_write(0, "/a.txt", 1);
    w.sync();
    w.client_rm(0, "/a.txt");
    w.server_write("/a.txt", 2);
    w.sync();
    assert_eq!(w.server_file("/a.txt"), Some(2));
    w.assert_converged();
}

#[test]
fn ordner_loeschen_behaelt_neue_datei_darin() {
    let mut w = World::new(2, false);
    w.client_write(0, "/Ordner/alt.txt", 1);
    w.sync();
    w.client_rm(0, "/Ordner"); // Mac 0 deletes the folder
    w.client_write(1, "/Ordner/neu.txt", 2); // Mac 1 puts something into it at the same time
    w.sync();
    assert_eq!(
        w.server_file("/Ordner/neu.txt"),
        Some(2),
        "neue Datei bleibt: {:?}",
        files(&w)
    );
    assert_eq!(
        w.server_file("/Ordner/alt.txt"),
        None,
        "gelöschte Datei bleibt gelöscht"
    );
    w.assert_converged();
}

#[test]
fn atomic_save_ist_eine_aenderung_und_behaelt_die_identitaet() {
    let mut w = World::new(1, false);
    w.client_write(0, "/Text.pages", 1);
    w.sync();
    let before = w.server_node("/Text.pages");
    w.client_atomic_save(0, "/Text.pages", 2); // new file renamed over the old one
    w.sync();
    assert_eq!(w.server_file("/Text.pages"), Some(2));
    assert_eq!(
        w.server_node("/Text.pages"),
        before,
        "Server-Knoten (und damit Versionen) bleibt erhalten"
    );
    w.assert_converged();
}

#[test]
fn namen_tauschen_lokal() {
    let mut w = World::new(2, false);
    w.client_write(0, "/a", 1);
    w.client_write(0, "/b", 2);
    w.sync();
    w.client_mv(0, "/a", "/tmp");
    w.client_mv(0, "/b", "/a");
    w.client_mv(0, "/tmp", "/b");
    w.sync();
    assert_eq!(w.server_file("/a"), Some(2));
    assert_eq!(w.server_file("/b"), Some(1));
    w.assert_converged();
}

#[test]
fn namen_tauschen_auf_dem_server() {
    let mut w = World::new(1, false);
    w.client_write(0, "/a", 1);
    w.client_write(0, "/b", 2);
    w.sync();
    w.server_mv("/a", "/tmp");
    w.server_mv("/b", "/a");
    w.server_mv("/tmp", "/b");
    w.sync();
    assert_eq!(w.client_listing(0).get("/a"), Some(&Some(2)));
    assert_eq!(w.client_listing(0).get("/b"), Some(&Some(1)));
    w.assert_converged();
}

#[test]
fn verschieben_ueber_kreuz_fuehrt_nicht_zu_zyklus() {
    let mut w = World::new(1, false);
    w.client_mkdir(0, "/A");
    w.client_mkdir(0, "/B");
    w.client_write(0, "/A/a.txt", 1);
    w.client_write(0, "/B/b.txt", 2);
    w.sync();
    w.client_mv(0, "/B", "/A/B"); // locally: B into A
    w.server_mv("/A", "/B/A"); // server: A into B
    w.sync();
    w.assert_converged();
    let tags: Vec<u64> = w.server_listing().values().filter_map(|c| *c).collect();
    assert!(
        tags.contains(&1) && tags.contains(&2),
        "beide Dateien bleiben: {:?}",
        files(&w)
    );
}

#[test]
fn datei_wird_zu_gleichnamigem_ordner() {
    let mut w = World::new(2, false);
    w.client_write(0, "/Notiz", 1);
    w.sync();
    w.client_mv(0, "/Notiz", "/tmp");
    w.client_mkdir(0, "/Notiz");
    w.client_mv(0, "/tmp", "/Notiz/Notiz");
    w.sync();
    assert_eq!(w.server_file("/Notiz/Notiz"), Some(1));
    w.assert_converged();
}

#[test]
fn absturz_nach_upload_erzeugt_keine_duplikate() {
    let mut w = World::new(1, false);
    w.client_write(0, "/a.txt", 1);
    w.sync();
    w.client_write(0, "/b.txt", 2);
    w.client_mv(0, "/a.txt", "/c.txt");
    // Upload and rename reach the server, but the client crashes before processing the
    // responses. After restarting, it repeats the operations with the same ID.
    w.execute_then_crash(0);
    w.sync();
    assert_eq!(
        w.server_listing().len(),
        2,
        "keine doppelte Datei: {:?}",
        files(&w)
    );
    assert_eq!(w.server_file("/c.txt"), Some(1));
    assert_eq!(w.server_file("/b.txt"), Some(2));
    w.assert_converged();
}

#[test]
fn gross_klein_umbenennung_auf_apfs() {
    let mut w = World::new(2, true);
    w.client_write(0, "/bericht.txt", 1);
    w.sync();
    w.client_mv(0, "/bericht.txt", "/Bericht.txt");
    w.sync();
    assert_eq!(w.server_file("/Bericht.txt"), Some(1));
    assert_eq!(w.server_file("/bericht.txt"), None);
    w.assert_converged();
}
