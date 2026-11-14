//! Worker processes: starting, supervising and talking to them.
//!
//! Each runtime gets a [`Pool`] of worker processes. Requests go to the
//! least busy worker that has capacity. Workers speak the protocol in
//! `unsworn-proto`, over stdin/stdout (Tier 1) or a Unix socket (Tier 2).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, mpsc, oneshot};
use unsworn_core::Project;
use unsworn_core::config::Protocol;
use unsworn_core::project::Runtime;
use unsworn_proto::{
    self as proto, Cancel, DecodeError, Frame, Message, PROTOCOL_VERSION, Request, Response, Verify,
};

/// What a handler call produced.
pub enum Reply {
    Response { response: Response, body: ReplyBody },
    Continue(Map<String, Value>),
}

pub enum ReplyBody {
    Empty,
    Bytes(Bytes),
    Stream(mpsc::Receiver<Result<Bytes, String>>),
}

#[derive(Debug, Clone)]
pub enum CallError {
    /// The runtime couldn't start or has no healthy workers.
    Unavailable(String),
    /// The worker reported that this handler doesn't exist.
    MissingHandler(String),
    Timeout,
    /// The worker exited or broke the protocol mid-request.
    WorkerFailed(String),
    /// The handler sent an `error` message.
    Handler(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unavailable(m) => write!(f, "runtime unavailable: {m}"),
            CallError::MissingHandler(m) => write!(f, "handler not found: {m}"),
            CallError::Timeout => write!(f, "the handler timed out"),
            CallError::WorkerFailed(m) => write!(f, "worker failed: {m}"),
            CallError::Handler(m) => write!(f, "handler error: {m}"),
        }
    }
}

enum Pending {
    Call(oneshot::Sender<Result<Reply, CallError>>),
    Stream(mpsc::Sender<Result<Bytes, String>>),
    Verify(oneshot::Sender<Vec<proto::MissingHandler>>),
}

struct Worker {
    id: String,
    tx: mpsc::UnboundedSender<Frame>,
    pending: Mutex<HashMap<String, Pending>>,
    inflight: AtomicUsize,
    max_in_flight: usize,
    alive: AtomicBool,
    verify: bool,
}

impl Worker {
    fn fail_all(&self, reason: &str) {
        let pending: Vec<Pending> = self
            .pending
            .lock()
            .unwrap()
            .drain()
            .map(|(_, p)| p)
            .collect();
        for p in pending {
            match p {
                Pending::Call(tx) => {
                    let _ = tx.send(Err(CallError::WorkerFailed(reason.to_string())));
                }
                Pending::Stream(tx) => {
                    let _ = tx.try_send(Err(reason.to_string()));
                }
                Pending::Verify(_) => {}
            }
        }
    }
}

#[derive(Default)]
enum StartState {
    #[default]
    NotStarted,
    Started,
    Failed {
        at: Instant,
        message: String,
    },
}

/// Settings shared by all pools.
#[derive(Clone)]
pub struct PoolOptions {
    pub root: PathBuf,
    pub dev: bool,
    pub socket_dir: PathBuf,
}

pub struct Pool {
    pub runtime: Arc<Runtime>,
    handlers: Vec<String>,
    opts: PoolOptions,
    workers: Mutex<Vec<Arc<Worker>>>,
    capacity: Notify,
    start: tokio::sync::Mutex<StartState>,
    missing: Mutex<HashMap<String, String>>,
    next_id: AtomicU64,
    shutting_down: AtomicBool,
    crashes: Mutex<Vec<Instant>>,
    worker_count: usize,
}

/// The pools for every runtime in a project.
pub struct Pools {
    pools: BTreeMap<String, Arc<Pool>>,
}

