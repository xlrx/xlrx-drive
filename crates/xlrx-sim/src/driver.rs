//! Ausführung geplanter Operationen gegen das simulierte Dateisystem – mit denselben Vorbedingungen,
//! die der echte Client prüft.

use xlrx_proto::{Kind, Name};
use xlrx_sync::{LocalId, LocalOp, LocalResult, RemoteOp};

use crate::fs::SimFs;

/// Führt eine lokale Operation aus. Ist eine Vorbedingung verletzt, bleibt alles unverändert.
pub fn exec_local(fs: &mut SimFs, op: &LocalOp, clock: i64) -> LocalResult {
    let done = |id: u64, fs: &SimFs| LocalResult::Done {
        id: LocalId(id),
        fp: fs.fingerprint(id),
    };
    match op {
        LocalOp::CreateDir { parent, name, .. } => {
            match fs.create(parent.0, name, Kind::Dir, None, clock) {
                Ok(id) => done(id, fs),
                Err(_) => LocalResult::Precondition,
            }
        }
        LocalOp::Download {
            parent,
            name,
            content,
            ..
        } => match fs.create(parent.0, name, Kind::File, Some(*content), clock) {
            Ok(id) => done(id, fs),
            Err(_) => LocalResult::Precondition,
        },
        LocalOp::Replace {
            local,
            expect,
            content,
            ..
        } => {
            if fs.fingerprint(local.0) != Some(*expect) {
                return LocalResult::Precondition;
            }
            let Some(orig) = fs.inodes.get(&local.0).cloned() else {
                return LocalResult::Precondition;
            };
            // Wie im echten Client: temporäre Datei schreiben und atomar über das Original legen.
            let tmp = Name::new(&format!(".xlrx-dl-{clock}")).expect("gültiger Name");
            let Ok(t) = fs.create(orig.parent, &tmp, Kind::File, Some(*content), clock) else {
                return LocalResult::Error;
            };
            match fs.rename(t, orig.parent, &orig.name, true, clock) {
                Ok(_) => done(t, fs),
                Err(_) => {
                    let _ = fs.unlink(t);
                    LocalResult::Error
                }
            }
        }
        LocalOp::Move {
            local,
            from_parent,
            from_name,
            parent,
            name,
            ..
        } => {
            let Some(i) = fs.inodes.get(&local.0) else {
                return LocalResult::Precondition;
            };
            if i.parent != from_parent.0 || i.name != *from_name {
                return LocalResult::Precondition;
            }
            match fs.rename(local.0, parent.0, name, false, clock) {
                Ok(_) => LocalResult::Done {
                    id: *local,
                    fp: fs.fingerprint(local.0),
                },
                Err(_) => LocalResult::Precondition,
            }
        }
        LocalOp::DeleteFile { local, expect, .. } => {
            if fs.fingerprint(local.0) != Some(*expect) {
                return LocalResult::Precondition;
            }
            match fs.unlink(local.0) {
                Ok(_) => LocalResult::Done {
                    id: *local,
                    fp: None,
                },
                Err(_) => LocalResult::Precondition,
            }
        }
        LocalOp::DeleteDir { local, .. } => match fs.rmdir(local.0) {
            Ok(()) => LocalResult::Done {
                id: *local,
                fp: None,
            },
            Err(_) => LocalResult::Precondition,
        },
    }
}

/// Inhalt wird aus der lokalen Datei hochgeladen: Diese muss noch genau dem gehashten Stand entsprechen.
pub fn source_ok(fs: &SimFs, op: &RemoteOp) -> bool {
    match op {
        RemoteOp::CreateFile {
            source,
            fp,
            content,
            ..
        }
        | RemoteOp::Upload {
            source,
            fp,
            content,
            ..
        } => {
            fs.fingerprint(source.0) == Some(*fp)
                && fs.inodes.get(&source.0).and_then(|i| i.content) == Some(*content)
        }
        _ => true,
    }
}
