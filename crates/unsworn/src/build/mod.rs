//! `unsworn build --target <name>`: bundles a project for a platform that
//! runs the core as WebAssembly, with JS/TS handlers called in the same
//! isolate or function. The `image` target is the odd one out: it writes
//! a Dockerfile for the server and its workers (see `image.rs`).
//!
//! What every target shares is here: compiling the project while noting the
//! files it reads, checking it against the target, and writing the bundle
//! the SDK's runtime loads. Each target's module writes its entry and the
//! platform's own config.

mod cloudflare;
mod functions;
mod image;
mod lambda;
mod netlify;
mod vercel;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};
use unsworn_core::policy::HandlerRef;
use unsworn_core::project::{Action, Host};
use unsworn_core::{Project, compile, targets};

use crate::load::{self, FsHost, SKIP_DIRS};

/// Marks a directory `unsworn build` wrote, so a rebuild may replace it.
const MARKER: &str = ".unsworn-build";

pub struct BuildOptions {
    pub root: PathBuf,
    pub config: String,
    pub target: String,
    pub out: Option<PathBuf>,
    /// For the `image` target: the image to copy the `unsworn` binary from.
    pub from: Option<String>,
}

fn normalize(path: &str) -> String {
    let path = path.strip_prefix("./").unwrap_or(path);
    path.trim_end_matches('/').to_string()
}

/// Loads from disk, noting every file the compiler touches, so the bundle
/// carries exactly those, and every `{env.NAME}` that isn't set here.
struct Recorder {
    fs: FsHost,
    files: RefCell<BTreeMap<String, Option<String>>>,
    missing_env: RefCell<BTreeSet<String>>,
}

impl Host for Recorder {
    fn read(&self, path: &str) -> Option<String> {
        let text = self.fs.read(path)?;
        self.files
            .borrow_mut()
            .insert(normalize(path), Some(text.clone()));
        Some(text)
    }

    fn exists(&self, path: &str) -> bool {
        let found = self.fs.exists(path);
        if found {
            self.files
                .borrow_mut()
                .entry(normalize(path))
                .or_insert(None);
        }
        found
    }

    fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        let list = self.fs.list(dir)?;
        let mut files = self.files.borrow_mut();
        let dir = normalize(dir);
        for f in &list {
            let path = if dir.is_empty() || dir == "." {
                f.clone()
            } else {
                format!("{dir}/{f}")
            };
            files.entry(path).or_insert(None);
        }
        Ok(list)
    }

    /// A variable that isn't set here reads as a placeholder URL, which
    /// passes the checks for URLs and secrets alike, so the build can go
    /// on. The platform supplies the real value at runtime; the bundle
    /// keeps `{env.NAME}` as written.
    fn env(&self, key: &str) -> Option<String> {
        match self.fs.env(key) {
            Some(v) => Some(v),
            None => {
                self.missing_env.borrow_mut().insert(key.to_string());
                let host: String = key
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() {
                            c.to_ascii_lowercase()
                        } else {
                            '-'
                        }
                    })
                    .collect();
                Some(format!("https://unset-{host}.invalid"))
            }
        }
    }
}

/// A project compiled and checked for a target, with its output
/// directory emptied and ready.
struct Prepared {
    target: &'static str,
    root: PathBuf,
    /// The installed `@unswornjs/sdk`.
    sdk: PathBuf,
    /// The config file's path, as the bundle names it.
    config: String,
    /// The output directory, absolute.
    out: PathBuf,
    project: Project,
    /// Every project file the core reads when it loads: its text, or `None`
    /// for files that only need to exist.
    files: BTreeMap<String, Option<String>>,
    /// Handler modules, project-relative.
    modules: Vec<String>,
    /// The directories of `static` routes, project-relative.
    statics: Vec<String>,
    /// `{env.NAME}` variables that aren't set where the build runs.
    missing_env: BTreeSet<String>,
}

/// A target `unsworn build` can write.
struct Target {
    name: &'static str,
    /// Where the output goes without `--out`, relative to the project.
    out: &'static str,
    /// How to set a variable on the platform, with `{name}` for it.
    env_hint: &'static str,
    build: fn(Prepared) -> Result<(), String>,
}

