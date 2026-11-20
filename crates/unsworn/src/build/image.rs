//! `unsworn build --target image`: a Dockerfile for the whole project.
//!
//! A project with handlers in three languages needs an image with three
//! toolchains' worth of care: the right version of each runtime, each
//! one's dependencies installed from its lockfile, compiled workers built
//! in stages that don't end up in the image, and the project owned by
//! root so handler code can't rewrite the rules it runs under. This writes
//! that Dockerfile from what the config and the project's files say.
//!
//! It writes; it doesn't run Docker. The file is meant to be read, kept
//! and changed.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use unsworn_core::project::Runtime;
use unsworn_core::{Project, compile_symbolic};

use super::BuildOptions;
use crate::load::{self, FsHost};

/// What runs a runtime's workers, or builds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tool {
    Bun,
    Node,
    Deno,
    Python,
    Go,
    Rust,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Bun => "bun",
            Tool::Node => "node",
            Tool::Deno => "deno",
            Tool::Python => "python",
            Tool::Go => "go",
            Tool::Rust => "rust",
        }
    }

    /// Workers are compiled ahead of time, in a stage of their own.
    fn compiled(self) -> bool {
        matches!(self, Tool::Go | Tool::Rust)
    }

    /// The version to use when nothing in the project or on this machine
    /// says: the newest of this major.
    fn fallback(self) -> &'static str {
        match self {
            Tool::Bun | Tool::Go | Tool::Rust => "1",
            Tool::Node => "22",
            Tool::Deno => "2",
            Tool::Python => "3",
        }
    }

    /// The command that prints the installed version.
    fn version_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Tool::Bun => ("bun", &["--version"]),
            Tool::Node => ("node", &["--version"]),
            Tool::Deno => ("deno", &["--version"]),
            Tool::Python => ("python3", &["--version"]),
            Tool::Go => ("go", &["version"]),
            Tool::Rust => ("rustc", &["--version"]),
        }
    }
}

/// The words of a shell command that could name a program: split at
/// spaces and at `&&`, `;` and `|`, without `VAR=value` settings.
fn programs(command: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut first = true;
    for word in command.split_whitespace() {
        if matches!(word, "&&" | "||" | ";" | "|") {
            first = true;
            continue;
        }
        if first && !word.contains('=') {
            out.push(word);
            first = false;
        }
    }
    out
}

/// Which tool a runtime needs, from its `command` and `build`.
fn tool(runtime: &Runtime) -> Option<Tool> {
    let named = |command: &str, names: &[&str]| {
        programs(command)
            .iter()
            .any(|p| names.contains(&p.rsplit('/').next().unwrap_or(p)))
    };
    let build = runtime.build.as_deref().unwrap_or_default();
    if named(build, &["go"]) {
        Some(Tool::Go)
    } else if named(build, &["cargo"]) {
        Some(Tool::Rust)
    } else if named(&runtime.command, &["bun", "bunx"]) {
        Some(Tool::Bun)
    } else if named(&runtime.command, &["node", "npx", "tsx"]) {
        Some(Tool::Node)
    } else if named(&runtime.command, &["deno"]) {
        Some(Tool::Deno)
    } else if named(&runtime.command, &["python", "python3", "uv"]) {
        Some(Tool::Python)
    } else {
        None
    }
}

/// The first run of digits and dots in a text: `1.27.1` from
/// `go version go1.27.1 linux/amd64`, `26.13.0` from `v26.13.0`.
fn version_in(text: &str) -> Option<String> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let version = rest[..end].trim_end_matches('.');
    (!version.is_empty()).then(|| version.to_string())
}

/// Where a version came from, for the Dockerfile's comment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pinned {
    /// A file in the project names it.
    By(String),
    /// It's what this machine has installed.
    Installed,
    /// Nothing says, so the newest of a major.
    Unknown,
}

/// What the image build looks at outside the config: project files, and
/// the tools installed here.
trait Look {
    fn read(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    fn installed(&self, tool: Tool) -> Option<String>;
}

struct Disk<'a> {
    root: &'a Path,
}