impl Pools {
    pub fn new(project: &Project, opts: PoolOptions) -> Arc<Pools> {
        let handlers = project.handlers_by_runtime();
        let pools = project
            .runtimes
            .iter()
            .map(|(name, runtime)| {
                let handlers = handlers.get(name).cloned().unwrap_or_default();
                (
                    name.clone(),
                    Pool::standalone(runtime.clone(), handlers, opts.clone()),
                )
            })
            .collect();
        Arc::new(Pools { pools })
    }

    pub fn get(&self, runtime: &str) -> Option<&Arc<Pool>> {
        self.pools.get(runtime)
    }

    /// Starts runtimes marked `eager`. Returns errors, including handlers
    /// that workers reported missing.
    pub async fn start_eager(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for pool in self.pools.values().filter(|p| p.runtime.eager) {
            match pool.ensure_started().await {
                Ok(()) => {
                    for (handler, reason) in pool.missing.lock().unwrap().iter() {
                        errors.push(format!("{handler}: {reason}"));
                    }
                }
                Err(e) => errors.push(format!("[runtimes.{}]: {e}", pool.runtime.name)),
            }
        }
        errors
    }

    /// Starts every runtime and returns (runtime, missing handlers, error).
    pub async fn start_all(&self) -> Vec<(String, Vec<(String, String)>, Option<String>)> {
        let mut out = Vec::new();
        for (name, pool) in &self.pools {
            if pool.handlers.is_empty() {
                continue;
            }
            let err = pool.ensure_started().await.err();
            let mut missing: Vec<(String, String)> = pool
                .missing
                .lock()
                .unwrap()
                .iter()
                .map(|(h, r)| (h.clone(), r.clone()))
                .collect();
            missing.sort();
            out.push((name.clone(), missing, err));
        }
        out
    }

    pub async fn shutdown(&self, grace: Duration) {
        let mut tasks = Vec::new();
        for pool in self.pools.values() {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move { pool.shutdown(grace).await }));
        }
        for t in tasks {
            let _ = t.await;
        }
    }

    /// Health of each runtime: (name, started, healthy workers, recent crashes).
    pub fn health(&self) -> Vec<(String, bool, usize, usize)> {
        self.pools
            .iter()
            .map(|(name, p)| {
                let alive = p
                    .workers
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|w| w.alive.load(Ordering::Relaxed))
                    .count();
                let started = matches!(
                    p.start.try_lock().as_deref(),
                    Ok(StartState::Started) | Err(_)
                );
                (name.clone(), started, alive, p.recent_crashes())
            })
            .collect()
    }
}