const TARGETS: &[Target] = &[
    Target {
        name: "cloudflare",
        out: "dist/cloudflare",
        env_hint: "set it with `wrangler secret put {name}` or as a var",
        build: cloudflare::build,
    },
    Target {
        name: "lambda",
        out: "dist/lambda",
        env_hint: "set it in the function's environment variables",
        build: lambda::build,
    },
    Target {
        name: "netlify",
        // Where Netlify's Frameworks API looks.
        out: ".netlify/v1",
        env_hint: "set it with `netlify env:set {name} <value>`",
        build: netlify::build,
    },
    Target {
        name: "vercel",
        // Where `vercel deploy --prebuilt` looks.
        out: ".vercel/output",
        env_hint: "set it with `vercel env add {name}`",
        build: vercel::build,
    },
];

pub fn build(opts: BuildOptions) -> Result<(), String> {
    if opts.target == "image" {
        return image::build(&opts, opts.from.as_deref());
    }
    let Some(target) = TARGETS.iter().find(|t| t.name == opts.target) else {
        let names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
        return Err(format!(
            "unsworn build doesn't build for \"{}\"; its targets are {}, and image for a container",
            opts.target,
            names.join(", ")
        ));
    };
    (target.build)(prepare(&opts, target)?)
}

/// Compiles the project, checks it for the target and empties the output
/// directory.
fn prepare(opts: &BuildOptions, target: &Target) -> Result<Prepared, String> {
    let root = &opts.root;
    let env_hint = target.env_hint;
    let recorder = Recorder {
        fs: FsHost { root: root.clone() },
        files: RefCell::default(),
        missing_env: RefCell::default(),
    };
    let compiled = compile(&recorder, &opts.config);
    let missing_env = recorder.missing_env.take();
    for name in &missing_env {
        eprintln!(
            "! {{env.{name}}} isn't set here; it's read at runtime, so {}",
            env_hint.replace("{name}", name)
        );
    }
    let mut errors = load::print_diagnostics(&compiled.diagnostics);
    let Some(project) = compiled.project else {
        return Err(format!("{errors} error(s)"));
    };
    errors += load::print_diagnostics(&targets::check(&project, target.name));
    if errors > 0 {
        return Err(format!("{errors} error(s)"));
    }
    let sdk = check_sdk(root)?;

    let out = opts.out.clone().unwrap_or_else(|| root.join(target.out));
    let out = if out.is_absolute() {
        out
    } else {
        root.join(out)
    };
    prepare_out(&out)?;
    let out = std::fs::canonicalize(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    write(&out.join(MARKER), "")?;

    Ok(Prepared {
        target: target.name,
        sdk,
        root: root.clone(),
        config: normalize(&opts.config),
        modules: handler_modules(&project),
        statics: static_dirs(&project),
        files: recorder.files.take(),
        project,
        out,
        missing_env,
    })
}

impl Prepared {
    /// Copies every `static` route's directory below `to`, at its project
    /// path. Returns the number of files.
    fn copy_statics(&self, to: &Path) -> Result<usize, String> {
        let mut n = 0;
        for dir in &self.statics {
            n += copy_dir(&self.root.join(dir), &to.join(dir), &self.out)?;
        }
        Ok(n)
    }

    /// The bundle for an entry in `entry_dir`: an `import` line per handler
    /// module, and the object literal the SDK's runtime takes (`config`,
    /// `files`, `modules`).
    fn bundle_js(&self, entry_dir: &Path) -> Result<(String, String), String> {
        let mut imports = String::new();
        let mut map = String::new();
        for (i, m) in self.modules.iter().enumerate() {
            let path = Path::new(m);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.root.join(path)
            };
            let spec = relative(entry_dir, &full);
            imports.push_str(&format!("import * as m{i} from {};\n", json!(spec)));
            map.push_str(&format!("    {}: m{i},\n", json!(m)));
        }
        let files = serde_json::to_string_pretty(&self.files).map_err(|e| e.to_string())?;
        let files = files.replace('\n', "\n  ");
        let literal = format!(
            "{{\n  config: {},\n  files: {files},\n  modules: {{\n{map}  }},\n}}",
            json!(self.config)
        );
        Ok((imports, literal))
    }

    /// The closing lines every build prints.
    fn report(&self, assets: usize, next: &[(&str, &str)]) {
        let shown = self
            .out
            .strip_prefix(&self.root)
            .unwrap_or(&self.out)
            .display()
            .to_string();
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        println!(
            "✓ built {shown}: {}, {}, {}",
            plural(self.project.routes.len(), "route"),
            plural(self.modules.len(), "handler module"),
            plural(assets, "asset file"),
        );
        for (label, command) in next {
            println!("  {label:<8} {command}");
        }
        if !self.missing_env.is_empty() {
            let names: Vec<&str> = self.missing_env.iter().map(String::as_str).collect();
            println!("  set before deploying: {}", names.join(", "));
        }
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Empties the output directory, if it's one a build wrote.
fn prepare_out(out: &Path) -> Result<(), String> {
    if out.exists() {
        let ours = out.join(MARKER).exists();
        let empty = std::fs::read_dir(out)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !ours && !empty {
            return Err(format!(
                "{} exists and wasn't written by unsworn build; remove it or pass --out",
                out.display()
            ));
        }
        std::fs::remove_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))
}

/// The entry imports the core from `@unswornjs/sdk`, whose wasm must load
/// the same config this binary checked. Returns the SDK's directory.
fn check_sdk(root: &Path) -> Result<PathBuf, String> {
    let install = format!("npm install @unswornjs/sdk@{}", unsworn_core::VERSION);
    let found = root
        .ancestors()
        .map(|d| d.join("node_modules/@unswornjs/sdk/package.json"))
        .find(|p| p.exists());
    let Some(path) = found else {
        return Err(format!(
            "the build needs @unswornjs/sdk, which isn't installed; run `{install}`"
        ));
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let version = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["version"].as_str().map(String::from))
        .unwrap_or_default();
    if version != unsworn_core::VERSION {
        return Err(format!(
            "@unswornjs/sdk is {version}, but this unsworn is {}; run `{install}`",
            unsworn_core::VERSION
        ));
    }
    Ok(path.parent().unwrap_or(root).to_path_buf())
}

/// Every module a handler reference names, sorted.
fn handler_modules(project: &Project) -> Vec<String> {
    let set: BTreeSet<String> = project
        .handlers_by_runtime()
        .values()
        .flatten()
        .map(|raw| normalize(HandlerRef::split(raw).0))
        .collect();
    set.into_iter().collect()
}

fn static_dirs(project: &Project) -> Vec<String> {
    let set: BTreeSet<String> = project
        .routes
        .iter()
        .filter_map(|r| match &r.action {
            Action::Static { dir, .. } => Some(normalize(dir)),
            _ => None,
        })
        .collect();
    set.into_iter().collect()
}

/// A relative import path from `from_dir` to `to`, with `/` separators.
fn relative(from_dir: &Path, to: &Path) -> String {
    let from: Vec<Component> = from_dir.components().collect();
    let to_parts: Vec<Component> = to.components().collect();
    let common = from
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(
        to_parts[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// Copies `from` into `to`, skipping dependency and VCS directories and
/// the output itself. Returns the number of files.
fn copy_dir(from: &Path, to: &Path, out: &Path) -> Result<usize, String> {
    let mut n = 0;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str())
                || std::fs::canonicalize(&path).is_ok_and(|p| p.starts_with(out))
            {
                continue;
            }
            n += copy_dir(&path, &to.join(&name), out)?;
        } else {
            std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
            std::fs::copy(&path, to.join(&name)).map_err(|e| format!("{}: {e}", path.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let out = Path::new("/p/dist/cloudflare");
        assert_eq!(relative(out, Path::new("/p/src/a.ts")), "../../src/a.ts");
        assert_eq!(
            relative(Path::new("/p"), Path::new("/p/src/a.ts")),
            "./src/a.ts"
        );
        assert_eq!(
            relative(
                Path::new("/p/.wrangler/deploy"),
                Path::new("/p/dist/cloudflare/wrangler.json")
            ),
            "../../dist/cloudflare/wrangler.json"
        );
    }
}