impl Look for Disk<'_> {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        self.root.join(path).exists()
    }

    fn installed(&self, tool: Tool) -> Option<String> {
        let (program, args) = tool.version_command();
        let out = Command::new(program).args(args).output().ok()?;
        out.status
            .success()
            .then(|| version_in(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    }
}

/// A line of `.tool-versions` (asdf, mise): `nodejs 26.13.0`.
fn tool_versions(look: &dyn Look, names: &[&str]) -> Option<String> {
    let text = look.read(".tool-versions")?;
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        names
            .contains(&words.next()?)
            .then(|| words.next().and_then(version_in))
            .flatten()
    })
}

/// The version of a tool to put in the image, and what said so.
fn version(look: &dyn Look, tool: Tool) -> (String, Pinned) {
    let file = |path: &str| {
        look.read(path)
            .and_then(|text| version_in(text.trim()))
            .map(|v| (v, Pinned::By(path.to_string())))
    };
    let listed = |names: &[&str]| {
        tool_versions(look, names).map(|v| (v, Pinned::By(".tool-versions".to_string())))
    };
    let found = match tool {
        Tool::Bun => file(".bun-version")
            .or_else(|| listed(&["bun"]))
            .or_else(|| {
                // `"packageManager": "bun@1.4.4"`
                let package = look.read("package.json")?;
                let at = package.find("\"bun@")?;
                version_in(&package[at..]).map(|v| (v, Pinned::By("package.json".into())))
            }),
        Tool::Node => file(".nvmrc")
            .or_else(|| file(".node-version"))
            .or_else(|| listed(&["nodejs", "node"])),
        Tool::Deno => listed(&["deno"]),
        Tool::Python => file(".python-version").or_else(|| listed(&["python"])),
        Tool::Go => listed(&["golang", "go"]).or_else(|| {
            let module = look.read("go.mod")?;
            let line = module.lines().find(|l| l.starts_with("go "))?;
            version_in(line).map(|v| (v, Pinned::By("go.mod".into())))
        }),
        Tool::Rust => listed(&["rust"]).or_else(|| {
            let text = look
                .read("rust-toolchain.toml")
                .or_else(|| look.read("rust-toolchain"))?;
            let line = text
                .lines()
                .find(|l| l.contains("channel"))
                .unwrap_or(&text);
            version_in(line).map(|v| (v, Pinned::By("rust-toolchain.toml".into())))
        }),
    };
    found
        .or_else(|| look.installed(tool).map(|v| (v, Pinned::Installed)))
        .unwrap_or_else(|| (tool.fallback().to_string(), Pinned::Unknown))
}

/// How a project's JavaScript dependencies are installed.
fn js_install(look: &dyn Look, tool: Tool) -> Option<(Vec<&'static str>, String)> {
    if !look.exists("package.json") {
        return None;
    }
    let lock = |name: &'static str| look.exists(name).then_some(name);
    Some(
        if let Some(l) = lock("bun.lock").or_else(|| lock("bun.lockb")) {
            (
                vec!["package.json", l],
                "bun install --frozen-lockfile --production".into(),
            )
        } else if let Some(l) = lock("package-lock.json") {
            (vec!["package.json", l], "npm ci --omit=dev".into())
        } else if let Some(l) = lock("pnpm-lock.yaml") {
            (
                vec!["package.json", l],
                "corepack enable && pnpm install --frozen-lockfile --prod".into(),
            )
        } else if let Some(l) = lock("yarn.lock") {
            (
                vec!["package.json", l],
                "corepack enable && yarn install --frozen-lockfile --production".into(),
            )
        } else if tool == Tool::Bun {
            (vec!["package.json"], "bun install --production".into())
        } else {
            (vec!["package.json"], "npm install --omit=dev".into())
        },
    )
}

/// How a project's Python dependencies are installed, and the files that
/// decides it.
fn python_install(look: &dyn Look) -> (Vec<&'static str>, String) {
    if look.exists("uv.lock") && look.exists("pyproject.toml") {
        (
            vec!["pyproject.toml", "uv.lock"],
            "pip install --no-cache-dir uv && uv export --frozen --no-dev --no-emit-project -o /tmp/requirements.txt && pip install --no-cache-dir -r /tmp/requirements.txt".into(),
        )
    } else if look.exists("requirements.txt") {
        (
            vec!["requirements.txt"],
            "pip install --no-cache-dir -r requirements.txt".into(),
        )
    } else {
        (Vec::new(), "pip install --no-cache-dir unsworn".into())
    }
}

