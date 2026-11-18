use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::snapshot::{Change, FileEntry, Kind, Manifest, hash_file};
use crate::workspace::{Workspace, read_json, write_json};

/// One recorded command and everything it changed.
#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub command: String,
    pub cwd: String,
    pub started_ns: i64,
    pub finished_ns: i64,
    pub exit_code: Option<i32>,
    pub changes: Vec<Change>,
    pub dirs_created: Vec<String>,
    pub dirs_deleted: Vec<String>,
    #[serde(default)]
    pub undone: bool,
}

/// A command that has started but not finished: the snapshot taken before it.
#[derive(Serialize, Deserialize)]
pub struct Pending {
    pub command: String,
    pub cwd: String,
    pub started_ns: i64,
    pub manifest: Manifest,
}

impl Entry {
    pub fn summary(&self) -> String {
        let count = |k: Kind| self.changes.iter().filter(|c| c.kind == k).count();
        let mut parts = Vec::new();
        for (kind, label) in
            [(Kind::Deleted, "deleted"), (Kind::Modified, "modified"), (Kind::Created, "created")]
        {
            if count(kind) > 0 {
                parts.push(format!("{} {label}", count(kind)));
            }
        }
        if parts.is_empty() {
            let dirs = self.dirs_created.len() + self.dirs_deleted.len();
            parts.push(format!("{dirs} folder change{}", if dirs == 1 { "" } else { "s" }));
        }
        parts.join(", ")
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.dirs_created.is_empty() && self.dirs_deleted.is_empty()
    }
}

fn entry_path(ws: &Workspace, id: u64) -> PathBuf {
    ws.journal_dir().join(format!("{id:08}.json"))
}

pub fn list_ids(ws: &Workspace) -> Result<Vec<u64>> {
    let mut ids: Vec<u64> = fs::read_dir(ws.journal_dir())?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.path().file_stem()?.to_str()?.parse().ok())
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

pub fn load(ws: &Workspace, id: u64) -> Result<Entry> {
    read_json(&entry_path(ws, id)).with_context(|| format!("no journal entry #{id}"))
}

pub fn save(ws: &Workspace, entry: &Entry) -> Result<()> {
    write_json(&entry_path(ws, entry.id), entry)
}

pub fn next_id(ws: &Workspace) -> Result<u64> {
    Ok(list_ids(ws)?.last().copied().unwrap_or(0) + 1)
}

/// The newest entry whose `undone` flag equals `undone`.
pub fn latest(ws: &Workspace, undone: bool) -> Result<Option<Entry>> {
    for id in list_ids(ws)?.into_iter().rev() {
        let entry = load(ws, id)?;
        if entry.undone == undone {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Does the file on disk match the recorded state (`None` = should not exist)?
fn on_disk_matches(ws: &Workspace, path: &str, want: &Option<FileEntry>) -> bool {
    let abs = ws.abs(path);
    match (fs::symlink_metadata(&abs), want) {
        (Err(_), None) => true,
        (Ok(meta), Some(entry)) if meta.is_file() => match &entry.hash {
            Some(hash) => hash_file(&abs).is_ok_and(|h| &h == hash),
            None => meta.len() == entry.size,
        },
        _ => false,
    }
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: Option<u32>) {}

pub struct Applied {
    pub restored: usize,
    pub removed: usize,
    pub unrecoverable: Vec<String>,
}

/// Move the workspace between an entry's before and after states.
/// `undo == true` goes after -> before; `false` (redo) goes before -> after.
///
/// Refuses if any touched file no longer matches the state we are moving away
/// from, since that means something else changed it and would be overwritten.
pub fn apply(ws: &Workspace, entry: &Entry, undo: bool, force: bool) -> Result<Applied> {
    let ends = |c: &Change| {
        if undo { (c.after.clone(), c.before.clone()) } else { (c.before.clone(), c.after.clone()) }
    };

    if !force {
        let conflicts: Vec<&str> = entry
            .changes
            .iter()
            .filter(|c| !on_disk_matches(ws, &c.path, &ends(c).0))
            .map(|c| c.path.as_str())
            .collect();
        if !conflicts.is_empty() {
            bail!(
                "these files changed again after #{} and would be overwritten:\n  {}\nre-run with --force to overwrite them",
                entry.id,
                conflicts.join("\n  ")
            );
        }
    }

    let (make_dirs, remove_dirs) = if undo {
        (&entry.dirs_deleted, &entry.dirs_created)
    } else {
        (&entry.dirs_created, &entry.dirs_deleted)
    };
    for dir in make_dirs {
        fs::create_dir_all(ws.abs(dir))?;
    }

    let mut applied = Applied { restored: 0, removed: 0, unrecoverable: Vec::new() };
    for change in &entry.changes {
        let abs = ws.abs(&change.path);
        match ends(change).1 {
            None => {
                if abs.exists() {
                    fs::remove_file(&abs).with_context(|| format!("removing {}", change.path))?;
                }
                applied.removed += 1;
            }
            Some(target) => {
                let Some(hash) = &target.hash else {
                    applied.unrecoverable.push(change.path.clone());
                    continue;
                };
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(ws.object_path(hash), &abs)
                    .with_context(|| format!("restoring {}", change.path))?;
                set_mode(&abs, target.mode);
                applied.restored += 1;
            }
        }
    }

    // Deepest first, and only if empty: never delete a folder that holds
    // something this entry did not account for.
    let mut remove_dirs = remove_dirs.clone();
    remove_dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for dir in &remove_dirs {
        let _ = fs::remove_dir(ws.abs(dir));
    }
    Ok(applied)
}