impl Pool {
    /// A pool for one runtime and the handlers it will be sent.
    pub fn standalone(
        runtime: Arc<Runtime>,
        handlers: Vec<String>,
        opts: PoolOptions,
    ) -> Arc<Pool> {
        let worker_count = runtime
            .workers
            .map(|w| w.max(1) as usize)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get().min(4))
                    .unwrap_or(2)
            });
        Arc::new(Pool {
            runtime,
            handlers,
            opts,
            workers: Mutex::new(Vec::new()),
            capacity: Notify::new(),
            start: tokio::sync::Mutex::new(StartState::NotStarted),
            missing: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            crashes: Mutex::new(Vec::new()),
            worker_count,
        })
    }

    /// Whether this runtime's workers answer `verify`.
    pub fn verify_supported(&self) -> bool {
        self.workers.lock().unwrap().iter().any(|w| w.verify)
    }

    /// The highest `max_in_flight` among the workers.
    pub fn max_in_flight(&self) -> usize {
        self.workers
            .lock()
            .unwrap()
            .iter()
            .map(|w| w.max_in_flight)
            .max()
            .unwrap_or(1)
    }

    pub fn missing(&self) -> HashMap<String, String> {
        self.missing.lock().unwrap().clone()
    }

    fn recent_crashes(&self) -> usize {
        let mut crashes = self.crashes.lock().unwrap();
        crashes.retain(|t| t.elapsed() < Duration::from_secs(60));
        crashes.len()
    }

    /// Builds (if needed) and starts the workers, once.
    pub async fn ensure_started(self: &Arc<Self>) -> Result<(), String> {
        let mut state = self.start.lock().await;
        match &*state {
            StartState::Started => return Ok(()),
            StartState::Failed { at, message } if at.elapsed() < Duration::from_secs(5) => {
                return Err(message.clone());
            }
            _ => {}
        }
        match self.start_inner().await {
            Ok(()) => {
                *state = StartState::Started;
                Ok(())
            }
            Err(message) => {
                tracing::error!(runtime = %self.runtime.name, "{message}");
                *state = StartState::Failed {
                    at: Instant::now(),
                    message: message.clone(),
                };
                Err(message)
            }
        }
    }

    async fn start_inner(self: &Arc<Self>) -> Result<(), String> {
        if let Some(build) = &self.runtime.build {
            tracing::info!(runtime = %self.runtime.name, "building: {build}");
            let output = shell(build)
                .current_dir(&self.opts.root)
                .stdin(Stdio::null())
                .output()
                .await
                .map_err(|e| format!("can't run build command: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                return Err(format!(
                    "build failed ({}):\n{}{}",
                    output.status,
                    stdout.trim(),
                    stderr.trim()
                ));
            }
        }
        let mut started = Vec::new();
        for i in 0..self.worker_count {
            match self.spawn_worker(i).await {
                Ok(w) => started.push(w),
                Err(e) => {
                    for w in &started {
                        w.alive.store(false, Ordering::Relaxed);
                    }
                    return Err(e);
                }
            }
        }
        tracing::info!(
            runtime = %self.runtime.name,
            workers = started.len(),
            "runtime started"
        );
        Ok(())
    }

    async fn spawn_worker(self: &Arc<Self>, index: usize) -> Result<Arc<Worker>, String> {
        let rt = &self.runtime;
        let worker_id = format!("{}-{index}", rt.name);
        let mut cmd = shell(&rt.command);
        cmd.current_dir(&self.opts.root)
            .env("UNSWORN_PROTOCOL", PROTOCOL_VERSION.to_string())
            .env("UNSWORN_VERSION", unsworn_core::VERSION)
            .env("UNSWORN_ROOT", &self.opts.root)
            .env("UNSWORN_WORKER_ID", &worker_id)
            .envs(&rt.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if self.opts.dev {
            cmd.env("UNSWORN_DEV", "1");
        }

        let socket_path = self.opts.socket_dir.join(format!(
            "{worker_id}-{}.sock",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = match rt.protocol {
            Protocol::Jsonl => {
                cmd.env("UNSWORN_TRANSPORT", "stdio").stdin(Stdio::piped());
                None
            }
            Protocol::Binary => {
                let _ = std::fs::remove_file(&socket_path);
                let l = tokio::net::UnixListener::bind(&socket_path)
                    .map_err(|e| format!("can't create socket {}: {e}", socket_path.display()))?;
                cmd.env(
                    "UNSWORN_TRANSPORT",
                    format!("unix:{}", socket_path.display()),
                )
                .stdin(Stdio::null());
                Some(l)
            }
        };

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("can't start `{}`: {e}", rt.command))?;
        let stderr = child.stderr.take().expect("stderr piped");
        spawn_log_reader(stderr, rt.name.clone(), worker_id.clone(), "stderr");
        let stdout = child.stdout.take().expect("stdout piped");

        let startup = rt.startup_timeout;
        let result = match listener {
            None => {
                let stdin = child.stdin.take().expect("stdin piped");
                self.connect(
                    worker_id.clone(),
                    stdout,
                    stdin,
                    Protocol::Jsonl,
                    child,
                    startup,
                )
                .await
            }
            Some(listener) => {
                spawn_log_reader(stdout, rt.name.clone(), worker_id.clone(), "stdout");
                let accepted = tokio::select! {
                    a = listener.accept() => a.map_err(|e| e.to_string()),
                    status = child.wait() => Err(format!("worker exited before connecting ({})", status.map(|s| s.to_string()).unwrap_or_default())),
                    _ = tokio::time::sleep(startup) => Err(format!("worker didn't connect to its socket within {}s", startup.as_secs())),
                };
                let _ = std::fs::remove_file(&socket_path);
                match accepted {
                    Ok((stream, _)) => {
                        let (r, w) = stream.into_split();
                        self.connect(worker_id.clone(), r, w, Protocol::Binary, child, startup)
                            .await
                    }
                    Err(e) => Err(e),
                }
            }
        };
        result.map_err(|e| format!("worker {worker_id}: {e}"))
    }

    async fn connect<R, W>(
        self: &Arc<Self>,
        worker_id: String,
        reader: R,
        writer: W,
        protocol: Protocol,
        mut child: Child,
        startup: Duration,
    ) -> Result<Arc<Worker>, String>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = FrameReader::new(reader, protocol);
        let ready = tokio::select! {
            f = reader.next() => f,
            status = child.wait() => {
                return Err(format!(
                    "exited before sending `ready` ({})",
                    status.map(|s| s.to_string()).unwrap_or_default()
                ));
            }
            _ = tokio::time::sleep(startup) => {
                return Err(format!("didn't send `ready` within {}s", startup.as_secs()));
            }
        };
        let ready = match ready {
            Some(Ok(Frame {
                message: Message::Ready(r),
                ..
            })) => r,
            Some(Ok(other)) => {
                return Err(format!(
                    "expected `ready` as the first message, got {:?}",
                    other.message
                ));
            }
            Some(Err(e)) => return Err(format!("expected `ready`: {e}")),
            None => return Err("closed its output before sending `ready`".into()),
        };
        if ready.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "speaks protocol {}, but Unsworn speaks {PROTOCOL_VERSION}",
                ready.protocol
            ));
        }

        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        let worker = Arc::new(Worker {
            id: worker_id.clone(),
            tx,
            pending: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
            max_in_flight: ready.max_in_flight.unwrap_or(1).max(1) as usize,
            alive: AtomicBool::new(true),
            verify: ready.verify,
        });
        tokio::spawn(write_loop(writer, rx, protocol));

        let pool = self.clone();
        let w = worker.clone();
        tokio::spawn(async move {
            pool.read_loop(&w, reader).await;
            w.alive.store(false, Ordering::Relaxed);
            w.fail_all("the worker stopped");
            // Give the process a moment to exit on its own.
            let exit = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            if exit.is_err() {
                let _ = child.kill().await;
            }
            pool.worker_gone(&w);
        });

        if ready.verify && !self.handlers.is_empty() {
            let (vtx, vrx) = oneshot::channel();
            let id = format!("v-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
            worker
                .pending
                .lock()
                .unwrap()
                .insert(id.clone(), Pending::Verify(vtx));
            let _ = worker.tx.send(Frame::new(Message::Verify(Verify {
                id,
                handlers: self.handlers.clone(),
            })));
            match tokio::time::timeout(startup, vrx).await {
                Ok(Ok(missing)) => {
                    let mut map = self.missing.lock().unwrap();
                    map.clear();
                    for m in missing {
                        tracing::error!(
                            runtime = %self.runtime.name,
                            handler = %m.handler,
                            "handler not found: {}",
                            m.reason
                        );
                        map.insert(m.handler, m.reason);
                    }
                }
                _ => {
                    tracing::warn!(worker = %worker_id, "worker didn't answer `verify`");
                }
            }
        }

        self.workers.lock().unwrap().push(worker.clone());
        self.capacity.notify_waiters();
        Ok(worker)
    }

    async fn read_loop<R: AsyncRead + Unpin>(&self, w: &Arc<Worker>, mut reader: FrameReader<R>) {
        while let Some(next) = reader.next().await {
            let frame = match next {
                Ok(f) => f,
                Err(DecodeError::Invalid { snippet, error }) => {
                    tracing::warn!(
                        worker = %w.id,
                        "ignored output that isn't a protocol message ({error}): {snippet}"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::error!(worker = %w.id, "protocol error: {e}");
                    return;
                }
            };
            let body = frame.body;
            match frame.message {
                Message::Response(response) => {
                    let id = response.id.clone();
                    let pending = w.pending.lock().unwrap().remove(&id);
                    let Some(Pending::Call(tx)) = pending else {
                        continue;
                    };
                    if response.stream {
                        let (stx, srx) = mpsc::channel(32);
                        w.pending.lock().unwrap().insert(id, Pending::Stream(stx));
                        let _ = tx.send(Ok(Reply::Response {
                            response,
                            body: ReplyBody::Stream(srx),
                        }));
                    } else {
                        let body = match (body, &response.json, &response.text) {
                            (Some(b), _, _) => ReplyBody::Bytes(b),
                            (None, Some(j), _) => ReplyBody::Bytes(Bytes::from(
                                serde_json::to_vec(j).unwrap_or_default(),
                            )),
                            (None, None, Some(t)) => ReplyBody::Bytes(Bytes::from(t.clone())),
                            _ => ReplyBody::Empty,
                        };
                        self.release(w);
                        let _ = tx.send(Ok(Reply::Response { response, body }));
                    }
                }
                Message::Chunk(chunk) => {
                    let tx = match w.pending.lock().unwrap().get(&chunk.id) {
                        Some(Pending::Stream(tx)) => Some(tx.clone()),
                        _ => None,
                    };
                    if let Some(tx) = tx {
                        let data = body.or_else(|| chunk.text.map(Bytes::from));
                        if let Some(data) = data
                            && tx.send(Ok(data)).await.is_err()
                        {
                            // The client went away.
                            w.pending.lock().unwrap().remove(&chunk.id);
                            self.release(w);
                            let _ = w.tx.send(Frame::new(Message::Cancel(Cancel {
                                id: chunk.id,
                                reason: Some("client disconnected".into()),
                            })));
                        }
                    }
                }
                Message::End(end) => {
                    if w.pending.lock().unwrap().remove(&end.id).is_some() {
                        self.release(w);
                    }
                }
                Message::Continue(c) => {
                    if let Some(Pending::Call(tx)) = w.pending.lock().unwrap().remove(&c.id) {
                        self.release(w);
                        let _ = tx.send(Ok(Reply::Continue(c.context)));
                    }
                }
                Message::Error(e) => match e.id {
                    Some(id) => match w.pending.lock().unwrap().remove(&id) {
                        Some(Pending::Call(tx)) => {
                            self.release(w);
                            let _ = tx.send(Err(CallError::Handler(e.message)));
                        }
                        Some(Pending::Stream(tx)) => {
                            self.release(w);
                            let _ = tx.try_send(Err(e.message));
                        }
                        _ => {}
                    },
                    None => {
                        tracing::error!(worker = %w.id, "worker error: {}", e.message);
                        return;
                    }
                },
                Message::Verified(v) => {
                    if let Some(Pending::Verify(tx)) = w.pending.lock().unwrap().remove(&v.id) {
                        let _ = tx.send(v.missing);
                    }
                }
                Message::Ready(_) | Message::Unknown => {}
                other => {
                    tracing::warn!(worker = %w.id, "unexpected message from worker: {other:?}");
                }
            }
        }
    }

    fn release(&self, w: &Worker) {
        w.inflight.fetch_sub(1, Ordering::AcqRel);
        self.capacity.notify_one();
    }

    fn worker_gone(self: &Arc<Self>, w: &Arc<Worker>) {
        self.workers.lock().unwrap().retain(|x| !Arc::ptr_eq(x, w));
        self.capacity.notify_waiters();
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let crashes = {
            let mut c = self.crashes.lock().unwrap();
            c.push(Instant::now());
            c.retain(|t| t.elapsed() < Duration::from_secs(60));
            c.len()
        };
        tracing::warn!(worker = %w.id, crashes, "worker exited; restarting");
        let delay = Duration::from_millis(100 * 2u64.pow((crashes as u32).min(7)))
            .min(Duration::from_secs(10));
        let pool = self.clone();
        let index: usize =
            w.id.rsplit('-')
                .next()
                .and_then(|i| i.parse().ok())
                .unwrap_or(0);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if pool.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            if let Err(e) = pool.spawn_worker(index).await {
                tracing::error!(runtime = %pool.runtime.name, "restart failed: {e}");
                // Try again later; worker_gone isn't called for a worker that never started.
                let p = pool.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    if !p.shutting_down.load(Ordering::Relaxed) {
                        let _ = p.spawn_worker(index).await;
                    }
                });
            }
        });
    }

    /// Sends a request to a worker and waits for its reply.
    pub fn call(
        self: &Arc<Self>,
        request: Request,
        body: Option<Bytes>,
        deadline: Instant,
    ) -> futures_util::future::BoxFuture<'static, Result<Reply, CallError>> {
        let pool = self.clone();
        Box::pin(async move { pool.call_inner(request, body, deadline).await })
    }

    async fn call_inner(
        self: Arc<Self>,
        mut request: Request,
        body: Option<Bytes>,
        deadline: Instant,
    ) -> Result<Reply, CallError> {
        if let Some(reason) = self.missing.lock().unwrap().get(&request.handler) {
            return Err(CallError::MissingHandler(format!(
                "{}: {reason}",
                request.handler
            )));
        }
        self.ensure_started()
            .await
            .map_err(CallError::Unavailable)?;
        if let Some(reason) = self.missing.lock().unwrap().get(&request.handler) {
            return Err(CallError::MissingHandler(format!(
                "{}: {reason}",
                request.handler
            )));
        }

        let worker = loop {
            if let Some(w) = self.pick() {
                break w;
            }
            let wait = self.capacity.notified();
            if let Some(w) = self.pick() {
                break w;
            }
            if tokio::time::timeout_at(deadline.into(), wait)
                .await
                .is_err()
            {
                return Err(if self.workers.lock().unwrap().is_empty() {
                    CallError::Unavailable("no healthy workers".into())
                } else {
                    CallError::Timeout
                });
            }
        };

        let id = format!("r-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        request.id = id.clone();
        request.deadline_ms = Some(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
        );
        let (tx, rx) = oneshot::channel();
        worker
            .pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending::Call(tx));
        let frame = match body {
            Some(b) if !b.is_empty() => match Frame::with_body(Message::Request(request), b) {
                Ok(f) => f,
                Err(e) => {
                    worker.pending.lock().unwrap().remove(&id);
                    self.release(&worker);
                    return Err(CallError::WorkerFailed(e.to_string()));
                }
            },
            _ => Frame::new(Message::Request(request)),
        };
        if worker.tx.send(frame).is_err() {
            worker.pending.lock().unwrap().remove(&id);
            self.release(&worker);
            return Err(CallError::WorkerFailed("the worker stopped".into()));
        }

        let mut guard = CancelGuard {
            pool: self.clone(),
            worker: worker.clone(),
            id: id.clone(),
            armed: true,
        };
        let result = tokio::time::timeout_at(deadline.into(), rx).await;
        match result {
            Ok(Ok(reply)) => {
                guard.armed = false;
                reply
            }
            Ok(Err(_)) => {
                guard.armed = false;
                Err(CallError::WorkerFailed("the worker stopped".into()))
            }
            Err(_) => Err(CallError::Timeout), // guard sends `cancel`
        }
    }

    fn pick(&self) -> Option<Arc<Worker>> {
        let workers = self.workers.lock().unwrap();
        let best = workers
            .iter()
            .filter(|w| w.alive.load(Ordering::Relaxed))
            .filter(|w| w.inflight.load(Ordering::Acquire) < w.max_in_flight)
            .min_by_key(|w| w.inflight.load(Ordering::Acquire))?;
        best.inflight.fetch_add(1, Ordering::AcqRel);
        Some(best.clone())
    }

    pub async fn shutdown(&self, grace: Duration) {
        self.shutting_down.store(true, Ordering::Relaxed);
        let workers: Vec<Arc<Worker>> = self.workers.lock().unwrap().drain(..).collect();
        let deadline = Instant::now() + grace;
        // Wait for in-flight requests, then close each worker's input so it can exit.
        for w in &workers {
            while w.inflight.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            w.alive.store(false, Ordering::Relaxed);
            w.fail_all("Unsworn is shutting down");
        }
        drop(workers); // dropping the senders closes stdin/sockets
    }
}