/// A compiled runtime: what builds its workers and the file that comes
/// out, which is the first word of its `command`.
struct Compiled {
    runtime: String,
    tool: Tool,
    version: String,
    pinned: Pinned,
    build: String,
    artifact: String,
}

/// Everything the Dockerfile is written from.
struct Image {
    config: String,
    /// The image the `unsworn` binary is copied from.
    unsworn: String,
    /// Interpreters to put in the image, with their versions.
    interpreters: Vec<(Tool, String, Pinned)>,
    js: Option<(Tool, String, Vec<&'static str>, String)>,
    python: Option<(Vec<&'static str>, String)>,
    compiled: Vec<Compiled>,
    /// Runtimes whose toolchain couldn't be worked out, with their commands.
    unknown: Vec<(String, String)>,
    /// `{env.NAME}` variables the config reads.
    env: Vec<String>,
}

fn pinned_note(tool: Tool, pinned: &Pinned) -> String {
    match pinned {
        Pinned::By(file) => format!("the version {file} names"),
        Pinned::Installed => format!(
            "the version installed where this was written; a version file in the project (see docs/image.md) pins {}",
            tool.name()
        ),
        Pinned::Unknown => format!(
            "nothing says which {} to use, so the newest of this major",
            tool.name()
        ),
    }
}

fn plan(project: &Project, config_text: &str, look: &dyn Look, unsworn: String) -> Image {
    let used = project.handlers_by_runtime();
    let mut interpreters: Vec<(Tool, String, Pinned)> = Vec::new();
    let mut compiled = Vec::new();
    let mut unknown = Vec::new();
    for runtime in project.runtimes.values() {
        if !used.contains_key(&runtime.name) {
            continue;
        }
        let Some(tool) = tool(runtime) else {
            unknown.push((runtime.name.clone(), runtime.command.clone()));
            continue;
        };
        let (version, pinned) = version(look, tool);
        if tool.compiled() {
            compiled.push(Compiled {
                runtime: runtime.name.clone(),
                tool,
                version,
                pinned,
                build: runtime.build.clone().unwrap_or_default(),
                artifact: programs(&runtime.command)
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .trim_start_matches("./")
                    .to_string(),
            });
        } else if !interpreters.iter().any(|(t, _, _)| *t == tool) {
            interpreters.push((tool, version, pinned));
        }
    }
    interpreters.sort_by_key(|(tool, _, _)| *tool);

    let js_tool = interpreters
        .iter()
        .map(|(tool, _, _)| *tool)
        .find(|t| matches!(t, Tool::Bun | Tool::Node));
    let js = js_tool.and_then(|tool| {
        let (files, install) = js_install(look, tool)?;
        // Installed with the tool the lockfile is for.
        let with = if install.starts_with("bun") {
            Tool::Bun
        } else {
            Tool::Node
        };
        let version = interpreters
            .iter()
            .find(|(t, _, _)| *t == with)
            .map(|(_, v, _)| v.clone())
            .unwrap_or_else(|| version(look, with).0);
        Some((with, version, files, install))
    });
    let python = interpreters
        .iter()
        .any(|(t, _, _)| *t == Tool::Python)
        .then(|| python_install(look));

    let mut env = BTreeSet::new();
    let mut rest = config_text;
    while let Some(start) = rest.find("{env.") {
        let after = &rest[start + 5..];
        let Some(end) = after.find('}') else { break };
        env.insert(after[..end].to_string());
        rest = &after[end..];
    }
    Image {
        config: project.config_file.clone(),
        unsworn,
        interpreters,
        js,
        python,
        compiled,
        unknown,
        env: env.into_iter().collect(),
    }
}

fn base_image(tool: Tool, version: &str) -> String {
    match tool {
        Tool::Bun => format!("oven/bun:{version}"),
        Tool::Node => format!("node:{version}-bookworm-slim"),
        Tool::Deno => format!("denoland/deno:bin-{version}"),
        Tool::Python => format!("python:{version}-slim-bookworm"),
        Tool::Go => format!("golang:{version}-bookworm"),
        Tool::Rust => format!("rust:{version}-bookworm"),
    }
}

/// Where an image is meant to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum For {
    /// Anything that runs a container and sends it HTTP.
    Containers,
    /// AWS Lambda, which invokes a function instead. AWS's Lambda Web
    /// Adapter, an extension in the image, turns each invocation into an
    /// HTTP request to the server, so the server is the same one.
    Lambda,
}

/// The Lambda Web Adapter this was written against. It's a line in a
/// Dockerfile that's the project's to change.
const LAMBDA_ADAPTER: &str = "public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1";

fn dockerfile(image: &Image, target: For) -> String {
    let mut out = format!(
        "# syntax=docker/dockerfile:1\n\
         # Written by `unsworn build --target {}` from {}. It's yours to keep\n\
         # and change; run the command again to start over from the config.\n\
         #\n\
         # Build it from the project directory:\n\
         #   docker build -f <this file> -t <name> .\n",
        match target {
            For::Containers => "image",
            For::Lambda => "lambda",
        },
        image.config
    );

    for c in &image.compiled {
        let _ = write!(
            out,
            "\n# The {} runtime's workers, built here so the toolchain stays out of\n\
             # the image. {}: {}.\n\
             FROM {} AS build-{}\n\
             WORKDIR /app\n\
             COPY . .\n\
             RUN {}\n",
            c.runtime,
            base_image(c.tool, &c.version),
            pinned_note(c.tool, &c.pinned),
            base_image(c.tool, &c.version),
            c.runtime,
            c.build
        );
    }
    if let Some((tool, version, files, install)) = &image.js {
        let _ = write!(
            out,
            "\n# JavaScript dependencies, from {}.\n\
             FROM {} AS js-deps\n\
             WORKDIR /app\n\
             COPY {} ./\n\
             RUN {install}\n",
            files.last().copied().unwrap_or("package.json"),
            base_image(*tool, version),
            files.join(" "),
        );
        if files.len() == 1 {
            out.push_str("# There's no lockfile, so this installs whatever is newest each time.\n");
        }
    }

    // Python brings a whole directory tree, so the image starts from it.
    let python = image
        .interpreters
        .iter()
        .find(|(tool, _, _)| *tool == Tool::Python);
    out.push_str("\n# The image: Unsworn, the runtimes and the project.\n");
    match python {
        Some((tool, version, pinned)) => {
            let _ = writeln!(
                out,
                "# {}: {}.\nFROM {}",
                base_image(*tool, version),
                pinned_note(*tool, pinned),
                base_image(*tool, version)
            );
        }
        None => out.push_str(
            "FROM debian:bookworm-slim\n\
             RUN apt-get update \\\n    && apt-get install -y --no-install-recommends ca-certificates \\\n    && rm -rf /var/lib/apt/lists/*\n",
        ),
    }
    let _ = writeln!(
        out,
        "COPY --from={} /usr/local/bin/unsworn /usr/local/bin/unsworn",
        image.unsworn
    );
    for (tool, version, pinned) in &image.interpreters {
        let from = base_image(*tool, version);
        let copy = match tool {
            Tool::Bun => "/usr/local/bin/bun /usr/local/bin/bun",
            Tool::Node => "/usr/local/bin/node /usr/local/bin/node",
            Tool::Deno => "/deno /usr/local/bin/deno",
            _ => continue,
        };
        let _ = writeln!(
            out,
            "# {from}: {}.\nCOPY --from={from} {copy}",
            pinned_note(*tool, pinned)
        );
    }
    out.push_str(
        "\n# Unsworn and every handler run as `unsworn`. The project belongs to root,\n\
         # so a flaw in a handler can't rewrite the config, or another handler,\n\
         # for the next start.\n\
         RUN useradd --system --uid 10001 --user-group --create-home unsworn \\\n    && install -d -o root -g root -m 755 /app\n\
         WORKDIR /app\n",
    );
    if let Some((files, install)) = &image.python {
        if files.is_empty() {
            let _ = writeln!(
                out,
                "# No requirements.txt or uv.lock, so only the SDK is installed.\nRUN {install}"
            );
        } else {
            let _ = writeln!(out, "COPY {} ./\nRUN {install}", files.join(" "));
        }
    }
    out.push_str("COPY . .\n");
    if image.js.is_some() {
        out.push_str("COPY --from=js-deps /app/node_modules ./node_modules\n");
    }
    for c in &image.compiled {
        let _ = writeln!(
            out,
            "COPY --from=build-{} /app/{} {}",
            c.runtime, c.artifact, c.artifact
        );
    }
    for (runtime, command) in &image.unknown {
        let _ = writeln!(
            out,
            "\n# TODO: the {runtime} runtime runs `{command}`, and Unsworn doesn't know what\n\
             # that needs. Install it here, as root."
        );
    }
    if target == For::Lambda {
        let _ = write!(
            out,
            "\n# AWS's Lambda Web Adapter. Lambda starts it beside the server, and it\n\
             # sends each invocation to the server as an HTTP request. Anywhere\n\
             # else it isn't started, and this is an ordinary image.\n\
             COPY --from={LAMBDA_ADAPTER} /lambda-adapter /opt/extensions/lambda-adapter\n\
             # Where the server listens, and what answers once it does.\n\
             ENV AWS_LWA_PORT=8080\n\
             ENV AWS_LWA_READINESS_CHECK_PATH=/_unsworn/health\n\
             # Lambda's filesystem can only be written under /tmp, and it runs\n\
             # the image as a user of its own choosing.\n\
             ENV HOME=/tmp\n"
        );
    }
    out.push_str(
        "\nUSER unsworn\n\
         ENV UNSWORN_LOG=info\n\
         # Workers are built above, and handlers can't write to /app, so the\n\
         # runtimes' `build` commands are skipped at startup.\n\
         ENV UNSWORN_NO_BUILD=1\n\
         EXPOSE 8080\n\
         STOPSIGNAL SIGTERM\n\
         ENTRYPOINT [\"unsworn\"]\n\
         CMD [\"serve\", \"--listen\", \"0.0.0.0:8080\"]\n",
    );
    out
}

/// What the build's context leaves out: what the stages install or build
/// themselves, and what shouldn't be in an image at all.
fn dockerignore(out_dir: &str) -> String {
    format!(
        "# Written by `unsworn build --target image`, for the Dockerfile next to it.\n\
         .git\n\
         node_modules\n\
         target\n\
         .venv\n\
         __pycache__\n\
         .unsworn\n\
         {out_dir}\n\
         # Secrets belong in the environment the container runs in.\n\
         .env\n\
         .env.*\n"
    )
}

pub fn build(opts: &BuildOptions, unsworn_image: Option<&str>, target: For) -> Result<(), String> {
    let root = &opts.root;
    let host = FsHost { root: root.clone() };
    // The image doesn't depend on this machine's environment.
    let compiled = compile_symbolic(&host, &opts.config);
    let errors: Vec<_> = compiled
        .diagnostics
        .into_iter()
        .filter(|d| d.is_error())
        .collect();
    load::print_diagnostics(&errors);
    let Some(project) = compiled.project else {
        return Err(format!("{} error(s)", errors.len()));
    };
    let config_text = std::fs::read_to_string(root.join(&opts.config)).unwrap_or_default();
    let unsworn = unsworn_image.map_or_else(
        || format!("ghcr.io/j4crev/unsworn:v{}", unsworn_core::VERSION),
        String::from,
    );
    let image = plan(&project, &config_text, &Disk { root }, unsworn);

    let out = opts.out.clone().unwrap_or_else(|| {
        Path::new(match target {
            For::Containers => "dist/image",
            For::Lambda => "dist/lambda",
        })
        .to_path_buf()
    });
    let shown = out.display().to_string();
    let dir = if out.is_absolute() {
        out
    } else {
        root.join(out)
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let write = |name: &str, text: String| {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))
    };
    write("Dockerfile", dockerfile(&image, target))?;
    // BuildKit reads this one for a Dockerfile of that name.
    write("Dockerfile.dockerignore", dockerignore(&shown))?;

