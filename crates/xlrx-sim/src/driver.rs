//! Execution of planned operations against the simulated file system, with the same
//! preconditions the real client checks.
//!
//! Destructive operations (`Replace`, `DeleteFile`) check location, fingerprint and, via the hash
//! cache (i.e. rehashed if the fingerprint falls within the unsafe time window), the content.
//! In the real client this is followed by the atomic swap or the move into its own trash, each
//! with another check (see ADR 0001); in the simulator every operation is atomic.

use xlrx_proto::{Kind, Name};
use xlrx_sync::{DOWNLOAD_TEMP_PREFIX, Expected, LocalId, LocalOp, LocalResult, RemoteOp};

use crate::fs::SimFs;

/// Is the file still at the expected location and in the expected state?
fn file_as_expected(
    fs: &mut SimFs,
    local: LocalId,
    parent: LocalId,
    name: &Name,
    expect: &Expected,
    clock: i64,
) -> bool {
    let Some(i) = fs.inodes.get(&local.0) else {
        return false;
    };
    if i.parent != parent.0 || i.name != *name || i.kind != Kind::File {
        return false;
    }
    fs.fingerprint(local.0) == Some(expect.fp)
        && fs.hashed_content(local.0, clock) == Some(expect.content)
}

/// Executes a local operation. If a precondition is violated, everything stays unchanged.
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
            parent,
            name,
            expect,
            content,
            ..
        } => {
            if !file_as_expected(fs, *local, *parent, name, expect, clock) {
                return LocalResult::Precondition;
            }
            // As in the real client: write a temp file and atomically move it over the original.
            let tmp = Name::new(&format!("{DOWNLOAD_TEMP_PREFIX}{clock}")).expect("gültiger Name");
            let Ok(t) = fs.create(parent.0, &tmp, Kind::File, Some(*content), clock) else {
                return LocalResult::Error;
            };
            match fs.rename(t, parent.0, name, true, clock) {
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
        LocalOp::DeleteFile {
            local,
            parent,
            name,
            expect,
            ..
        } => {
            if !file_as_expected(fs, *local, *parent, name, expect, clock) {
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
        LocalOp::DeleteDir {
            local,
            parent,
            name,
            ..
        } => {
            if fs
                .inodes
                .get(&local.0)
                .is_none_or(|i| i.parent != parent.0 || i.name != *name)
            {
                return LocalResult::Precondition;
            }
            match fs.rmdir(local.0) {
                Ok(()) => LocalResult::Done {
                    id: *local,
                    fp: None,
                },
                Err(_) => LocalResult::Precondition,
            }
        }
    }
}

/// Content is uploaded from the local file, which must still exactly match the hashed state.
/// (The real client hashes while uploading and aborts on a mismatch.)
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
