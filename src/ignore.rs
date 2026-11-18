use std::fs;

use crate::workspace::{DIR_NAME, Workspace};

/// Ignore rules loaded from `.unrun/ignore`.
pub struct Ignore {
    /// Bare names, matched against every path component.
    names: Vec<String>,
    /// Root-relative paths, matched as a prefix.
    prefixes: Vec<String>,
}

impl Ignore {
    pub fn load(ws: &Workspace) -> Self {
        let text = fs::read_to_string(ws.ignore_path()).unwrap_or_default();
        let mut ignore = Ignore { names: vec![DIR_NAME.to_string()], prefixes: Vec::new() };
        for line in text.lines() {
            let line = line.trim().replace('\\', "/");
            let line = line.trim_matches('/');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.contains('/') {
                ignore.prefixes.push(line.to_string());
            } else {
                ignore.names.push(line.to_string());
            }
        }
        ignore
    }

    /// `rel` is a `/`-separated path relative to the workspace root.
    pub fn matches(&self, rel: &str) -> bool {
        rel.split('/').any(|part| self.names.iter().any(|n| n == part))
            || self
                .prefixes
                .iter()
                .any(|p| rel == p || rel.strip_prefix(p.as_str()).is_some_and(|rest| rest.starts_with('/')))
    }
}
