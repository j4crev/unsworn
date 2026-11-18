//! `plan` and `diff`: the project's rules as a file, and what changed in
//! them since another version of the project.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use unsworn_core::plan::Plan;
use unsworn_core::{Host, compile_symbolic};

use crate::load::{self, FsHost};

/// The plan file's name, next to the config.
pub const PLAN_FILE: &str = "unsworn.plan";

/// Compiles the project a host holds and works out its plan. Only errors
/// are printed: `unsworn check` reports the warnings.
fn plan_of(host: &dyn Host, config: &str, what: &str) -> Result<Plan, String> {
    let compiled = compile_symbolic(host, config);
    let errors: Vec<_> = compiled
        .diagnostics
        .into_iter()
        .filter(|d| d.is_error())
        .collect();
    load::print_diagnostics(&errors);
    match compiled.project {
        Some(project) => Ok(Plan::of(&project)),
        None => Err(format!("{what} has {} error(s)", errors.len())),
    }
}

fn working_plan(root: &Path, config: &str) -> Result<Plan, String> {
    let host = FsHost {
        root: root.to_path_buf(),
    };
    plan_of(&host, config, "the project")
}

pub fn plan(root: &Path, config: &str, check: bool, output: Option<&str>) -> Result<(), String> {
    let text = working_plan(root, config)?.render();
    let name = output.unwrap_or(PLAN_FILE);
    if name == "-" {
        if check {
            return Err("--check compares with a file, so it can't be used with `-o -`".into());
        }
        print!("{text}");
        return Ok(());
    }
    let path = root.join(name);
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() == Some(text.as_str()) {
        println!("✓ {name} is up to date");
        return Ok(());
    }
    if check {
        eprintln!(
            "✗ {name} {}",
            match current {
                Some(_) => "is out of date: the config's rules have changed since it was written",
                None => "doesn't exist",
            }
        );
        eprintln!("  → run `unsworn plan` and commit {name}");
        return Err(String::new());
    }
    std::fs::write(&path, &text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    println!(
        "✓ {} {name}",
        if current.is_some() {
            "updated"
        } else {
            "wrote"
        }
    );
    Ok(())
}

pub fn diff(
    root: &Path,
    config: &str,
    config_given: bool,
    base: Option<&str>,
    fail_on: Option<&str>,
) -> Result<(), String> {
    let base = base.unwrap_or("HEAD");
    let dir = Path::new(base);
    let old = if dir.is_dir() {
        let other = std::fs::canonicalize(dir).map_err(|e| format!("can't open {base}: {e}"))?;
        let config = if config_given {
            config.to_string()
        } else {
            load::find_config(&other, None)
        };
        let host = FsHost { root: other };
        plan_of(&host, &config, &format!("the project in {base}"))?
    } else {
        let host = GitHost::open(root, base)?;
        let config = if config_given {
            config.to_string()
        } else {
            host.find_config()
        };
        plan_of(&host, &config, &format!("the project at {base}"))?
    };
    let new = working_plan(root, config)?;
    let diff = unsworn_core::plan::diff(&old, &new);
    print!("{}", diff.render());
    let failed = match fail_on {
        Some("any") => !diff.is_empty(),
        Some("looser") => diff.looser() > 0,
        _ => false,
    };
    if failed {
        // The difference above says why.
        return Err(String::new());
    }
    Ok(())
}

/// A project as a git revision holds it, read without checking it out.
struct GitHost {
    root: PathBuf,
    rev: String,
    /// Every file below the project directory at that revision.
    files: BTreeSet<String>,
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("can't run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

impl GitHost {
    fn open(root: &Path, rev: &str) -> Result<GitHost, String> {
        let not_found = |detail: String| {
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            };
            format!(
                "\"{rev}\" isn't a directory, or a git revision of this project{detail}\n  → pass a branch, tag or commit, such as `unsworn diff origin/main`, or the path of another copy of the project"
            )
        };
        // `--end-of-options` keeps a revision that starts with a dash from
        // being read as one of git's flags.
        let commit = format!("{rev}^{{commit}}");
        git(
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &commit,
            ],
        )
        .map_err(not_found)?;
        // In a subdirectory of the repository, this lists only what is
        // below it, by paths relative to it.
        let listing = git(
            root,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                "-z",
                "--end-of-options",
                rev,
            ],
        )?;
        let files = listing
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        Ok(GitHost {
            root: root.to_path_buf(),
            rev: rev.to_string(),
            files,
        })
    }

    fn find_config(&self) -> String {
        load::CONFIG_NAMES
            .iter()
            .find(|n| self.files.contains(**n))
            .unwrap_or(&load::CONFIG_NAMES[0])
            .to_string()
    }
}

/// A path as the listing has it: no `./` in front, no `/` behind.
fn tidy(path: &str) -> &str {
    path.trim_start_matches("./").trim_end_matches('/')
}

impl Host for GitHost {
    fn read(&self, path: &str) -> Option<String> {
        // A file outside the project, such as a certificate, isn't in the
        // repository at any revision.
        if Path::new(path).is_absolute() {
            return std::fs::read_to_string(path).ok();
        }
        let path = tidy(path);
        if !self.files.contains(path) {
            return None;
        }
        let object = format!("{}:./{path}", self.rev);
        let bytes = git(&self.root, &["show", "--end-of-options", &object]).ok()?;
        String::from_utf8(bytes).ok()
    }

    fn exists(&self, path: &str) -> bool {
        if Path::new(path).is_absolute() {
            return Path::new(path).exists();
        }
        let path = tidy(path);
        let dir = format!("{path}/");
        path.is_empty()
            || self.files.contains(path)
            || self.files.iter().any(|f| f.starts_with(&dir))
    }

    fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        let dir = tidy(dir);
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let out: Vec<String> = self
            .files
            .iter()
            .filter_map(|f| f.strip_prefix(&prefix))
            .filter(|rel| !rel.split('/').any(|part| load::SKIP_DIRS.contains(&part)))
            .map(String::from)
            .collect();
        if out.is_empty() {
            return Err("not a directory".into());
        }
        Ok(out)
    }

    /// A plan is compiled without reading the environment.
    fn env(&self, _: &str) -> Option<String> {
        None
    }
}