/// Sends `cancel` if a call is abandoned (timeout or client disconnect).
struct CancelGuard {
    pool: Arc<Pool>,
    worker: Arc<Worker>,
    id: String,
    armed: bool,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self
            .worker
            .pending
            .lock()
            .unwrap()
            .remove(&self.id)
            .is_some()
        {
            self.pool.release(&self.worker);
            let _ = self.worker.tx.send(Frame::new(Message::Cancel(Cancel {
                id: self.id.clone(),
                reason: Some("timeout or client disconnected".into()),
            })));
        }
    }
}

fn shell(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

fn spawn_log_reader<R: AsyncRead + Unpin + Send + 'static>(
    stream: R,
    runtime: String,
    worker: String,
    kind: &'static str,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "worker", runtime = %runtime, worker = %worker, stream = kind, "{line}");
        }
    });
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    protocol: Protocol,
) {
    let mut buf = Vec::new();
    while let Some(frame) = rx.recv().await {
        buf.clear();
        let encoded = match protocol {
            Protocol::Jsonl => proto::jsonl::encode(&frame, &mut buf),
            Protocol::Binary => proto::framed::encode(&frame, &mut buf),
        };
        if let Err(e) = encoded {
            tracing::error!("can't encode message: {e}");
            continue;
        }
        if writer.write_all(&buf).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Reads frames from a worker's output.
pub struct FrameReader<R> {
    reader: R,
    buf: BytesMut,
    decoder: Decoder,
    eof: bool,
}

enum Decoder {
    Jsonl(proto::jsonl::Decoder),
    Framed(proto::framed::Decoder),
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R, protocol: Protocol) -> Self {
        FrameReader {
            reader,
            buf: BytesMut::with_capacity(8192),
            decoder: match protocol {
                Protocol::Jsonl => Decoder::Jsonl(proto::jsonl::Decoder::default()),
                Protocol::Binary => Decoder::Framed(proto::framed::Decoder::default()),
            },
            eof: false,
        }
    }

    pub async fn next(&mut self) -> Option<Result<Frame, DecodeError>> {
        loop {
            let decoded = match &mut self.decoder {
                Decoder::Jsonl(d) => d.decode(&mut self.buf),
                Decoder::Framed(d) => d.decode(&mut self.buf),
            };
            match decoded {
                Ok(Some(frame)) => return Some(Ok(frame)),
                Ok(None) => {}
                Err(e) => return Some(Err(e)),
            }
            if self.eof {
                return None;
            }
            match self.reader.read_buf(&mut self.buf).await {
                Ok(0) | Err(_) => {
                    self.eof = true;
                    // A final line without a trailing newline.
                    if matches!(self.decoder, Decoder::Jsonl(_))
                        && !self.buf.is_empty()
                        && !self.buf.ends_with(b"\n")
                    {
                        self.buf.extend_from_slice(b"\n");
                        continue;
                    }
                    return None;
                }
                Ok(_) => {}
            }
        }
    }
}

/// A directory for worker sockets, removed when dropped.
pub struct SocketDir(pub PathBuf);

impl SocketDir {
    pub fn create() -> std::io::Result<SocketDir> {
        let dir = std::env::temp_dir().join(format!("unsworn-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(SocketDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
