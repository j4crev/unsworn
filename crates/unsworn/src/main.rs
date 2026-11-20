//! The `unsworn` binary: HTTP ingress, worker supervision and the CLI,
//! wrapped around `unsworn-core`. See `docs/architecture.md`.

#[cfg(not(target_os = "linux"))]
compile_error!("the unsworn server runs on Linux only");

// The released binary is a static musl build, and musl's allocator takes
// one lock for every thread, which cost 25 to 40% of throughput under
// load. mimalloc doesn't. On glibc it's about level on static routes and
// uses about 10% less CPU on proxied requests and handler calls.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod build;
mod codegen;
mod commands;
mod conformance;
mod load;
mod plan;
mod scale;
mod server;
mod store;
mod top;
mod workers;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "unsworn",
    version,
    about = "A sealed, config-driven API router. Write handlers in any language."
)]
struct Cli {
    /// Project directory.
    #[arg(long, short = 'C', global = true, default_value = ".")]
    dir: PathBuf,
    /// Config file, relative to the project directory.
    #[arg(long, short, global = true)]
    config: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the router.
    Serve {
        /// Override `[server].listen`.
        #[arg(long)]
        listen: Option<String>,
        /// Log format.
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        log_format: String,
        /// Skip runtimes' `build` commands and start the workers as they
        /// are, for deployments that ship prebuilt workers. The official
        /// image sets UNSWORN_NO_BUILD=1; UNSWORN_NO_BUILD=0 builds again.
        #[arg(
            long,
            env = "UNSWORN_NO_BUILD",
            value_parser = clap::builder::BoolishValueParser::new()
        )]
        no_build: bool,
    },
    /// Serve with file watching, every runtime started up front, and
    /// detailed errors in responses.
    Dev {
        #[arg(long)]
        listen: Option<String>,
    },
    /// Validate the config, routes and handler references.
    Check {
        /// Also start each runtime and ask its workers whether every
        /// handler exists.
        #[arg(long)]
        deep: bool,
        /// Check against a deployment target's capabilities.
        #[arg(long)]
        target: Option<String>,
    },
    /// Bundle the project for a deployment target: `image` (a Dockerfile
    /// for the server and its workers), `lambda` (the same, for AWS
    /// Lambda), `cloudflare` (a Worker), or `vercel` or `netlify` (a
    /// function on each).
    Build {
        /// The deployment target.
        #[arg(long)]
        target: String,
        /// Output directory, relative to the project directory. Default:
        /// where the platform's tools look (`.vercel/output`,
        /// `.netlify/v1`), else `dist/<target>`.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// For `image`: the image to copy the `unsworn` binary from.
        /// Default: this version's official image.
        #[arg(long)]
        from: Option<String>,
    },
    /// Print the resolved route table.
    Routes {
        /// Show each route's merged policies and where they come from.
        #[arg(long)]
        policies: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Write the rules the project enforces, worked out for every route,
    /// to `unsworn.plan`. Commit it next to the config.
    Plan {
        /// Fail when the file isn't up to date, without writing it. For CI.
        #[arg(long)]
        check: bool,
        /// The file to write, relative to the project directory. `-`
        /// prints the plan.
        #[arg(long, short)]
        output: Option<String>,
    },
    /// Show what changed in the rules since another version of the
    /// project, and which changes let more requests through.
    Diff {
        /// A git revision (the default is HEAD), or the directory of
        /// another copy of the project.
        base: Option<String>,
        /// Exit with an error when a change loosens the rules (`looser`),
        /// or when anything changed (`any`).
        #[arg(long, value_parser = ["looser", "any"])]
        fail_on: Option<String>,
    },
    /// Write typed request models for handlers, from each route's params,
    /// query, body and response schemas. Without a language, writes every
    /// file the config's `[gen]` table lists.
    Gen {
        /// One language: ts, python, go or rust. Its types are printed
        /// unless `[gen]` or `--output` names a file.
        lang: Option<String>,
        /// The file to write, relative to the project directory.
        #[arg(long, short)]
        output: Option<String>,
        /// The Go package of the generated file. Default: main.
        #[arg(long)]
        package: Option<String>,
        /// Fail when a file isn't up to date, without writing it. For CI.
        #[arg(long)]
        check: bool,
    },
    /// Show what each route costs on a running server: requests, latency,
    /// and the handler's share of time and CPU. Busiest handlers first.
    Top {
        /// Where the server's stats are: its admin listener. Default:
        /// `[server].admin_listen`, or where `unsworn dev` listens.
        #[arg(long)]
        addr: Option<String>,
        /// Print totals since the server started, once, and exit.
        #[arg(long)]
        once: bool,
        /// Print the server's numbers as JSON, once, and exit.
        #[arg(long)]
        json: bool,
        /// Seconds between updates.
        #[arg(long, short, default_value_t = 2.0)]
        interval: f64,
        /// The most routes to list.
        #[arg(short = 'n', default_value_t = 25)]
        limit: usize,
    },
    /// Print the OpenAPI document for the routes Unsworn serves.
    Openapi {
        /// Write to a file instead of stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Run the protocol conformance suite against a runtime.
    Conformance {
        /// The runtime to test, from `[runtimes.*]`.
        #[arg(long)]
        runtime: String,
        /// The file that implements the conformance handlers.
        #[arg(long)]
        handlers: String,
    },
    /// Create a starter `unsworn.toml` in the project directory.
    Init,
    /// Print the JSON Schema for the config file, for editor autocomplete.
    Schema,
}

fn init_logging(format: &str, dev: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if dev { "info,access=info" } else { "info" };
    let filter = EnvFilter::try_from_env("UNSWORN_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    use std::io::IsTerminal;
    let terminal = std::io::stderr().is_terminal();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(terminal)
        .with_writer(std::io::stderr);
    if format == "json" {
        builder.json().flatten_event(true).init();
    } else if dev {
        builder.compact().without_time().init();
    } else {
        builder.init();
    }
    // A terminal gets each line as it happens, and in color. `json` output
    // has no colors anywhere.
    server::access::init(format, format == "json" || !(dev || terminal));
}

/// Closes this process to the others its user runs, which is what workers
/// are.
///
/// Linux lets a process read the environment and memory of another one
/// with the same user (`/proc/<pid>/environ`, `/proc/<pid>/mem`, `ptrace`),
/// so a handler could read the credentials Unsworn keeps out of its
/// environment from Unsworn's own. A process that isn't "dumpable" can
/// only be read that way by one with `CAP_SYS_PTRACE`, which a worker has
/// only when the server runs as root outside a container. It also writes
/// no core dump, which would hold the same credentials.
fn close_to_workers() {
    use rustix::process::{DumpableBehavior, set_dumpable_behavior};
    if let Err(e) = set_dumpable_behavior(DumpableBehavior::NotDumpable) {
        eprintln!(
            "! can't close this process to others running as its user ({e}): handlers can read the credentials in its environment"
        );
    }
}

fn main() -> ExitCode {
    // Before anything else starts: a worker is a process this one starts.
    close_to_workers();
    let cli = Cli::parse();
    let root = match std::fs::canonicalize(&cli.dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ can't open {}: {e}", cli.dir.display());
            return ExitCode::FAILURE;
        }
    };
    let config_given = cli.config.is_some();
    let config = load::find_config(&root, cli.config.as_deref());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result: Result<(), String> = match cli.command {
        Command::Serve {
            listen,
            log_format,
            no_build,
        } => {
            init_logging(&log_format, false);
            runtime.block_on(server::run(server::ServeOptions {
                root,
                config,
                dev: false,
                build: !no_build,
                listen,
            }))
        }
        Command::Dev { listen } => {
            init_logging("text", true);
            runtime.block_on(server::run(server::ServeOptions {
                root,
                config,
                dev: true,
                build: true,
                listen,
            }))
        }
        Command::Check { deep, target } => {
            if deep {
                init_logging("text", true);
            }
            runtime.block_on(commands::check(&root, &config, deep, target.as_deref()))
        }
        Command::Build { target, out, from } => build::build(build::BuildOptions {
            root,
            config,
            target,
            out,
            from,
        }),
        Command::Routes { policies, json } => commands::routes(&root, &config, policies, json),
        Command::Plan { check, output } => plan::plan(&root, &config, check, output.as_deref()),
        Command::Diff { base, fail_on } => plan::diff(
            &root,
            &config,
            config_given,
            base.as_deref(),
            fail_on.as_deref(),
        ),
        Command::Gen {
            lang,
            output,
            package,
            check,
        } => codegen::run(
            &root,
            &config,
            lang.as_deref(),
            output.as_deref(),
            package.as_deref(),
            check,
        ),
        Command::Top {
            addr,
            once,
            json,
            interval,
            limit,
        } => top::run(
            &root,
            &config,
            top::Options {
                addr,
                once,
                json,
                interval,
                limit,
            },
        ),
        Command::Openapi { output } => commands::openapi(&root, &config, output.as_deref()),
        Command::Conformance {
            runtime: rt,
            handlers,
        } => {
            init_logging("text", true);
            runtime.block_on(conformance::run(&root, &config, &rt, &handlers))
        }
        Command::Init => commands::init(&root),
        Command::Schema => {
            let schema = unsworn_core::config::json_schema();
            println!(
                "{}",
                serde_json::to_string_pretty(&schema).unwrap_or_default()
            );
            Ok(())
        }
    };
    server::access::flush();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("✗ {e}");
            }
            ExitCode::FAILURE
        }
    }
}
