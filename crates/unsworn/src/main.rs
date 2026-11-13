//! The `unsworn` binary: HTTP ingress, worker supervision and the CLI,
//! wrapped around `unsworn-core`. See `docs/architecture.md`.

mod commands;
mod conformance;
mod load;
mod server;
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
    /// Print the resolved route table.
    Routes {
        /// Show each route's merged policies and where they come from.
        #[arg(long)]
        policies: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
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
}

fn init_logging(format: &str, dev: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if dev { "info,access=info" } else { "info" };
    let filter = EnvFilter::try_from_env("UNSWORN_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    use std::io::IsTerminal;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    if format == "json" {
        builder.json().flatten_event(true).init();
    } else if dev {
        builder.compact().without_time().init();
    } else {
        builder.init();
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match std::fs::canonicalize(&cli.dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ can't open {}: {e}", cli.dir.display());
            return ExitCode::FAILURE;
        }
    };
    let config = load::find_config(&root, cli.config.as_deref());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result: Result<(), String> = match cli.command {
        Command::Serve { listen, log_format } => {
            init_logging(&log_format, false);
            runtime.block_on(server::run(server::ServeOptions {
                root,
                config,
                dev: false,
                listen,
            }))
        }
        Command::Dev { listen } => {
            init_logging("text", true);
            runtime.block_on(server::run(server::ServeOptions {
                root,
                config,
                dev: true,
                listen,
            }))
        }
        Command::Check { deep, target } => {
            if deep {
                init_logging("text", true);
            }
            runtime.block_on(commands::check(&root, &config, deep, target.as_deref()))
        }
        Command::Routes { policies, json } => commands::routes(&root, &config, policies, json),
        Command::Openapi { output } => commands::openapi(&root, &config, output.as_deref()),
        Command::Conformance {
            runtime: rt,
            handlers,
        } => {
            init_logging("text", true);
            runtime.block_on(conformance::run(&root, &config, &rt, &handlers))
        }
        Command::Init => commands::init(&root),
    };
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
