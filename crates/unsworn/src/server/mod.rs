//! The HTTP server: listeners, reloads and shutdown.

mod compress;
mod host;
mod proxy;
mod static_files;
pub mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use unsworn_core::Project;

use crate::load;
use crate::workers::{PoolOptions, Pools, SocketDir};
use support::{Body, HttpClient, Metrics};
use unsworn_core::pipeline::{ClientIp, KeyCache, Router};

use crate::store::Store;

/// One loaded version of the project. Replaced on reload.
pub struct Generation {
    pub project: Arc<Project>,
    pub router: Router,
    pub pools: Arc<Pools>,
    pub client_ip: ClientIp,
}

/// State that lives for the whole process.
pub struct Shared {
    pub generation: ArcSwap<Generation>,
    pub client: HttpClient,
    /// Rebuilt on reload only when `[store]` changes, so reloads keep
    /// in-memory rate limits and cache entries.
    pub store: ArcSwap<Store>,
    pub metrics: Metrics,
    /// JWT keys, kept across reloads.
    pub keys: Arc<KeyCache>,
    pub dev: bool,
    build: bool,
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
    /// Run runtimes' `build` commands. `unsworn serve --no-build` turns it
    /// off for deployments that ship prebuilt workers.
    pub build: bool,
    pub listen: Option<String>,
}

fn make_generation(
    project: Project,
    keys: &Arc<KeyCache>,
    root: &Path,
    dev: bool,
    build: bool,
    socket_dir: &Path,
    generation: u64,
) -> Result<Generation, String> {
    let client_ip = ClientIp::new(&project.server.trusted_proxies)?;
    let dir = socket_dir.join(format!("g{generation}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let pools = Pools::new(
        &project,
        PoolOptions {
            root: root.to_path_buf(),
            dev,
            build,
            socket_dir: dir,
            log_tap: None,
            withheld_env: Vec::new(),
        },
    );
    let project = Arc::new(project);
    Ok(Generation {
        router: Router::new(project.clone(), keys.clone()),
        project,
        pools,
        client_ip,
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
    let keys = Arc::new(KeyCache::default());
    let generation = make_generation(
        project,
        &keys,
        &opts.root,
        opts.dev,
        opts.build,
        sockets.path(),
        0,
    )?;
    if let Err(errors) = warm_up(&generation, opts.dev).await {
        for e in &errors {
            eprintln!("✗ {e}");
        }
        generation.pools.shutdown(Duration::from_secs(1)).await;
        return Err("an eager runtime failed to start".into());
    }
    let server = generation.project.server.clone();
    let store = Store::new(&generation.project.store)?;
    if store.backend_name() != "memory" {
        tracing::info!(store = store.backend_name(), "using a shared store");
    }

    let shared = Arc::new(Shared {
        generation: ArcSwap::from_pointee(generation),
        client: support::build_client(),
        store: ArcSwap::from_pointee(store),
        metrics: Metrics::default(),
        keys,
        dev: opts.dev,
        build: opts.build,
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
                shared.store.load().sweep();
            }
        });
    }

    if let Some(admin) = &server.admin_listen {
        let listener = TcpListener::bind(admin)
            .await
            .map_err(|e| format!("can't listen on {admin}: {e}"))?;
        tracing::info!(
            "metrics on http://{admin}/_unsworn/metrics, health details on http://{admin}/_unsworn/health"
        );
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
    let mut builder = auto::Builder::new(TokioExecutor::new());
    // On HTTP/1, hyper's header timer also runs while a kept-alive
    // connection waits for its next request, which makes it the idle
    // timeout. On HTTP/2, pings find peers that have gone away.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(server.idle_timeout);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(H2_PING_INTERVAL)
        .keep_alive_timeout(H2_PING_TIMEOUT);
    let options = ConnOptions {
        compression: server.compression,
        first_request: FIRST_REQUEST_TIMEOUT.min(server.idle_timeout),
    };
    // One permit per open connection. With none left, new connections wait
    // in the listener's backlog rather than taking memory here.
    let connections = Arc::new(tokio::sync::Semaphore::new(server.max_connections));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let permit = tokio::select! {
            permit = connections.clone().acquire_owned() => permit.expect("the semaphore is never closed"),
            _ = &mut shutdown => break,
        };
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = &mut shutdown => break,
        };
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
        let builder = builder.clone();
        let watcher = graceful.watcher();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tls {
                Some(acceptor) => {
                    let handshake = acceptor.accept(stream);
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, handshake).await {
                        Ok(Ok(stream)) => {
                            serve_connection(stream, peer, true, shared, builder, watcher, options)
                                .await
                        }
                        Ok(Err(e)) => tracing::debug!("TLS handshake failed: {e}"),
                        Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                    }
                }
                None => {
                    serve_connection(stream, peer, false, shared, builder, watcher, options).await
                }
            }
        });
    }
    tracing::info!("shutting down");
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