    let mut parts: Vec<String> = image
        .interpreters
        .iter()
        .map(|(tool, version, _)| format!("{} {version}", tool.name()))
        .collect();
    parts.extend(
        image
            .compiled
            .iter()
            .map(|c| format!("{} {} (built in the image)", c.tool.name(), c.version)),
    );
    println!(
        "✓ wrote {shown}/Dockerfile{}",
        if parts.is_empty() {
            String::new()
        } else {
            format!(": {}", parts.join(", "))
        }
    );
    for (tool, _, pinned) in &image.interpreters {
        if !matches!(pinned, Pinned::By(_)) {
            eprintln!("! {}: {}", tool.name(), pinned_note(*tool, pinned));
        }
    }
    for c in image
        .compiled
        .iter()
        .filter(|c| !matches!(c.pinned, Pinned::By(_)))
    {
        eprintln!("! {}: {}", c.tool.name(), pinned_note(c.tool, &c.pinned));
    }
    for (runtime, command) in &image.unknown {
        eprintln!(
            "! [runtimes.{runtime}]: Unsworn doesn't know what `{command}` needs; the Dockerfile has a TODO where to install it"
        );
    }
    for runtime in project.runtimes.values() {
        if tool(runtime).is_some_and(Tool::compiled) && !runtime.eager {
            eprintln!(
                "! [runtimes.{}]: set `eager = true`, so a worker that's missing from the image stops the container at startup instead of failing its first request",
                runtime.name
            );
        }
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "api".into());
    let env: String = image.env.iter().map(|e| format!(" -e {e}")).collect();
    match target {
        For::Containers => {
            println!("  build    docker build -f {shown}/Dockerfile -t {name} .");
            println!("  run      docker run -p 8080:8080{env} {name}");
        }
        For::Lambda => {
            // Lambda takes one image, not the index Docker writes when it
            // adds a provenance record.
            println!(
                "  build    docker build --provenance=false -f {shown}/Dockerfile -t {name} ."
            );
            println!(
                "  try it   docker run -p 8080:8080{env} {name}    (the same server, without Lambda)"
            );
            println!(
                "  deploy   push it to Amazon ECR, create a function from the image, and give it a function URL or an HTTP API"
            );
            if !image.env.is_empty() {
                println!(
                    "  set      {} in the function's environment variables",
                    image.env.join(", ")
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        files: BTreeMap<&'static str, &'static str>,
        installed: BTreeMap<&'static str, &'static str>,
    }

    impl Look for Fake {
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).map(|s| s.to_string())
        }
        fn exists(&self, path: &str) -> bool {
            self.files.contains_key(path)
        }
        fn installed(&self, tool: Tool) -> Option<String> {
            self.installed.get(tool.name()).map(|s| s.to_string())
        }
    }

    struct Mem(BTreeMap<String, String>);

    impl unsworn_core::Host for Mem {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).cloned()
        }
        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }
        fn list(&self, _: &str) -> Result<Vec<String>, String> {
            Err("not a directory".into())
        }
        fn env(&self, _: &str) -> Option<String> {
            None
        }
    }

    const CONFIG: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run node_modules/@unswornjs/sdk/src/worker.ts"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"

