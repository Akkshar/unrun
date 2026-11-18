use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub const DIR_NAME: &str = ".unrun";

const DEFAULT_IGNORE: &str = "\
# Folders and files unrun does not snapshot, one per line.
# A bare name matches at any depth; a path with / matches from the workspace root.
node_modules
target
.venv
__pycache__
";

/// A tracked folder: the directory that holds `.unrun/`.
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    /// Walk up from `start` to the nearest folder containing `.unrun/`.
    pub fn find(start: &Path) -> Option<Self> {
        start
            .ancestors()
            .find(|p| p.join(DIR_NAME).is_dir())
            .map(|p| Workspace { root: p.to_path_buf() })
    }

    pub fn init(root: &Path) -> Result<Self> {
        let ws = Workspace { root: root.to_path_buf() };
        if ws.dir().is_dir() {
            bail!("already initialised: {}", ws.dir().display());
        }
        fs::create_dir_all(ws.objects_dir())?;
        fs::create_dir_all(ws.journal_dir())?;
        fs::write(ws.ignore_path(), DEFAULT_IGNORE)?;
        Ok(ws)
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(DIR_NAME)
    }
    pub fn objects_dir(&self) -> PathBuf {
        self.dir().join("objects")
    }
    pub fn journal_dir(&self) -> PathBuf {
        self.dir().join("journal")
    }
    pub fn index_path(&self) -> PathBuf {
        self.dir().join("index.json")
    }
    pub fn pending_path(&self) -> PathBuf {
        self.dir().join("pending.json")
    }
    pub fn ignore_path(&self) -> PathBuf {
        self.dir().join("ignore")
    }
    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.objects_dir().join(&hash[..2]).join(hash)
    }
    /// Absolute path for a workspace-relative path (always `/`-separated).
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Write via a temp file and rename, so a crash never leaves a half-written file.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value)?)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}
