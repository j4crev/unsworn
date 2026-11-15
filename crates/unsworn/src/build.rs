//! `unsworn build --target cloudflare`: bundles a project into a Worker.
//!
//! The Worker runs the core as WebAssembly from `@unswornjs/sdk/cloudflare`
//! and imports the handler modules directly. The build writes, by default
//! to `dist/cloudflare`:
//!
//! - `worker.js`: the entry, with the project files the core loads at
//!   startup and an import per handler module;
//! - `wrangler.json`: the user's `wrangler.jsonc` (if any) plus what the
//!   Worker needs: the entry, `nodejs_compat`, the rate-limit Durable
//!   Object and the static assets;
//! - `assets/`: each `static` route's directory, at its project path.
//!
//! It also points `.wrangler/deploy/config.json` at the generated config,
//! so `wrangler dev` and `wrangler deploy` in the project root use it.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};
use unsworn_core::policy::HandlerRef;
use unsworn_core::project::{Action, Host};
use unsworn_core::{Project, compile, targets};

use crate::load::{self, FsHost, SKIP_DIRS};

/// Used when neither the user's wrangler config nor `--compatibility-date`
/// sets one: a date the Worker is tested on.
pub const COMPATIBILITY_DATE: &str = "2026-10-01";

/// Marks a directory `unsworn build` wrote, so a rebuild may replace it.
const MARKER: &str = ".unsworn-build";

/// The Durable Object class and binding for rate limits. Always declared,
/// so adding or removing `rate_limit` never needs a migration.
const LIMITER_CLASS: &str = "UnswornRateLimiter";
const LIMITER_BINDING: &str = "UNSWORN_RATE_LIMITER";
const LIMITER_MIGRATION: &str = "unsworn-rate-limiter-v1";

pub struct BuildOptions {
    pub root: PathBuf,
    pub config: String,
    pub target: String,
    pub out: Option<PathBuf>,
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
    /// on. The Worker reads the real value from its variables and secrets;
    /// the bundle keeps `{env.NAME}` as written.
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

pub fn build(opts: BuildOptions) -> Result<(), String> {
    if opts.target != "cloudflare" {
        return Err(format!(
            "unsworn build doesn't build for \"{}\"; the only target so far is cloudflare (run the server with unsworn serve)",
            opts.target
        ));
    }
    let root = &opts.root;
    let recorder = Recorder {
        fs: FsHost { root: root.clone() },
        files: RefCell::default(),
        missing_env: RefCell::default(),
    };
    let compiled = compile(&recorder, &opts.config);
    let missing_env = recorder.missing_env.take();
    for name in &missing_env {
        eprintln!(
            "! {{env.{name}}} isn't set here; the Worker reads it at runtime, so set it with `wrangler secret put {name}` or as a var"
        );
    }
    let mut errors = load::print_diagnostics(&compiled.diagnostics);
    let Some(project) = compiled.project else {
        return Err(format!("{errors} error(s)"));
    };
    errors += load::print_diagnostics(&targets::check(&project, "cloudflare"));
    if errors > 0 {
        return Err(format!("{errors} error(s)"));
    }
    check_sdk(root)?;

    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| root.join("dist/cloudflare"));
    let out = if out.is_absolute() {
        out
    } else {
        root.join(out)
    };
    prepare_out(&out)?;
    let out = std::fs::canonicalize(&out).map_err(|e| format!("{}: {e}", out.display()))?;

    let modules = handler_modules(&project);
    let files = recorder.files.take();
    let entry = worker_js(&opts.config, &files, &modules, &out, root)?;
    write(&out.join("worker.js"), &entry)?;

    let statics = static_dirs(&project);
    let mut assets = 0;
    for dir in &statics {
        assets += copy_dir(&root.join(dir), &out.join("assets").join(dir), &out)?;
    }

    let user = read_user_config(root)?;
    let name = project_name(root);
    let config = wrangler_config(user, &name, !statics.is_empty())?;
    write(
        &out.join("wrangler.json"),
        &(serde_json::to_string_pretty(&config).unwrap_or_default() + "\n"),
    )?;
    write(&out.join(MARKER), "")?;

    // Wrangler's redirected config: `wrangler dev` and `wrangler deploy` in
    // the project root use the generated config.
    let redirect = root.join(".wrangler/deploy/config.json");
    let rel = relative(&root.join(".wrangler/deploy"), &out.join("wrangler.json"));
    write(&redirect, &format!("{{\"configPath\":{}}}\n", json!(rel)))?;

