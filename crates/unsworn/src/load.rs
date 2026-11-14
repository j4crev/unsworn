//! Loading a project from disk.

use std::path::{Path, PathBuf};

use unsworn_core::{Compiled, Diagnostic, Host, compile};

/// Reads project files from a root directory.
pub struct FsHost {
    pub root: PathBuf,
}

impl FsHost {
    fn path(&self, rel: &str) -> PathBuf {
        let rel = Path::new(rel);
        if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            self.root.join(rel)
        }
    }
}

const SKIP_DIRS: &[&str] = &["node_modules", ".git", "target", "__pycache__", ".venv"];

impl Host for FsHost {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(path)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }

    fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        let base = self.path(dir);
        if !base.is_dir() {
            return Err("not a directory".into());
        }
        let mut out = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(d) = stack.pop() {
            let entries = std::fs::read_dir(&d).map_err(|e| e.to_string())?;
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    if !SKIP_DIRS.contains(&name.as_str()) {
                        stack.push(path);
                    }
                } else if let Ok(rel) = path.strip_prefix(&base) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

pub const CONFIG_NAMES: &[&str] = &[
    "unsworn.toml",
    "unsworn.yaml",
    "unsworn.yml",
    "unsworn.json",
];

/// Finds the config file in `root`, unless one was given.
pub fn find_config(root: &Path, explicit: Option<&str>) -> String {
    if let Some(c) = explicit {
        return c.to_string();
    }
    CONFIG_NAMES
        .iter()
        .find(|n| root.join(n).exists())
        .unwrap_or(&CONFIG_NAMES[0])
        .to_string()
}

pub fn load(root: &Path, config: &str) -> Compiled {
    let host = FsHost {
        root: root.to_path_buf(),
    };
    compile(&host, config)
}

/// Prints diagnostics to stderr. Returns the number of errors.
pub fn print_diagnostics(diags: &[Diagnostic]) -> usize {
    for d in diags {
        eprintln!("{d}");
    }
    diags.iter().filter(|d| d.is_error()).count()
}