/// How long a new connection has to finish its TLS handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a new connection has to send its first request's headers, at
/// most. `[server].idle_timeout` lowers it: a connection that has sent
/// nothing gets no longer than one that's been useful.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How often an HTTP/2 connection is pinged, and how long it has to answer.
const H2_PING_INTERVAL: Duration = Duration::from_secs(30);
const H2_PING_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
struct ConnOptions {
    compression: bool,
    /// See [`FIRST_REQUEST_TIMEOUT`].
    first_request: Duration,
}

/// Serves one connection until it closes, or until `first_request` passes
/// without a request on it. That covers what hyper's own timers don't: a
/// connection that never says which HTTP version it speaks, and an HTTP/2
/// one that never opens a stream.
async fn serve_connection<I>(
    io: I,
    peer: std::net::SocketAddr,
    is_tls: bool,
    shared: Arc<Shared>,
    builder: auto::Builder<TokioExecutor>,
    watcher: hyper_util::server::graceful::Watcher,
    options: ConnOptions,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let seen = Arc::new(AtomicBool::new(false));
    let svc = {
        let seen = seen.clone();
        hyper::service::service_fn(move |req: Request<Incoming>| {
            seen.store(true, Ordering::Relaxed);
            let shared = shared.clone();
            async move {
                let accept = req.headers().get(http::header::ACCEPT_ENCODING).cloned();
                let method = req.method().clone();
                let resp = host::handle(shared, req, peer, is_tls).await;
                let resp = if options.compression {
                    compress::maybe_compress(&method, accept.as_ref(), resp)
                } else {
                    resp
                };
                Ok::<_, std::convert::Infallible>(resp)
            }
        })
    };
    let conn = builder.serve_connection_with_upgrades(TokioIo::new(io), svc);
    let conn = watcher.watch(conn.into_owned());
    let silent = async {
        tokio::time::sleep(options.first_request).await;
        if seen.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = conn => {}
        _ = silent => tracing::debug!(%peer, "closing a connection that sent no request"),
    }
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
    let generation = match make_generation(
        project,
        &shared.keys,
        &shared.root,
        shared.dev,
        shared.build,
        shared.sockets.path(),
        n,
    ) {
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
    let store = if shared.store.load().config == generation.project.store {
        None
    } else {
        match Store::new(&generation.project.store) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!("reload failed: {e}");
                generation.pools.shutdown(Duration::from_secs(1)).await;
                return;
            }
        }
    };
    if let Some(store) = store {
        tracing::info!(store = store.backend_name(), "the store changed");
        shared.store.store(Arc::new(store));
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
                    let resp: Response<Body> = if req.uri().path() == "/_unsworn/health" {
                        host::health(&shared, &shared.generation.load(), true)
                    } else if req.uri().path() == "/_unsworn/metrics" {
                        let runtimes = shared.generation.load().pools.health();
                        let store = shared.store.load();
                        let mut r =
                            Response::new(support::full(shared.metrics.render(&runtimes, &store)));
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