[runtimes.go]
extensions = [".go"]
build = "go build -o .unsworn/go-worker ./handlers"
command = ".unsworn/go-worker"
eager = true

[runtimes.rs]
extensions = [".rs"]
build = "cargo build --release"
command = "target/release/handlers"

[runtimes.cobol]
extensions = [".cob"]
command = "cobc-run worker.cob"

[runtimes.unused]
extensions = [".rb"]
command = "ruby worker.rb"

[defaults]
auth = "none"

[auth.key]
type = "api_key"
keys = "{env.API_KEYS}"

[[route]]
path = "/a"
handler = "src/a.ts"

[[route]]
path = "/b"
handler = "src/b.py"

[[route]]
path = "/c"
handler = "handlers/c.go#C"

[[route]]
path = "/d"
handler = "src/d.rs#d"

[[route]]
path = "/e"
handler = "e.cob"

[[route]]
path = "/{*rest}"
proxy = "{env.APP_URL}"
"#;

    fn image(look: &Fake) -> Image {
        let files = ["src/a.ts", "src/b.py", "handlers/c.go", "src/d.rs", "e.cob"];
        let mut all: BTreeMap<String, String> = files
            .iter()
            .map(|f| (f.to_string(), String::new()))
            .collect();
        all.insert("unsworn.toml".into(), CONFIG.into());
        let project = compile_symbolic(&Mem(all), "unsworn.toml")
            .project
            .expect("a project");
        plan(
            &project,
            CONFIG,
            look,
            "ghcr.io/j4crev/unsworn:v9.9.9".into(),
        )
    }

    #[test]
    fn works_out_each_runtimes_tool_and_version() {
        let look = Fake {
            files: BTreeMap::from([
                (".nvmrc", "v26.13.0\n"),
                (".python-version", "3.14\n"),
                ("go.mod", "module x\n\ngo 1.27.1\n"),
                ("package.json", r#"{"packageManager": "bun@1.4.4"}"#),
                ("bun.lock", ""),
                ("requirements.txt", "unsworn\n"),
            ]),
            installed: BTreeMap::from([("rust", "1.97.1")]),
        };
        let image = image(&look);
        let tools: Vec<(&str, &str, &Pinned)> = image
            .interpreters
            .iter()
            .map(|(t, v, p)| (t.name(), v.as_str(), p))
            .collect();
        assert_eq!(
            tools,
            [
                ("bun", "1.4.4", &Pinned::By("package.json".into())),
                ("python", "3.14", &Pinned::By(".python-version".into())),
            ]
        );
        let built: Vec<(&str, &str, &str, &str)> = image
            .compiled
            .iter()
            .map(|c| {
                (
                    c.runtime.as_str(),
                    c.version.as_str(),
                    c.build.as_str(),
                    c.artifact.as_str(),
                )
            })
            .collect();
        assert_eq!(
            built,
            [
                (
                    "go",
                    "1.27.1",
                    "go build -o .unsworn/go-worker ./handlers",
                    ".unsworn/go-worker"
                ),
                (
                    "rs",
                    "1.97.1",
                    "cargo build --release",
                    "target/release/handlers"
                ),
            ]
        );
        assert_eq!(image.compiled[1].pinned, Pinned::Installed);
        // A runtime nothing uses isn't in the image; one Unsworn can't
        // read is left for the user.
        assert_eq!(
            image.unknown,
            [("cobol".to_string(), "cobc-run worker.cob".to_string())]
        );
        assert_eq!(image.env, ["API_KEYS", "APP_URL"]);
    }

    #[test]
    fn writes_a_stage_for_what_is_built_and_copies_only_what_runs() {
        let look = Fake {
            files: BTreeMap::from([
                (".python-version", "3.14\n"),
                ("go.mod", "module x\n\ngo 1.27.1\n"),
                ("package.json", r#"{"packageManager": "bun@1.4.4"}"#),
                ("bun.lock", ""),
                ("requirements.txt", "unsworn\n"),
                ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.97.1\"\n"),
            ]),
            installed: BTreeMap::new(),
        };
        let text = dockerfile(&image(&look), For::Containers);
        // For Lambda it's the same image, with AWS's adapter beside the
        // server and the two things Lambda's sandbox needs.
        let lambda = dockerfile(&image(&look), For::Lambda);
        assert!(!text.contains("lambda"), "{text}");
        for wanted in [
            "# Written by `unsworn build --target lambda` from",
            "COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter\n",
            "ENV AWS_LWA_PORT=8080\nENV AWS_LWA_READINESS_CHECK_PATH=/_unsworn/health\n",
            "ENV HOME=/tmp\n\nUSER unsworn\n",
            "CMD [\"serve\", \"--listen\", \"0.0.0.0:8080\"]\n",
        ] {
            assert!(lambda.contains(wanted), "missing {wanted:?} in\n{lambda}");
        }
        // Everything else is the same, in the same order.
        let mut rest = lambda.lines().skip(2);
        for line in text.lines().skip(2) {
            assert!(
                rest.any(|l| l == line),
                "{line:?} is missing or moved in\n{lambda}"
            );
        }
        for wanted in [
            "FROM golang:1.27.1-bookworm AS build-go\nWORKDIR /app\nCOPY . .\nRUN go build -o .unsworn/go-worker ./handlers\n",
            "FROM rust:1.97.1-bookworm AS build-rs\nWORKDIR /app\nCOPY . .\nRUN cargo build --release\n",
            "FROM oven/bun:1.4.4 AS js-deps\nWORKDIR /app\nCOPY package.json bun.lock ./\nRUN bun install --frozen-lockfile --production\n",
            "FROM python:3.14-slim-bookworm\nCOPY --from=ghcr.io/j4crev/unsworn:v9.9.9 /usr/local/bin/unsworn /usr/local/bin/unsworn\n",
            "COPY --from=oven/bun:1.4.4 /usr/local/bin/bun /usr/local/bin/bun\n",
            "COPY requirements.txt ./\nRUN pip install --no-cache-dir -r requirements.txt\nCOPY . .\nCOPY --from=js-deps /app/node_modules ./node_modules\nCOPY --from=build-go /app/.unsworn/go-worker .unsworn/go-worker\nCOPY --from=build-rs /app/target/release/handlers target/release/handlers\n",
            "# TODO: the cobol runtime runs `cobc-run worker.cob`",
            "\nUSER unsworn\nENV UNSWORN_LOG=info\n",
            "ENV UNSWORN_NO_BUILD=1\nEXPOSE 8080\nSTOPSIGNAL SIGTERM\nENTRYPOINT [\"unsworn\"]\nCMD [\"serve\", \"--listen\", \"0.0.0.0:8080\"]\n",
        ] {
            assert!(text.contains(wanted), "missing {wanted:?} in\n{text}");
        }
        // The project is copied in while still root, and the user changes
        // after everything is in place.
        assert!(
            text.find("COPY . .\nCOPY --from=js-deps").unwrap()
                < text.find("USER unsworn").unwrap()
        );
        // No Go or Rust toolchain in the final stage.
        let last = &text[text.rfind("\nFROM ").unwrap()..];
        assert!(
            !last.contains("golang") && !last.contains("cargo"),
            "{last}"
        );
    }

    #[test]
    fn a_project_without_python_starts_from_debian() {
        let config = "[runtimes.ts]\nextensions = [\".ts\"]\ncommand = \"node worker.ts\"\n\n[[route]]\npath = \"/a\"\nhandler = \"a.ts\"\n";
        let all = BTreeMap::from([
            ("unsworn.toml".to_string(), config.to_string()),
            ("a.ts".to_string(), String::new()),
        ]);
        let project = compile_symbolic(&Mem(all), "unsworn.toml").project.unwrap();
        let look = Fake {
            files: BTreeMap::from([("package.json", "{}"), ("package-lock.json", "")]),
            installed: BTreeMap::new(),
        };
        let text = dockerfile(
            &plan(&project, config, &look, "unsworn:dev".into()),
            For::Containers,
        );
        assert!(
            text.contains("FROM debian:bookworm-slim\nRUN apt-get update"),
            "{text}"
        );
        assert!(text.contains("FROM node:22-bookworm-slim AS js-deps\nWORKDIR /app\nCOPY package.json package-lock.json ./\nRUN npm ci --omit=dev\n"), "{text}");
        assert!(
            text.contains(
                "COPY --from=node:22-bookworm-slim /usr/local/bin/node /usr/local/bin/node\n"
            ),
            "{text}"
        );
        assert!(text.contains("nothing says which node to use"), "{text}");
        assert!(!text.contains("pip install"), "{text}");
    }

    #[test]
    fn reads_tools_and_versions_from_text() {
        assert_eq!(
            programs("cd app && NODE_ENV=production node worker.js"),
            ["cd", "node"]
        );
        assert_eq!(
            version_in("go version go1.27.1 linux/amd64").as_deref(),
            Some("1.27.1")
        );
        assert_eq!(version_in("v26.13.0").as_deref(), Some("26.13.0"));
        assert_eq!(version_in("Python 3.14.8").as_deref(), Some("3.14.8"));
        assert_eq!(version_in("stable"), None);
        let look = Fake {
            files: BTreeMap::from([(".tool-versions", "nodejs 24.1.0\npython 3.13.2\n")]),
            installed: BTreeMap::from([("bun", "1.4.4")]),
        };
        assert_eq!(
            version(&look, Tool::Node),
            ("24.1.0".into(), Pinned::By(".tool-versions".into()))
        );
        assert_eq!(
            version(&look, Tool::Bun),
            ("1.4.4".into(), Pinned::Installed)
        );
        assert_eq!(version(&look, Tool::Deno), ("2".into(), Pinned::Unknown));
    }
}
