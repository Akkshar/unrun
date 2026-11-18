use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::ignore::Ignore;
use crate::workspace::{Workspace, now_ns};

/// A file whose mtime is this close to the snapshot time is re-hashed next
/// time, because a second write in the same timestamp tick would be invisible.
const RACY_WINDOW_NS: i64 = 2_000_000_000;

const DEFAULT_MAX_FILE_MB: u64 = 256;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub size: u64,
    pub mtime_ns: i64,
    /// Content hash; `None` when the file was over the size limit and not stored.
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

impl FileEntry {
    pub fn same_content(&self, other: &FileEntry) -> bool {
        match (&self.hash, &other.hash) {
            (Some(a), Some(b)) => a == b && self.mode == other.mode,
            _ => self.size == other.size && self.mtime_ns == other.mtime_ns,
        }
    }
}

/// The state of the workspace at one moment.
#[derive(Serialize, Deserialize, Default)]
pub struct Manifest {
    pub taken_ns: i64,
    pub files: BTreeMap<String, FileEntry>,
    pub dirs: BTreeSet<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Created,
    Modified,
    Deleted,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Change {
    pub path: String,
    pub kind: Kind,
    pub before: Option<FileEntry>,
    pub after: Option<FileEntry>,
}

pub struct Diff {
    pub changes: Vec<Change>,
    pub dirs_created: Vec<String>,
    pub dirs_deleted: Vec<String>,
}

fn max_file_bytes() -> u64 {
    std::env::var("UNRUN_MAX_FILE_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_FILE_MB)
        * 1024
        * 1024
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> Option<u32> {
    None
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Copy `path` into the object store, hashing in the same pass so the stored
/// bytes always match the hash even if the file is changing underneath us.
fn store_file(ws: &Workspace, path: &Path, worker: usize) -> Result<String> {
    let mut src = File::open(path)?;
    let tmp = ws.objects_dir().join(format!("tmp-{}-{worker}", std::process::id()));
    let mut out = File::create(&tmp)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    drop(out);
    let hash = hasher.finalize().to_hex().to_string();
    let dest = ws.object_path(&hash);
    if dest.exists() {
        fs::remove_file(&tmp)?;
    } else {
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(&tmp, &dest)?;
    }
    Ok(hash)
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Record every file under the workspace, storing the content of any file that
/// changed since `prev`. Unchanged files reuse the hash from `prev`.
pub fn snapshot(ws: &Workspace, prev: Option<&Manifest>, ignore: &Ignore) -> Result<Manifest> {
    let mut manifest = Manifest { taken_ns: now_ns(), ..Default::default() };
    let max_bytes = max_file_bytes();
    let mut to_store: Vec<(String, PathBuf, FileEntry)> = Vec::new();

    let walker = WalkDir::new(&ws.root).follow_links(false).into_iter().filter_entry(|e| {
        rel_path(&ws.root, e.path()).is_none_or(|rel| !ignore.matches(&rel))
    });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!("unrun: skipping unreadable path: {err}");
                continue;
            }
        };
        let Some(rel) = rel_path(&ws.root, entry.path()) else { continue };
        let file_type = entry.file_type();
        if file_type.is_dir() {
            manifest.dirs.insert(rel);
            continue;
        }
        if !file_type.is_file() {
            continue; // symlinks and special files are not tracked in v0
        }
        let Ok(meta) = entry.metadata() else { continue };
        let (size, mtime) = (meta.len(), mtime_ns(&meta));
        let known = prev.and_then(|p| p.files.get(&rel).map(|e| (e, p.taken_ns)));

        if let Some((old, taken)) = known {
            let settled = mtime < taken - RACY_WINDOW_NS;
            if old.size == size && old.mtime_ns == mtime && settled && old.hash.is_some() {
                manifest.files.insert(rel, old.clone());
                continue;
            }
        }

        let entry_for = |hash| FileEntry { size, mtime_ns: mtime, hash, mode: mode_of(&meta) };
        if size > max_bytes {
            manifest.files.insert(rel, entry_for(None));
        } else {
            to_store.push((rel, entry.into_path(), entry_for(None)));
        }
    }

    // Copying into the store dominates the cost of a first snapshot, so fan it
    // out across threads; the walk above stays single-threaded.
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let next = AtomicUsize::new(0);
    let stored: Vec<Vec<(usize, Result<String>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                let (next, to_store) = (&next, &to_store);
                scope.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((_, path, _)) = to_store.get(i) else { break };
                        done.push((i, store_file(ws, path, worker)));
                    }
                    done
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("snapshot worker panicked")).collect()
    });

    for (i, result) in stored.into_iter().flatten() {
        let (rel, _, entry) = &to_store[i];
        match result {
            Ok(hash) => {
                manifest.files.insert(rel.clone(), FileEntry { hash: Some(hash), ..entry.clone() });
            }
            Err(err) => {
                eprintln!("unrun: could not snapshot {rel}: {err}");
                // Keep the last known state rather than reporting a false delete.
                if let Some(old) = prev.and_then(|p| p.files.get(rel)) {
                    manifest.files.insert(rel.clone(), old.clone());
                }
            }
        }
    }
    Ok(manifest)
}

pub fn diff(before: &Manifest, after: &Manifest) -> Diff {
    let mut changes = Vec::new();
    for (path, old) in &before.files {
        match after.files.get(path) {
            None => changes.push(Change {
                path: path.clone(),
                kind: Kind::Deleted,
                before: Some(old.clone()),
                after: None,
            }),
            Some(new) if !old.same_content(new) => changes.push(Change {
                path: path.clone(),
                kind: Kind::Modified,
                before: Some(old.clone()),
                after: Some(new.clone()),
            }),
            Some(_) => {}
        }
    }
    for (path, new) in &after.files {
        if !before.files.contains_key(path) {
            changes.push(Change {
                path: path.clone(),
                kind: Kind::Created,
                before: None,
                after: Some(new.clone()),
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Diff {
        changes,
        dirs_created: after.dirs.difference(&before.dirs).cloned().collect(),
        dirs_deleted: before.dirs.difference(&after.dirs).cloned().collect(),
    }
}
