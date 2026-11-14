//! The HTTP server: listeners, reloads and shutdown.

mod compress;
mod pipeline;
mod proxy;
mod static_files;
pub mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::Value;
use tokio::net::TcpListener;
use unsworn_core::Project;

use crate::load;
use crate::workers::{PoolOptions, Pools, SocketDir};
use support::{Body, ClientIp, HttpClient, KeyCache, MemoryStore, Metrics};

/// One loaded version of the project. Replaced on reload.
pub struct Generation {
    pub project: Arc<Project>,
    pub pools: Arc<Pools>,
    pub client_ip: ClientIp,
    pub well_known: HashMap<String, Value>,
}

/// State that lives for the whole process.
pub struct Shared {
    pub generation: ArcSwap<Generation>,
    pub client: HttpClient,
    pub store: MemoryStore,
    pub metrics: Metrics,
    pub keys: KeyCache,
    pub dev: bool,
    pub root: PathBuf,
    request_ids: AtomicU64,
    generations: AtomicU64,
    sockets: SocketDir,
}

impl Shared {
    pub fn next_request_id(&self) -> String {
        format!(
            "{:x}-{:x}",
            std::process::id(),
            self.request_ids.fetch_add(1, Ordering::Relaxed)
        )
    }
}

pub struct ServeOptions {
    pub root: PathBuf,
    pub config: String,
    pub dev: bool,
    pub listen: Option<String>,
}

fn make_generation(
    project: Project,
    root: &Path,
    dev: bool,
    socket_dir: &Path,
    generation: u64,
) -> Result<Generation, String> {
    let client_ip = ClientIp::new(&project.server.trusted_proxies)?;
    let well_known = project.protected_resource_docs().into_iter().collect();
    let dir = socket_dir.join(format!("g{generation}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let pools = Pools::new(
        &project,
        PoolOptions {
            root: root.to_path_buf(),
            dev,
            socket_dir: dir,
        },
    );
    Ok(Generation {
        project: Arc::new(project),
        pools,
        client_ip,
        well_known,
    })
}

/// Starts runtimes that should start now: every runtime in dev mode (so
/// handler names are verified up front), `eager` ones otherwise.
async fn warm_up(generation: &Generation, dev: bool) -> Result<(), Vec<String>> {
    if dev {
        let mut problems = Vec::new();
        for (runtime, missing, err) in generation.pools.start_all().await {
            if let Some(e) = err {
                problems.push(format!("✗ [runtimes.{runtime}]: {e}"));
            }
            for (handler, reason) in missing {
                problems.push(format!("✗ {handler}: {reason}"));
            }
        }
        for p in &problems {
            eprintln!("{p}");
        }
        return Ok(());
    }
    let errors = generation.pools.start_eager().await;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub async fn run(opts: ServeOptions) -> Result<(), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let compiled = load::load(&opts.root, &opts.config);
    let errors = load::print_diagnostics(&compiled.diagnostics);
    let Some(mut project) = compiled.project else {
        return Err(format!("{errors} error(s) in the project"));
    };
    if let Some(listen) = &opts.listen {
        project.server.listen = listen.clone();
    }
    let sockets = SocketDir::create().map_err(|e| format!("can't create socket directory: {e}"))?;
    let generation = make_generation(project, &opts.root, opts.dev, sockets.path(), 0)?;
    if let Err(errors) = warm_up(&generation, opts.dev).await {
        for e in &errors {
            eprintln!("✗ {e}");
        }
        generation.pools.shutdown(Duration::from_secs(1)).await;
        return Err("an eager runtime failed to start".into());
    }
    let server = generation.project.server.clone();

    let shared = Arc::new(Shared {
        generation: ArcSwap::from_pointee(generation),
        client: support::build_client(),
        store: MemoryStore::default(),
        metrics: Metrics::default(),
        keys: KeyCache::new(),
        dev: opts.dev,
        root: opts.root.clone(),
        request_ids: AtomicU64::new(1),
        generations: AtomicU64::new(1),
        sockets,
    });

    // Periodic cleanup of expired cache entries and idle rate-limit buckets.
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                shared.store.sweep();
            }
        });
    }

    if let Some(admin) = &server.admin_listen {
        let listener = TcpListener::bind(admin)
            .await
            .map_err(|e| format!("can't listen on {admin}: {e}"))?;
        tracing::info!("metrics on http://{admin}/_unsworn/metrics");
        tokio::spawn(admin_server(listener, shared.clone()));
    }

    let tls = match &server.tls {
        Some((cert, key)) => Some(tls_acceptor(&opts.root.join(cert), &opts.root.join(key))?),
        None => None,
    };
    let listener = TcpListener::bind(&server.listen)
        .await
        .map_err(|e| format!("can't listen on {}: {e}", server.listen))?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    tracing::info!(
        "unsworn {} listening on {scheme}://{local}",
        unsworn_core::VERSION
    );
    if opts.dev {
        eprintln!("unsworn dev server on {scheme}://{local}");
    }

    if opts.dev {
        spawn_watcher(shared.clone(), opts.config.clone());
    }
    spawn_reload_on_hup(shared.clone(), opts.config.clone());

    let graceful = GracefulShutdown::new();
    let builder = auto::Builder::new(TokioExecutor::new());
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let shared = shared.clone();
                let compression = server.compression;
                let builder = builder.clone();
                let watcher = graceful.watcher();
                let tls = tls.clone();
                tokio::spawn(async move {
                    let is_tls = tls.is_some();
                    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let shared = shared.clone();
                        async move {
                            let accept = req.headers().get(http::header::ACCEPT_ENCODING).cloned();
                            let method = req.method().clone();
                            let resp = pipeline::handle(shared, req, peer, is_tls).await;
                            let resp = if compression {
                                compress::maybe_compress(&method, accept.as_ref(), resp)
                            } else {
                                resp
                            };
                            Ok::<_, std::convert::Infallible>(resp)
                        }
                    });
                    match tls {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let conn = builder.serve_connection_with_upgrades(TokioIo::new(tls_stream), svc);
                                let _ = watcher.watch(conn.into_owned()).await;
                            }
                            Err(e) => tracing::debug!("TLS handshake failed: {e}"),
                        },
                        None => {
                            let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), svc);
                            let _ = watcher.watch(conn.into_owned()).await;
                        }
                    }
                });
            }
            _ = &mut shutdown => {
                tracing::info!("shutting down");
                break;
            }
        }
    }
    drop(listener);
    let grace = server.shutdown_timeout;
    if tokio::time::timeout(grace, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            "some connections were still open after {}s",
            grace.as_secs()
        );
    }
    shared
        .generation
        .load()
        .pools
        .shutdown(Duration::from_secs(2))
        .await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}