    let shown = out.strip_prefix(root).unwrap_or(&out).display().to_string();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    println!(
        "✓ built {shown}: {}, {}, {}",
        plural(project.routes.len(), "route"),
        plural(modules.len(), "handler module"),
        plural(assets, "asset file"),
    );
    println!("  try it:  npx wrangler dev");
    println!("  deploy:  npx wrangler deploy");
    if !missing_env.is_empty() {
        let names: Vec<&str> = missing_env.iter().map(String::as_str).collect();
        println!("  set before deploying: {}", names.join(", "));
    }
    Ok(())
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

/// The Worker imports the core from `@unswornjs/sdk`, whose wasm must load
/// the same config this binary checked.
fn check_sdk(root: &Path) -> Result<(), String> {
    let install = format!("npm install @unswornjs/sdk@{}", unsworn_core::VERSION);
    let found = root
        .ancestors()
        .map(|d| d.join("node_modules/@unswornjs/sdk/package.json"))
        .find(|p| p.exists());
    let Some(path) = found else {
        return Err(format!(
            "the Worker needs @unswornjs/sdk, which isn't installed; run `{install}`"
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
    Ok(())
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

fn worker_js(
    config: &str,
    files: &BTreeMap<String, Option<String>>,
    modules: &[String],
    out: &Path,
    root: &Path,
) -> Result<String, String> {
    let mut imports = String::new();
    let mut map = String::new();
    for (i, m) in modules.iter().enumerate() {
        let path = Path::new(m);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let spec = relative(out, &full);
        imports.push_str(&format!("import * as m{i} from {};\n", json!(spec)));
        map.push_str(&format!("    {}: m{i},\n", json!(m)));
    }
    let files = serde_json::to_string_pretty(files).map_err(|e| e.to_string())?;
    let files = files.replace('\n', "\n  ");
    Ok(format!(
        "// Generated by `unsworn build --target cloudflare` (unsworn {version}).
// Don't edit: rebuild instead.

import {{ createWorker, UnswornRateLimiter }} from \"@unswornjs/sdk/cloudflare\";
{imports}
export default createWorker({{
  config: {config},
  files: {files},
  modules: {{
{map}  }},
}});

export {{ UnswornRateLimiter }};
",
        version = unsworn_core::VERSION,
        config = json!(normalize(config)),
    ))
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

/// A Worker name from the project directory: lowercase letters, digits
/// and dashes.
fn project_name(root: &Path) -> String {
    let raw = root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut name: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    name = name.trim_matches('-').to_string();
    name.truncate(63);
    if name.is_empty() {
        "unsworn".into()
    } else {
        name
    }
}

/// The user's own `wrangler.jsonc` or `wrangler.json`, for bindings,
/// routes, vars and the like.
fn read_user_config(root: &Path) -> Result<Option<Value>, String> {
    for name in ["wrangler.jsonc", "wrangler.json"] {
        let path = root.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let value =
                serde_json::from_str(&strip_jsonc(&text)).map_err(|e| format!("{name}: {e}"))?;
            return Ok(Some(value));
        }
    }
    if root.join("wrangler.toml").exists() {
        return Err(
            "unsworn build reads wrangler.jsonc or wrangler.json, not wrangler.toml; convert it (Cloudflare recommends JSON for new projects)".into(),
        );
    }
    Ok(None)
}

/// JSON with comments and trailing commas, as wrangler accepts, made
/// plain JSON.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    // Trailing commas: a comma followed only by whitespace before } or ].
    let mut cleaned = String::with_capacity(out.len());
    let bytes: Vec<char> = out.chars().collect();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            cleaned.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                cleaned.push(bytes[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            cleaned.push(c);
        } else if c == ',' {
            let next = bytes[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                cleaned.push(c);
            }
        } else {
            cleaned.push(c);
        }
        i += 1;
    }
    cleaned
}

/// The generated wrangler config: the user's, plus what the Worker needs.
fn wrangler_config(user: Option<Value>, name: &str, assets: bool) -> Result<Value, String> {
    let mut c = match user {
        None => Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => return Err("wrangler.jsonc must hold an object".into()),
    };
    c.entry("name").or_insert_with(|| json!(name));
    c.insert("main".into(), json!("worker.js"));
    c.entry("compatibility_date")
        .or_insert_with(|| json!(COMPATIBILITY_DATE));

    let flags = c.entry("compatibility_flags").or_insert_with(|| json!([]));
    let Value::Array(list) = flags else {
        return Err("compatibility_flags in wrangler.jsonc must be a list".into());
    };
    if !list
        .iter()
        .any(|f| f.as_str().is_some_and(|f| f.starts_with("nodejs_compat")))
    {
        list.push(json!("nodejs_compat"));
    }

    if assets {
        let entry = c.entry("assets").or_insert_with(|| json!({}));
        let Value::Object(a) = entry else {
            return Err("assets in wrangler.jsonc must be an object".into());
        };
        // Every request reaches the Worker, so routes' policies apply to
        // files too, and the core picks the file.
        a.insert("directory".into(), json!("assets"));
        a.insert("binding".into(), json!("ASSETS"));
        a.insert("run_worker_first".into(), json!(true));
        a.insert("html_handling".into(), json!("none"));
        a.insert("not_found_handling".into(), json!("none"));
    } else {
        c.remove("assets");
    }

    let objects = c
        .entry("durable_objects")
        .or_insert_with(|| json!({ "bindings": [] }));
    let bindings = objects
        .as_object_mut()
        .map(|o| o.entry("bindings").or_insert_with(|| json!([])));
    let Some(Value::Array(bindings)) = bindings else {
        return Err("durable_objects.bindings in wrangler.jsonc must be a list".into());
    };
    if !bindings.iter().any(|b| b["name"] == LIMITER_BINDING) {
        bindings.push(json!({ "name": LIMITER_BINDING, "class_name": LIMITER_CLASS }));
    }

    let migrations = c.entry("migrations").or_insert_with(|| json!([]));
    let Value::Array(migrations) = migrations else {
        return Err("migrations in wrangler.jsonc must be a list".into());
    };
    let declared = migrations.iter().any(|m| {
        ["new_sqlite_classes", "new_classes"].iter().any(|k| {
            m[*k]
                .as_array()
                .is_some_and(|l| l.iter().any(|c| c == LIMITER_CLASS))
        })
    });
    if !declared {
        migrations.push(json!({ "tag": LIMITER_MIGRATION, "new_sqlite_classes": [LIMITER_CLASS] }));
    }
    Ok(Value::Object(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_jsonc() {
        let text = r#"{
  // a comment
  "name": "a // not a comment", /* block */
  "list": [1, 2,],
  "s": "quote \" and , ]",
}"#;
        let v: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(v["name"], "a // not a comment");
        assert_eq!(v["list"], json!([1, 2]));
        assert_eq!(v["s"], "quote \" and , ]");
    }

    #[test]
    fn merges_the_user_config() {
        let user = json!({
            "name": "mine",
            "compatibility_flags": ["nodejs_compat_v2"],
            "kv_namespaces": [{ "binding": "KV", "id": "x" }],
            "migrations": [{ "tag": "v1", "new_sqlite_classes": ["Mine"] }],
        });
        let c = wrangler_config(Some(user), "dir", true).unwrap();
        assert_eq!(c["name"], "mine");
        assert_eq!(c["main"], "worker.js");
        assert_eq!(c["compatibility_date"], COMPATIBILITY_DATE);
        assert_eq!(c["compatibility_flags"], json!(["nodejs_compat_v2"]));
        assert_eq!(c["kv_namespaces"][0]["binding"], "KV");
        assert_eq!(c["assets"]["run_worker_first"], true);
        assert_eq!(c["durable_objects"]["bindings"][0]["name"], LIMITER_BINDING);
        assert_eq!(c["migrations"][1]["tag"], LIMITER_MIGRATION);
        // Building again over the result changes nothing.
        let again = wrangler_config(Some(c.clone()), "dir", true).unwrap();
        assert_eq!(again, c);

        let bare = wrangler_config(None, "dir", false).unwrap();
        assert_eq!(bare["name"], "dir");
        assert_eq!(bare["compatibility_flags"], json!(["nodejs_compat"]));
        assert!(bare.get("assets").is_none());
    }

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

    #[test]
    fn names_from_directories() {
        assert_eq!(project_name(Path::new("/x/My App_2")), "my-app-2");
        assert_eq!(project_name(Path::new("/")), "unsworn");
    }
}