/// Reloads the project. Keeps the current one if the new one has errors.
async fn reload(shared: &Arc<Shared>, config: &str) {
    let compiled = load::load(&shared.root, config);
    let errors = load::print_diagnostics(&compiled.diagnostics);
    let Some(mut project) = compiled.project else {
        tracing::error!("reload failed with {errors} error(s); still serving the previous version");
        return;
    };
    let current = shared.generation.load_full();
    project.server.listen = current.project.server.listen.clone();
    let n = shared.generations.fetch_add(1, Ordering::Relaxed);
    let generation =
        match make_generation(project, &shared.root, shared.dev, shared.sockets.path(), n) {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("reload failed: {e}");
                return;
            }
        };
    if let Err(errors) = warm_up(&generation, shared.dev).await {
        for e in &errors {
            tracing::error!("{e}");
        }
        generation.pools.shutdown(Duration::from_secs(1)).await;
        tracing::error!("reload failed; still serving the previous version");
        return;
    }
    let old = shared.generation.swap(Arc::new(generation));
    tracing::info!("reloaded");
    if shared.dev {
        eprintln!("↻ reloaded");
    }
    let grace = old.project.server.shutdown_timeout;
    tokio::spawn(async move {
        old.pools.shutdown(grace).await;
    });
}

fn spawn_reload_on_hup(shared: Arc<Shared>, config: String) {
    tokio::spawn(async move {
        let Ok(mut hup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return;
        };
        while hup.recv().await.is_some() {
            tracing::info!("SIGHUP: reloading");
            reload(&shared, &config).await;
        }
    });
}

/// Dev mode: reload when project files change.
fn spawn_watcher(shared: Arc<Shared>, config: String) {
    use notify::{RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let root = shared.root.clone();
    let ignored = [
        "node_modules",
        ".git",
        "target",
        ".unsworn",
        "__pycache__",
        ".venv",
    ];
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let relevant = event.paths.iter().any(|p| {
                !p.components()
                    .any(|c| ignored.contains(&c.as_os_str().to_string_lossy().as_ref()))
            });
            if relevant {
                let _ = tx.send(());
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("file watching unavailable: {e}");
                return;
            }
        };
    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
        tracing::warn!("can't watch {}: {e}", root.display());
        return;
    }
    tokio::spawn(async move {
        let _watcher = watcher;
        while rx.recv().await.is_some() {
            // Debounce bursts of events (editors write several times).
            tokio::time::sleep(Duration::from_millis(200)).await;
            while rx.try_recv().is_ok() {}
            reload(&shared, &config).await;
        }
    });
}

async fn admin_server(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let shared = shared.clone();
                async move {
                    let resp: Response<Body> = if req.uri().path() == "/_unsworn/metrics" {
                        let runtimes = shared.generation.load().pools.health();
                        let mut r = Response::new(support::full(shared.metrics.render(&runtimes)));
                        r.headers_mut().insert(
                            http::header::CONTENT_TYPE,
                            http::HeaderValue::from_static("text/plain; version=0.0.4"),
                        );
                        r
                    } else {
                        support::problem(StatusCode::NOT_FOUND, "not found")
                    };
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

fn tls_acceptor(cert: &Path, key: &Path) -> Result<tokio_rustls::TlsAcceptor, String> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("can't read {}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("invalid certificate {}: {e}", cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("can't read key {}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("invalid certificate or key: {e}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}
