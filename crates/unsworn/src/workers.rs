//! Worker processes: starting, supervising and talking to them.
//!
//! Each runtime gets a [`Pool`] of worker processes. Requests go to the
//! least busy worker that has capacity. Workers speak the protocol in
//! `unsworn-proto`, over stdin/stdout (Tier 1) or a Unix socket (Tier 2).
//!
//! An `http` runtime's workers are HTTP servers instead: the pool starts
//! and supervises them the same way, and hands out where each one listens
//! (see [`Pool::http_lease`]) for the proxy to forward to.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use unsworn_core::Project;
use unsworn_core::config::ResponseValidation;
use unsworn_core::config::{Listen, Protocol};
use unsworn_core::project::{Function, Outbound, Runtime, Target};
use unsworn_core::trace::TraceContext;
use unsworn_core::{calls, outbound};
use unsworn_proto::{
    self as proto, Cancel, Chunk, DecodeError, End, Frame, Json, LogLevel, Message,
    PROTOCOL_VERSION, Request, Response, Verify,
};

mod endpoint;

use crate::scale::{self, Decision, Sample, Scaler};
use crate::server::support::{Body, HttpClient};
pub use unsworn_core::pipeline::{BODY_IDLE, BodyError, CallError};

/// Unacknowledged bytes of a streamed request body allowed per request.
const STREAM_WINDOW: usize = 256 * 1024;
/// Largest `chunk` sent for a streamed request body.
const STREAM_CHUNK: usize = 64 * 1024;
/// Bodies this large are written to a worker after their message instead
/// of being copied in behind it.
const WRITE_APART: usize = 16 * 1024;
/// What a worker's reader keeps free for the next read, at least.
const READ_SPARE: usize = 16 * 1024;

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

/// A request body for [`Pool::call`].
pub enum CallBody {
    Empty,
    Bytes(Bytes),
    /// Sent to the worker as it arrives, as `chunk` messages, keeping at
    /// most [`STREAM_WINDOW`] bytes unacknowledged.
    Stream(Body),
}

impl From<Option<Bytes>> for CallBody {
    fn from(b: Option<Bytes>) -> CallBody {
        match b {
            Some(b) if !b.is_empty() => CallBody::Bytes(b),
            _ => CallBody::Empty,
        }
    }
}

enum Pending {
    Call(oneshot::Sender<Result<Reply, CallError>>),
    Stream(mpsc::Sender<Result<Bytes, String>>),
    Verify(oneshot::Sender<Vec<proto::MissingHandler>>),
}

/// What a worker's log records about a request are tagged with.
struct CallInfo {
    request_id: String,
    route: String,
    /// What a function called for this request starts from. Only kept in
    /// a project that has functions.
    caller: Option<Box<Caller>>,
}

/// The request a function is called for.
#[derive(Clone)]
struct Caller {
    request_id: String,
    handler: String,
    /// The function gets the same context, and no more time.
    context: Map<String, Value>,
    deadline: Instant,
    /// The trace the request belongs to, which its functions and outbound
    /// requests join.
    trace: Option<TraceContext>,
}

/// How long a finished request stays in its worker's `CallLog`, since
/// stderr is read separately and can lag behind the response.
const LOG_GRACE: Duration = Duration::from_secs(5);
/// The most finished requests a `CallLog` keeps, so a busy worker's
/// late log lines can lose their request id but memory stays bounded.
const LOG_GRACE_MAX: usize = 10_000;

/// Requests on one worker by protocol id: those in flight, and those that
/// finished within `LOG_GRACE`. Shared with the worker's output readers,
/// so log lines can be tied to their request.
#[derive(Default)]
struct CallLog {
    calls: HashMap<String, CallInfo>,
    /// Finished requests, oldest first, so expiring them never scans
    /// `calls`: that scan ran on every request and capped throughput.
    finished: VecDeque<(Instant, String)>,
}

impl CallLog {
    fn get(&self, id: &str) -> Option<&CallInfo> {
        self.calls.get(id)
    }

    fn start(&mut self, id: String, call: CallInfo) {
        self.expire();
        self.calls.insert(id, call);
    }

    fn finish(&mut self, id: &str) {
        if self.calls.contains_key(id) {
            self.finished.push_back((Instant::now(), id.to_owned()));
        }
        self.expire();
    }

    fn expire(&mut self) {
        while let Some((done, _)) = self.finished.front() {
            if done.elapsed() < LOG_GRACE && self.finished.len() <= LOG_GRACE_MAX {
                break;
            }
            if let Some((_, id)) = self.finished.pop_front() {
                self.calls.remove(&id);
            }
        }
    }
}

type Calls = Arc<Mutex<CallLog>>;

struct Worker {
    id: String,
    /// Its place in the pool, which its id ends with.
    index: usize,
    /// The process started for it: the worker, or the shell that runs it.
    pid: Option<u32>,
    /// Messages for the worker. `None` closes its input, which is how a
    /// worker is asked to exit.
    tx: mpsc::UnboundedSender<Option<Frame>>,
    /// The pool is done with this worker: it gets no new requests, and
    /// its exit isn't a crash.
    retired: AtomicBool,
    pending: Mutex<HashMap<String, Pending>>,
    calls: Calls,
    inflight: AtomicUsize,
    max_in_flight: usize,
    alive: AtomicBool,
    verify: bool,
    stream_body: bool,
    /// The worker reads a JSON request body from the raw bytes after the
    /// message.
    raw_json: bool,
    /// Its handlers can call functions.
    can_call: bool,
    /// Its handlers can ask for requests to outbound services.
    can_fetch: bool,
    /// Credit windows for streamed request bodies, by request id.
    credits: Mutex<HashMap<String, Arc<Credit>>>,
    /// Function calls made for each request in flight, to stop when the
    /// request ends before they do.
    subcalls: Mutex<HashMap<String, Vec<tokio::task::AbortHandle>>>,
    /// Whether `subcalls` has ever held anything, so a request that calls
    /// nothing doesn't take its lock.
    made_calls: AtomicBool,
    /// Where it listens, for the worker of an `http` runtime.
    http: Option<HttpAt>,
}

/// Where an `http` runtime's worker listens for Unsworn.
struct HttpAt {
    /// As a proxy's upstream is written: `unix:/path`, or
    /// `http://127.0.0.1:<port>`.
    upstream: Arc<str>,
    target: Target,
}

/// One request's hold on an `http` runtime's worker: where to send the
/// request. The worker counts as busy with it until this is dropped.
pub struct HttpLease {
    worker: Arc<Worker>,
}

impl HttpLease {
    /// Where the worker listens, as a proxy's upstream is written.
    pub fn upstream(&self) -> &str {
        self.worker.http.as_ref().map_or("", |at| &at.upstream)
    }

    pub fn target(&self) -> &Target {
        const NOWHERE: &Target = &Target::Url;
        self.worker.http.as_ref().map_or(NOWHERE, |at| &at.target)
    }
}

impl Drop for HttpLease {
    fn drop(&mut self) {
        self.worker.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The credit window for one streamed request body. Permits are bytes the
/// worker may receive before acknowledging more.
struct Credit {
    window: Semaphore,
    outstanding: AtomicU64,
    closed: Notify,
}

impl Credit {
    fn new() -> Credit {
        Credit {
            window: Semaphore::new(STREAM_WINDOW),
            outstanding: AtomicU64::new(0),
            closed: Notify::new(),
        }
    }

    /// The request is over: stops the body pump.
    fn close(&self) {
        self.window.close();
        self.closed.notify_waiters();
    }

    fn ack(&self, bytes: u64) {
        // Never grant more than was sent, whatever the worker claims.
        let mut current = self.outstanding.load(Ordering::Acquire);
        loop {
            let n = bytes.min(current);
            match self.outstanding.compare_exchange(
                current,
                current - n,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.window.add_permits(n as usize);
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }
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
        for (_, credit) in self.credits.lock().unwrap().drain() {
            credit.close();
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
    /// Run each runtime's `build` command before its workers start.
    pub build: bool,
    pub socket_dir: PathBuf,
    /// Receives a copy of every `log` message, for the conformance suite.
    pub log_tap: Option<mpsc::UnboundedSender<proto::Log>>,
    /// Environment variables workers don't inherit: the project's
    /// [`Project::secret_env`]. [`Pools::new`] fills it in.
    pub withheld_env: Vec<String>,
}

pub struct Pool {
    pub runtime: Arc<Runtime>,
    handlers: Vec<String>,
    opts: PoolOptions,
    workers: Mutex<Vec<Arc<Worker>>>,
    capacity: Notify,
    start: tokio::sync::Mutex<StartState>,
    missing: Mutex<HashMap<String, String>>,
    /// Handlers whose routes stream request bodies.
    stream_handlers: Vec<String>,
    next_id: AtomicU64,
    shutting_down: AtomicBool,
    crashes: Mutex<Vec<Instant>>,
    /// The fewest workers to run, and the number started first.
    min: usize,
    /// The most to run. See [`crate::scale`] for when one is added.
    max: usize,
    /// A worker is being started to add to the pool.
    growing: AtomicBool,
    /// A call had to wait for a worker since the last look at the load.
    waited: AtomicBool,
    /// Every pool of the project, set when it has functions for handlers
    /// to call.
    peers: std::sync::OnceLock<std::sync::Weak<Pools>>,
    /// The runtime's name, to hand out without copying it.
    name: Arc<str>,
    /// CPU time its workers have used, for `unsworn top`.
    cpu: Mutex<CpuSeen>,
    /// Where an `http` runtime's servers call functions and outbound
    /// services, once one has started.
    endpoint: std::sync::OnceLock<Option<endpoint::Endpoint>>,
    /// The requests in flight on an `http` runtime's servers, by the
    /// token each was sent with.
    http_callers: Mutex<HashMap<u64, endpoint::Entry>>,
}

/// A pool's CPU time so far. A worker's is read from `/proc` while it
/// runs, so what an exited one had used is kept from the last look.
#[derive(Default)]
struct CpuSeen {
    /// Ticks at the last look, by process.
    workers: HashMap<u32, u64>,
    /// Ticks of processes that have gone.
    gone: u64,
}

/// What a runtime's workers have used so far.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeUsage {
    pub name: String,
    pub workers: usize,
    pub cpu_seconds: f64,
    pub memory_kb: u64,
}

/// The pools for every runtime in a project.
pub struct Pools {
    pools: BTreeMap<String, Arc<Pool>>,
    /// The project's `[[function]]` entries, which a handler in any pool
    /// may call.
    functions: BTreeMap<String, Arc<Function>>,
    response_validation: ResponseValidation,
    /// The project's `[outbound.<name>]` services, which Unsworn calls
    /// for handlers.
    outbound: BTreeMap<String, Arc<Outbound>>,
    /// Makes those requests. Built for the first one.
    client: std::sync::OnceLock<HttpClient>,
}

impl Pools {
    pub fn new(project: &Project, mut opts: PoolOptions) -> Arc<Pools> {
        opts.withheld_env = project.secret_env.clone();
        let handlers = project.handlers_by_runtime();
        let streaming = project.stream_handlers_by_runtime();
        let pools = project
            .runtimes
            .iter()
            .map(|(name, runtime)| {
                let handlers = handlers.get(name).cloned().unwrap_or_default();
                let streaming = streaming.get(name).cloned().unwrap_or_default();
                (
                    name.clone(),
                    Pool::standalone(runtime.clone(), handlers, streaming, opts.clone()),
                )
            })
            .collect();
        let pools = Arc::new(Pools {
            pools,
            functions: project.functions.clone(),
            response_validation: project.server.response_validation,
            outbound: project.outbound.clone(),
            client: std::sync::OnceLock::new(),
        });
        if !pools.functions.is_empty() || !pools.outbound.is_empty() {
            for pool in pools.pools.values() {
                let _ = pool.peers.set(Arc::downgrade(&pools));
            }
        }
        pools
    }

    /// One pool, the functions its handlers may call (each run by that
    /// same pool) and the services they may fetch from. For the
    /// conformance suite, which tests one runtime.
    pub fn single(
        pool: Arc<Pool>,
        functions: Vec<Function>,
        outbound: Vec<Outbound>,
    ) -> Arc<Pools> {
        let pools = Arc::new(Pools {
            pools: BTreeMap::from([(pool.runtime.name.clone(), pool.clone())]),
            functions: functions
                .into_iter()
                .map(|f| (f.name.clone(), Arc::new(f)))
                .collect(),
            response_validation: ResponseValidation::Off,
            outbound: outbound
                .into_iter()
                .map(|o| (o.name.clone(), Arc::new(o)))
                .collect(),
            client: std::sync::OnceLock::new(),
        });
        let _ = pool.peers.set(Arc::downgrade(&pools));
        pools
    }

    pub fn get(&self, runtime: &str) -> Option<&Arc<Pool>> {
        self.pools.get(runtime)
    }

    /// What each runtime's workers have used so far. Reads `/proc`, so
    /// call it off the async threads.
    pub fn usage(&self) -> Vec<RuntimeUsage> {
        self.pools.values().map(|p| p.usage()).collect()
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

impl Pools {
    /// Runs a function for a handler that called it, and says how it went.
    /// What a call is checked against and turned into is the core's
    /// (`unsworn_core::calls`), the same as on every other host.
    async fn call_function(&self, call: proto::Call, caller: Caller) -> proto::CallResult {
        let outcome = self.run_function(&call.function, &call.json, caller).await;
        let (status, json, error) = match outcome {
            Ok((status, json)) => (status, json, None),
            Err(failure) => (failure.status, failure.json, Some(failure.error)),
        };
        proto::CallResult {
            id: call.id,
            status,
            json,
            error,
        }
    }

    async fn run_function(
        &self,
        name: &str,
        input: &Value,
        caller: Caller,
    ) -> Result<(u16, Value), calls::Failure> {
        let (function, request) = calls::prepare(
            &self.functions,
            name,
            input,
            calls::Caller {
                handler: caller.handler,
                context: caller.context,
                trace: caller.trace,
            },
        )?;
        let Some(pool) = self.pools.get(&function.handler.runtime) else {
            return Err(calls::Failure::new(
                503,
                format!("no runtime \"{}\"", function.handler.runtime),
            ));
        };
        // The function has its own time limit, and no more time than the
        // request it's called for.
        let deadline = caller.deadline.min(Instant::now() + function.timeout);
        let reply = pool
            .call(request, CallBody::Empty, deadline, caller.request_id)
            .await;
        let (response, body) = match reply {
            Ok(Reply::Response { response, body }) => (response, body),
            Ok(Reply::Continue(_)) => return Err(calls::continued()),
            Err(e) => return Err(calls::failed(e)),
        };
        let answer = match body {
            ReplyBody::Empty => calls::Answer::Empty,
            ReplyBody::Bytes(bytes) => calls::Answer::Bytes(bytes),
            ReplyBody::Stream(_) => calls::Answer::Stream,
        };
        calls::finish(&function, self.response_validation, response, answer)
    }
}

impl Pools {
    /// Makes a handler's request to an outbound service, with the
    /// service's own headers added, and gives back the `fetched` to send.
    async fn fetch(&self, fetch: proto::Fetch, body: Option<Bytes>, caller: &Caller) -> Frame {
        let id = fetch.id.clone();
        let fail = |status: u16, error: String| {
            Frame::new(Message::Fetched(proto::Fetched {
                id: id.clone(),
                status,
                error: Some(error),
                ..Default::default()
            }))
        };
        let body = crate::server::support::full(body.unwrap_or_default());
        let sent = self
            .send_outbound(
                &fetch.service,
                &fetch.method,
                &fetch.path,
                &fetch.headers,
                body,
                caller,
            )
            .await;
        let (parts, body) = match sent {
            Ok(answer) => answer,
            Err((status, error)) => return fail(status, error),
        };
        let headers = outbound::response_headers(&parts.headers);
        let fetched = Message::Fetched(proto::Fetched {
            id: id.clone(),
            status: parts.status.as_u16(),
            headers,
            ..Default::default()
        });
        if body.is_empty() {
            return Frame::new(fetched);
        }
        Frame::with_body(fetched, body).unwrap_or_else(|e| fail(502, e.to_string()))
    }

    /// Sends a request to the outbound service `name`, with the service's
    /// own headers added, and reads its answer. `Err` is the status and
    /// the reason the caller gets when there's no answer to give.
    async fn send_outbound(
        &self,
        name: &str,
        method: &str,
        path: &str,
        headers: &BTreeMap<String, String>,
        body: Body,
        caller: &Caller,
    ) -> Result<(http::response::Parts, Bytes), (u16, String)> {
        let service = outbound::service(&self.outbound, name)?;
        let planned = outbound::plan(service, method, path, headers, caller.trace.as_ref())?;
        let method = planned.method;
        let mut request = http::Request::builder()
            .method(method.as_str())
            .uri(planned.url);
        for (name, value) in &planned.headers {
            request = request.header(name, value);
        }
        let request = request
            .body(body)
            .map_err(|e| (400, format!("the request can't be made: {e}")))?;

        let started = Instant::now();
        let deadline = caller.deadline.min(started + service.timeout);
        let client = self
            .client
            .get_or_init(crate::server::support::build_client);
        let limit = usize::try_from(service.max_response).unwrap_or(usize::MAX);
        let answer = async {
            let response = client
                .request(request)
                .await
                .map_err(|e| format!("couldn't reach the service: {e}"))?;
            let (parts, body) = response.into_parts();
            let body = http_body_util::Limited::new(body, limit)
                .collect()
                .await
                .map_err(|e| {
                    if e.is::<http_body_util::LengthLimitError>() {
                        outbound::too_large(service)
                    } else {
                        format!("the response couldn't be read: {e}")
                    }
                })?
                .to_bytes();
            Ok::<_, String>((parts, body))
        };
        let outcome = tokio::time::timeout_at(deadline.into(), answer).await;
        let path = path.split('?').next().unwrap_or_default();
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let (parts, body) = match outcome {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::warn!(target: "outbound", service = %service.name, %method, %path, id = %caller.request_id, "{error}");
                return Err((502, error));
            }
            Err(_) => {
                tracing::warn!(target: "outbound", service = %service.name, %method, %path, id = %caller.request_id, "timed out");
                return Err((504, "the service took too long to answer".into()));
            }
        };
        // What a handler reached, and when: the query is left out, since
        // that's where a token may be.
        tracing::info!(
            target: "outbound",
            service = %service.name,
            %method,
            %path,
            status = parts.status.as_u16(),
            ms = format_args!("{ms:.3}"),
            id = %caller.request_id,
        );
        Ok((parts, body))
    }
}

impl Pool {
    /// A pool for one runtime and the handlers it will be sent.
    pub fn standalone(
        runtime: Arc<Runtime>,
        handlers: Vec<String>,
        stream_handlers: Vec<String>,
        opts: PoolOptions,
    ) -> Arc<Pool> {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let name = runtime.name.as_str().to_owned();
        let min = runtime.workers.min.max(1) as usize;
        let max = runtime.workers.most(cores).max(min);
        Arc::new(Pool {
            runtime,
            handlers,
            opts,
            workers: Mutex::new(Vec::new()),
            capacity: Notify::new(),
            start: tokio::sync::Mutex::new(StartState::NotStarted),
            missing: Mutex::new(HashMap::new()),
            stream_handlers,
            next_id: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            crashes: Mutex::new(Vec::new()),
            min,
            max,
            growing: AtomicBool::new(false),
            waited: AtomicBool::new(false),
            peers: std::sync::OnceLock::new(),
            name: Arc::from(name),
            cpu: Mutex::default(),
            endpoint: std::sync::OnceLock::new(),
            http_callers: Mutex::default(),
        })
    }

    /// The runtime's name.
    pub fn name(&self) -> Arc<str> {
        self.name.clone()
    }

    /// What this runtime's workers have used so far. Reads `/proc`.
    fn usage(&self) -> RuntimeUsage {
        let pids: Vec<u32> = self
            .workers
            .lock()
            .unwrap()
            .iter()
            .filter_map(|w| w.pid)
            .collect();
        let mut cpu = self.cpu.lock().unwrap();
        let mut now = HashMap::new();
        let mut memory_kb = 0;
        for pid in &pids {
            if let Some(ticks) = scale::tree_ticks(*pid) {
                now.insert(*pid, ticks);
            }
            memory_kb += scale::tree_memory_kb(*pid);
        }
        let gone: u64 = cpu
            .workers
            .iter()
            .filter(|(pid, _)| !now.contains_key(pid))
            .map(|(_, ticks)| ticks)
            .sum();
        cpu.gone += gone;
        cpu.workers = now;
        RuntimeUsage {
            name: self.runtime.name.clone(),
            workers: pids.len(),
            cpu_seconds: scale::seconds(cpu.gone + cpu.workers.values().sum::<u64>()),
            memory_kb,
        }
    }

    /// Whether this runtime's workers answer `verify`.
    pub fn verify_supported(&self) -> bool {
        self.workers.lock().unwrap().iter().any(|w| w.verify)
    }

    /// Whether this runtime's workers say their handlers can call functions.
    pub fn calls_supported(&self) -> bool {
        self.workers.lock().unwrap().iter().any(|w| w.can_call)
    }

    /// Whether this runtime's workers say their handlers can ask for
    /// requests to outbound services.
    pub fn fetch_supported(&self) -> bool {
        self.workers.lock().unwrap().iter().any(|w| w.can_fetch)
    }

    /// Whether this runtime's workers accept streamed request bodies.
    pub fn stream_body_supported(&self) -> bool {
        self.workers.lock().unwrap().iter().any(|w| w.stream_body)
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
        if let Some(build) = self.runtime.build.as_ref().filter(|_| self.opts.build) {
            tracing::info!(runtime = %self.runtime.name, "building: {build}");
            let mut cmd = shell(build);
            self.withhold_env(&mut cmd);
            let output = cmd
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
        let withheld: Vec<&str> = self
            .withheld()
            .filter(|name| std::env::var_os(name).is_some())
            .collect();
        if !withheld.is_empty() {
            tracing::info!(
                runtime = %self.runtime.name,
                "not passing {} to workers: the config reads credentials from {}; to pass one on, list it under [runtimes.{}].env",
                withheld.join(", "),
                if withheld.len() == 1 { "it" } else { "them" },
                self.runtime.name
            );
        }
        let mut started = Vec::new();
        for i in 0..self.min {
            match self.spawn_worker(i).await {
                Ok(w) => started.push(w),
                Err(e) => {
                    for w in &started {
                        w.alive.store(false, Ordering::Relaxed);
                    }
                    return Err(self.unbuilt_hint(e));
                }
            }
        }
        tracing::info!(
            runtime = %self.runtime.name,
            workers = started.len(),
            "runtime started"
        );
        if self.max > self.min {
            tokio::spawn(watch_load(Arc::downgrade(self)));
        }
        Ok(())
    }

    /// The workers that take new requests.
    fn active(&self) -> Vec<Arc<Worker>> {
        self.workers
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.alive.load(Ordering::Relaxed))
            .cloned()
            .collect()
    }

    /// Starts one more worker, unless the pool is at its most or already
    /// starting one. `why` is for the log.
    fn grow(self: &Arc<Self>, why: &'static str) {
        if self.shutting_down.load(Ordering::Relaxed)
            || self.workers.lock().unwrap().len() >= self.max
            || self.growing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let pool = self.clone();
        tokio::spawn(async move {
            // The lowest place no worker has, so ids stay small.
            let index = {
                let workers = pool.workers.lock().unwrap();
                (0..)
                    .find(|i| !workers.iter().any(|w| w.index == *i))
                    .unwrap_or(0)
            };
            match pool.spawn_worker(index).await {
                Ok(_) => tracing::info!(
                    runtime = %pool.runtime.name,
                    workers = pool.workers.lock().unwrap().len(),
                    "added a worker: {why}"
                ),
                Err(e) => {
                    tracing::warn!(runtime = %pool.runtime.name, "couldn't add a worker: {e}");
                }
            }
            pool.growing.store(false, Ordering::Release);
        });
    }

    /// Retires the newest worker: it gets no more requests, finishes the
    /// ones it has, and is then asked to exit. `why` is for the log.
    fn shrink(self: &Arc<Self>, why: &'static str) {
        let Some(worker) = self.active().into_iter().max_by_key(|w| w.index) else {
            return;
        };
        worker.retired.store(true, Ordering::Relaxed);
        worker.alive.store(false, Ordering::Relaxed);
        tracing::info!(
            runtime = %self.runtime.name,
            worker = %worker.id,
            "retiring a worker: {why}"
        );
        tokio::spawn(async move {
            while worker.inflight.load(Ordering::Acquire) > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let _ = worker.tx.send(None);
        });
    }

    /// A worker that won't start, for a runtime whose `build` was skipped,
    /// is most likely missing what that build makes: say so. The official
    /// image skips builds, since handlers can't write to the project there.
    fn unbuilt_hint(&self, error: String) -> String {
        match &self.runtime.build {
            Some(build) if !self.opts.build => format!(
                "{error}\n  [runtimes.{}].build (`{build}`) didn't run, since building is off (`--no-build` or UNSWORN_NO_BUILD=1, which the official image sets). Run it when building the image, or set UNSWORN_NO_BUILD=0 to build at startup",
                self.runtime.name
            ),
            _ => error,
        }
    }

    /// The credentials' environment variables this runtime doesn't ask for
    /// in its own `env`.
    fn withheld(&self) -> impl Iterator<Item = &str> {
        self.opts
            .withheld_env
            .iter()
            .filter(|name| !self.runtime.env.contains_key(*name))
            .map(String::as_str)
    }

    /// Keeps the variables Unsworn's own credentials come from out of a
    /// command's environment. Handlers are the project's code, but a flaw
    /// in one (or in a package it uses) shouldn't hand over the secret
    /// that signs every token.
    fn withhold_env(&self, cmd: &mut Command) {
        for name in self.withheld() {
            cmd.env_remove(name);
        }
    }

    async fn spawn_worker(self: &Arc<Self>, index: usize) -> Result<Arc<Worker>, String> {
        let rt = &self.runtime;
        let worker_id = format!("{}-{index}", rt.name);
        let mut cmd = match rt.protocol {
            Protocol::Http => server_shell(&rt.command),
            _ => shell(&rt.command),
        };
        self.withhold_env(&mut cmd);
        cmd.current_dir(&self.opts.root)
            .env("UNSWORN_PROTOCOL", PROTOCOL_VERSION.to_string())
            .env("UNSWORN_VERSION", unsworn_core::VERSION)
            .env("UNSWORN_ROOT", &self.opts.root)
            .env("UNSWORN_WORKER_ID", &worker_id)
            .env("UNSWORN_LOG_LEVEL", worker_log_level())
            // This Unsworn takes a JSON body as raw bytes (`body_json`), in
            // both directions. An SDK that knows how says so in `ready`.
            .env("UNSWORN_RAW_JSON", "1")
            // And answers `call` and `fetch` messages, for `ctx.call` and
            // `ctx.fetch`.
            .env("UNSWORN_CALLS", "1")
            .env("UNSWORN_FETCH", "1")
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
        if rt.protocol == Protocol::Http {
            return self
                .spawn_http_worker(cmd, worker_id.clone(), index, socket_path)
                .await
                .map_err(|e| format!("worker {worker_id}: {e}"));
        }
        let listener = match rt.protocol {
            Protocol::Jsonl => {
                cmd.env("UNSWORN_TRANSPORT", "stdio").stdin(Stdio::piped());
                None
            }
            Protocol::Http => unreachable!("an http runtime's worker is started above"),
            Protocol::Framed | Protocol::Binary => {
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
        let place = (index, child.id());
        let calls = Calls::default();
        let stderr = child.stderr.take().expect("stderr piped");
        spawn_log_reader(
            stderr,
            rt.name.clone(),
            worker_id.clone(),
            "stderr",
            calls.clone(),
        );
        let stdout = child.stdout.take().expect("stdout piped");

        let startup = rt.startup_timeout;
        let result = match listener {
            None => {
                let stdin = child.stdin.take().expect("stdin piped");
                self.connect(
                    worker_id.clone(),
                    place,
                    stdout,
                    stdin,
                    Protocol::Jsonl,
                    child,
                    startup,
                    calls,
                )
                .await
            }
            Some(listener) => {
                spawn_log_reader(
                    stdout,
                    rt.name.clone(),
                    worker_id.clone(),
                    "stdout",
                    calls.clone(),
                );
                let accepted = tokio::select! {
                    a = listener.accept() => a.map_err(|e| e.to_string()),
                    status = child.wait() => Err(format!("worker exited before connecting ({})", status.map(|s| s.to_string()).unwrap_or_default())),
                    _ = tokio::time::sleep(startup) => Err(format!("worker didn't connect to its socket within {}s", startup.as_secs())),
                };
                let _ = std::fs::remove_file(&socket_path);
                match accepted {
                    Ok((stream, _)) => {
                        let (r, w) = stream.into_split();
                        self.connect(
                            worker_id.clone(),
                            place,
                            r,
                            w,
                            Protocol::Framed,
                            child,
                            startup,
                            calls,
                        )
                        .await
                    }
                    Err(e) => Err(e),
                }
            }
        };
        result.map_err(|e| format!("worker {worker_id}: {e}"))
    }

    /// Starts the server of an `http` runtime and waits until it listens.
    ///
    /// Here the worker listens and Unsworn connects, the other way round
    /// from Tier 2: on a Unix socket in the directory only Unsworn's user
    /// can open (`UNSWORN_SOCKET`), or on a port Unsworn picks (`PORT`)
    /// for a server that can't use a socket.
    async fn spawn_http_worker(
        self: &Arc<Self>,
        mut cmd: Command,
        worker_id: String,
        index: usize,
        socket_path: PathBuf,
    ) -> Result<Arc<Worker>, String> {
        let rt = &self.runtime;
        let upstream: Arc<str> = match rt.listen {
            Listen::Socket => {
                let _ = std::fs::remove_file(&socket_path);
                cmd.env("UNSWORN_SOCKET", &socket_path);
                format!("unix:{}", socket_path.display()).into()
            }
            Listen::Port => {
                let port = free_port().map_err(|e| format!("can't find a free port: {e}"))?;
                cmd.env("PORT", port.to_string())
                    .env("UNSWORN_PORT", port.to_string());
                format!("http://127.0.0.1:{port}").into()
            }
        };
        // Where it calls functions and outbound services, in a project
        // that has them.
        if self.peers.get().is_some()
            && let Some(endpoint) = self.endpoint()
        {
            cmd.env("UNSWORN_CALLS_SOCKET", &endpoint.socket)
                .env("UNSWORN_CALLS_URL", &endpoint.url);
        }
        cmd.env("UNSWORN_TRANSPORT", "http")
            .stdin(Stdio::null())
            // A group of its own, so that asking it to stop reaches the
            // server when the command leaves a shell in between.
            .process_group(0);
        // A server has no input to see closed when Unsworn is gone, as a
        // protocol worker has: without this, one would outlive an Unsworn
        // that was killed. The signal goes to the shell `server_shell`
        // starts, which passes it on.
        // SAFETY: between `fork` and `exec` only async-signal-safe calls
        // are allowed, and this is one system call (`prctl`), which
        // allocates nothing and takes no lock.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ))
                .map_err(std::io::Error::from)
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("can't start `{}`: {e}", rt.command))?;
        let pid = child.id();
        let calls = Calls::default();
        for (stream, name) in [
            (child.stderr.take().map(Output::Err), "stderr"),
            (child.stdout.take().map(Output::Out), "stdout"),
        ] {
            match stream {
                Some(Output::Err(s)) => {
                    spawn_log_reader(s, rt.name.clone(), worker_id.clone(), name, calls.clone())
                }
                Some(Output::Out(s)) => {
                    spawn_log_reader(s, rt.name.clone(), worker_id.clone(), name, calls.clone())
                }
                None => {}
            }
        }

        let target = Target::of(&upstream);
        let startup = rt.startup_timeout;
        let listening = async {
            // Most servers listen within a few milliseconds, so look soon
            // and then less often: 1, 2, 4, 8, 16, then every 25 ms.
            let mut wait = LISTEN_FIRST_CHECK;
            while !accepts(&target).await {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(LISTEN_CHECK_EVERY);
            }
        };
        let place = match rt.listen {
            Listen::Socket => "the socket in UNSWORN_SOCKET".to_string(),
            Listen::Port => "the port in PORT".to_string(),
        };
        let started = tokio::select! {
            _ = listening => Ok(()),
            status = child.wait() => Err(format!(
                "exited before listening on {place} ({})",
                status.map(|s| s.to_string()).unwrap_or_default()
            )),
            _ = tokio::time::sleep(startup) => Err(format!(
                "didn't listen on {place} within {}s",
                startup.as_secs()
            )),
        };
        if let Err(e) = started {
            signal_group(pid, rustix::process::Signal::KILL);
            let _ = std::fs::remove_file(&socket_path);
            return Err(e);
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<Option<Frame>>();
        let worker = Arc::new(Worker {
            id: worker_id,
            index,
            pid,
            tx,
            retired: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            calls,
            inflight: AtomicUsize::new(0),
            // The server decides how many requests it takes at once.
            max_in_flight: usize::MAX,
            alive: AtomicBool::new(true),
            verify: false,
            stream_body: true,
            raw_json: false,
            can_call: false,
            can_fetch: false,
            credits: Mutex::new(HashMap::new()),
            subcalls: Mutex::new(HashMap::new()),
            made_calls: AtomicBool::new(false),
            http: Some(HttpAt { upstream, target }),
        });

        let pool = self.clone();
        let w = worker.clone();
        tokio::spawn(async move {
            // The pool asks a worker to stop by sending `None`.
            let asked = async {
                while let Some(message) = rx.recv().await {
                    if message.is_none() {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = child.wait() => {}
                _ = asked => {
                    // A server stops on SIGTERM, finishing what it has.
                    signal_group(pid, rustix::process::Signal::TERM);
                    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                        signal_group(pid, rustix::process::Signal::KILL);
                        let _ = child.wait().await;
                    }
                }
            }
            w.alive.store(false, Ordering::Relaxed);
            let _ = std::fs::remove_file(&socket_path);
            pool.worker_gone(&w);
        });

        self.workers.lock().unwrap().push(worker.clone());
        self.capacity.notify_waiters();
        Ok(worker)
    }

    /// A worker of an `http` runtime to send a request to, starting the
    /// runtime if this is its first. Waits up to `wait` for one, which a
    /// restart may take.
    pub async fn http_lease(self: &Arc<Self>, wait: Duration) -> Result<HttpLease, CallError> {
        self.ensure_started()
            .await
            .map_err(CallError::Unavailable)?;
        let deadline = Instant::now() + wait;
        loop {
            if let Some(worker) = self.pick() {
                return Ok(HttpLease { worker });
            }
            let again = self.capacity.notified();
            if let Some(worker) = self.pick() {
                return Ok(HttpLease { worker });
            }
            if tokio::time::timeout_at(deadline.into(), again)
                .await
                .is_err()
            {
                return Err(CallError::Unavailable("its server isn't running".into()));
            }
        }
    }

    async fn connect<R, W>(
        self: &Arc<Self>,
        worker_id: String,
        (index, pid): (usize, Option<u32>),
        reader: R,
        writer: W,
        protocol: Protocol,
        mut child: Child,
        startup: Duration,
        calls: Calls,
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

        let (tx, rx) = mpsc::unbounded_channel::<Option<Frame>>();
        let worker = Arc::new(Worker {
            id: worker_id.clone(),
            index,
            pid,
            tx,
            retired: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            calls,
            inflight: AtomicUsize::new(0),
            max_in_flight: ready.max_in_flight.unwrap_or(1).max(1) as usize,
            alive: AtomicBool::new(true),
            verify: ready.verify,
            stream_body: ready.stream_body,
            raw_json: ready.raw_json,
            can_call: ready.calls,
            can_fetch: ready.fetch,
            credits: Mutex::new(HashMap::new()),
            subcalls: Mutex::new(HashMap::new()),
            made_calls: AtomicBool::new(false),
            http: None,
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
            let _ = worker.tx.send(Some(Frame::new(Message::Verify(Verify {
                id,
                handlers: self.handlers.clone(),
            }))));
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

        if !ready.stream_body && !self.stream_handlers.is_empty() {
            let mut map = self.missing.lock().unwrap();
            for h in &self.stream_handlers {
                tracing::error!(
                    runtime = %self.runtime.name,
                    handler = %h,
                    "the worker doesn't accept streamed request bodies, which `stream_body` needs"
                );
                map.insert(
                    h.clone(),
                    "the worker doesn't accept streamed request bodies (`stream_body`); update its SDK"
                        .into(),
                );
            }
        }

        self.workers.lock().unwrap().push(worker.clone());
        self.capacity.notify_waiters();
        Ok(worker)
    }

    async fn read_loop<R: AsyncRead + Unpin>(
        self: &Arc<Self>,
        w: &Arc<Worker>,
        mut reader: FrameReader<R>,
    ) {
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
            let mut body = frame.body;
            match frame.message {
                Message::Response(mut response) => {
                    // JSON sent as the raw body is the response's `json`
                    // from here on, as text nothing has read.
                    if response.body_json
                        && let Some(text) = body.take()
                    {
                        response.json = Some(Json::unchecked(text));
                    }
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
                            (None, Some(j), _) => ReplyBody::Bytes(j.to_bytes()),
                            (None, None, Some(t)) => ReplyBody::Bytes(Bytes::from(t.clone())),
                            _ => ReplyBody::Empty,
                        };
                        self.release(w, &id);
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
                            self.release(w, &chunk.id);
                            let _ = w.tx.send(Some(Frame::new(Message::Cancel(Cancel {
                                id: chunk.id,
                                reason: Some("client disconnected".into()),
                            }))));
                        }
                    }
                }
                Message::End(end) => {
                    if w.pending.lock().unwrap().remove(&end.id).is_some() {
                        self.release(w, &end.id);
                    }
                }
                Message::Continue(c) => {
                    if let Some(Pending::Call(tx)) = w.pending.lock().unwrap().remove(&c.id) {
                        self.release(w, &c.id);
                        let _ = tx.send(Ok(Reply::Continue(c.context)));
                    }
                }
                Message::Error(e) => match e.id {
                    Some(id) => match w.pending.lock().unwrap().remove(&id) {
                        Some(Pending::Call(tx)) => {
                            self.release(w, &id);
                            let _ = tx.send(Err(CallError::Handler(e.message)));
                        }
                        Some(Pending::Stream(tx)) => {
                            self.release(w, &id);
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
                Message::Log(log) => {
                    if let Some(tap) = &self.opts.log_tap {
                        let _ = tap.send(log.clone());
                    }
                    let calls = w.calls.lock().unwrap();
                    let call = log.id.as_deref().and_then(|id| calls.get(id));
                    let fields =
                        (!log.fields.is_empty()).then(|| Value::Object(log.fields).to_string());
                    emit_log(
                        log.level,
                        &self.runtime.name,
                        &w.id,
                        call,
                        &log.message,
                        fields.as_deref(),
                    );
                }
                Message::Ack(ack) => {
                    let credit = w.credits.lock().unwrap().get(&ack.id).cloned();
                    if let Some(credit) = credit {
                        credit.ack(ack.bytes);
                    }
                }
                Message::Call(call) => self.function_call(w, call),
                Message::Fetch(fetch) => self.outbound_fetch(w, fetch, body.take()),
                Message::Ready(_) | Message::Unknown => {}
                other => {
                    tracing::warn!(worker = %w.id, "unexpected message from worker: {other:?}");
                }
            }
        }
    }

    /// The project's pools and the request a `call` or `fetch` is for.
    /// `Err` is the status and reason there's nothing to do it for.
    fn caller_of(&self, w: &Worker, request: &str) -> Result<(Arc<Pools>, Caller), (u16, String)> {
        let Some(pools) = self.peers.get().and_then(std::sync::Weak::upgrade) else {
            return Err((
                404,
                "this project has no functions or outbound services".into(),
            ));
        };
        let in_flight = w.pending.lock().unwrap().contains_key(request);
        let caller = w
            .calls
            .lock()
            .unwrap()
            .get(request)
            .and_then(|c| c.caller.as_deref().cloned());
        match caller.filter(|_| in_flight) {
            Some(caller) => Ok((pools, caller)),
            None => Err((
                409,
                format!("a call is made for a request in flight, and \"{request}\" isn't one"),
            )),
        }
    }

    /// Runs `work` for a request, until the request ends.
    fn spawn_for(
        w: &Arc<Worker>,
        request: String,
        work: impl Future<Output = ()> + Send + 'static,
    ) {
        let task = tokio::spawn(work);
        w.made_calls.store(true, Ordering::Relaxed);
        let mut subcalls = w.subcalls.lock().unwrap();
        // The request may have ended while this was starting.
        if w.pending.lock().unwrap().contains_key(&request) {
            subcalls
                .entry(request)
                .or_default()
                .push(task.abort_handle());
        } else {
            task.abort();
        }
    }

    /// A handler called a function. Runs it in its runtime's pool and
    /// sends the worker the result, unless the request the call was made
    /// for ends first.
    fn function_call(self: &Arc<Self>, w: &Arc<Worker>, call: proto::Call) {
        let (pools, caller) = match self.caller_of(w, &call.request) {
            Ok(found) => found,
            Err((status, error)) => {
                let _ =
                    w.tx.send(Some(Frame::new(Message::Result(proto::CallResult {
                        id: call.id,
                        status,
                        json: Value::Null,
                        error: Some(error),
                    }))));
                return;
            }
        };
        let worker = w.clone();
        Self::spawn_for(w, call.request.clone(), async move {
            let result = pools.call_function(call, caller).await;
            let _ = worker.tx.send(Some(Frame::new(Message::Result(result))));
        });
    }

    /// A handler asked for a request to an outbound service. Makes it and
    /// sends the worker the answer, unless the handler's own request ends
    /// first.
    fn outbound_fetch(self: &Arc<Self>, w: &Arc<Worker>, fetch: proto::Fetch, body: Option<Bytes>) {
        let (pools, caller) = match self.caller_of(w, &fetch.request) {
            Ok(found) => found,
            Err((status, error)) => {
                let _ = w.tx.send(Some(Frame::new(Message::Fetched(proto::Fetched {
                    id: fetch.id,
                    status,
                    error: Some(error),
                    ..Default::default()
                }))));
                return;
            }
        };
        let worker = w.clone();
        Self::spawn_for(w, fetch.request.clone(), async move {
            let frame = pools.fetch(fetch, body, &caller).await;
            let _ = worker.tx.send(Some(frame));
        });
    }

    fn release(&self, w: &Worker, id: &str) {
        w.calls.lock().unwrap().finish(id);
        if w.made_calls.load(Ordering::Relaxed)
            && let Some(calls) = w.subcalls.lock().unwrap().remove(id)
        {
            // Nobody is waiting for what they'd answer.
            calls.iter().for_each(tokio::task::AbortHandle::abort);
        }
        if let Some(credit) = w.credits.lock().unwrap().remove(id) {
            credit.close();
        }
        w.inflight.fetch_sub(1, Ordering::AcqRel);
        self.capacity.notify_one();
    }

    fn worker_gone(self: &Arc<Self>, w: &Arc<Worker>) {
        self.workers.lock().unwrap().retain(|x| !Arc::ptr_eq(x, w));
        self.capacity.notify_waiters();
        if self.shutting_down.load(Ordering::Relaxed) || w.retired.load(Ordering::Relaxed) {
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
        let index = w.index;
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
    /// `request_id` is the HTTP request's id, used to tag the worker's logs.
    pub fn call(
        self: &Arc<Self>,
        request: Request,
        body: CallBody,
        deadline: Instant,
        request_id: String,
    ) -> futures_util::future::BoxFuture<'static, Result<Reply, CallError>> {
        let pool = self.clone();
        Box::pin(async move { pool.call_inner(request, body, deadline, request_id).await })
    }

    async fn call_inner(
        self: Arc<Self>,
        mut request: Request,
        body: CallBody,
        deadline: Instant,
        request_id: String,
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
            // Every worker is full: that's one reason to add another.
            self.waited.store(true, Ordering::Relaxed);
            self.grow("calls were waiting for a worker");
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
        if matches!(body, CallBody::Stream(_)) && !worker.stream_body {
            self.release(&worker, &id);
            return Err(CallError::Unavailable(
                "the runtime's workers don't accept streamed request bodies".into(),
            ));
        }
        // A JSON body goes to a worker that takes one as the raw bytes after
        // the message, where the worker parses it if the handler asks.
        // Other workers get it inside the message, which only text known
        // to be JSON can be written into.
        let mut body = body;
        match request.json.take() {
            Some(Json::Raw(raw)) if worker.raw_json && matches!(body, CallBody::Empty) => {
                request.body_json = true;
                body = CallBody::Bytes(raw.text().clone());
            }
            Some(Json::Raw(raw)) if !raw.is_checked() => {
                if let Err(detail) = unsworn_core::pipeline::check_json_text(raw.text()) {
                    self.release(&worker, &id);
                    return Err(CallError::BadJson(detail));
                }
                request.json = Some(Json::raw(raw.text().clone()));
            }
            json => request.json = json,
        }
        request.id = id.clone();
        request.deadline_ms = Some(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
        );
        let (tx, rx) = oneshot::channel();
        worker.calls.lock().unwrap().start(
            id.clone(),
            CallInfo {
                caller: self.peers.get().map(|_| {
                    Box::new(Caller {
                        request_id: request_id.clone(),
                        handler: request.handler.clone(),
                        context: request.context.clone(),
                        deadline,
                        trace: TraceContext::from_headers(|name| {
                            request.headers.get(name).map(String::as_str)
                        }),
                    })
                }),
                request_id,
                route: request.route.clone(),
            },
        );
        worker
            .pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending::Call(tx));
        let (frame, stream) = match body {
            CallBody::Bytes(b) => match Frame::with_body(Message::Request(request), b) {
                Ok(f) => (f, None),
                Err(e) => {
                    worker.pending.lock().unwrap().remove(&id);
                    self.release(&worker, &id);
                    return Err(CallError::WorkerFailed(e.to_string()));
                }
            },
            CallBody::Empty => (Frame::new(Message::Request(request)), None),
            CallBody::Stream(body) => {
                request.stream = true;
                let credit = Arc::new(Credit::new());
                worker
                    .credits
                    .lock()
                    .unwrap()
                    .insert(id.clone(), credit.clone());
                (Frame::new(Message::Request(request)), Some((body, credit)))
            }
        };
        if worker.tx.send(Some(frame)).is_err() {
            worker.pending.lock().unwrap().remove(&id);
            self.release(&worker, &id);
            return Err(CallError::WorkerFailed("the worker stopped".into()));
        }
        if let Some((body, credit)) = stream {
            // The pump outlives this call when the handler streams its
            // response while still reading the request. It stops when the
            // body ends or fails, or when the request is released.
            tokio::spawn(self.clone().pump(worker.clone(), id.clone(), body, credit));
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

    /// Sends a streamed request body to the worker as `chunk` messages,
    /// then `end`. Fails the request if the body is too large, stalls or
    /// can't be read.
    async fn pump(
        self: Arc<Self>,
        worker: Arc<Worker>,
        id: String,
        mut body: Body,
        credit: Arc<Credit>,
    ) {
        let error = loop {
            let closed = credit.closed.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if credit.window.is_closed() {
                return;
            }
            let next = tokio::select! {
                _ = closed => return, // answered or cancelled: stop reading
                next = tokio::time::timeout(BODY_IDLE, body.frame()) => next,
            };
            let frame = match next {
                Err(_) => break BodyError::Idle,
                Ok(None) => {
                    if worker.pending.lock().unwrap().contains_key(&id) {
                        let _ = worker.tx.send(Some(Frame::new(Message::End(End { id }))));
                    }
                    return;
                }
                Ok(Some(Err(e))) if e.is::<http_body_util::LengthLimitError>() => {
                    break BodyError::TooLarge;
                }
                Ok(Some(Err(_))) => break BodyError::Read,
                Ok(Some(Ok(frame))) => frame,
            };
            let Ok(mut data) = frame.into_data() else {
                continue; // trailers
            };
            while !data.is_empty() {
                let piece = data.split_to(data.len().min(STREAM_CHUNK));
                // Waits for the worker to read earlier chunks. Closed when
                // the request is released.
                let Ok(permits) = credit.window.acquire_many(piece.len() as u32).await else {
                    return;
                };
                permits.forget();
                credit
                    .outstanding
                    .fetch_add(piece.len() as u64, Ordering::AcqRel);
                if !worker.pending.lock().unwrap().contains_key(&id) {
                    return; // already answered or cancelled
                }
                let chunk = Message::Chunk(Chunk {
                    id: id.clone(),
                    text: None,
                    body_len: None,
                });
                let Ok(frame) = Frame::with_body(chunk, piece) else {
                    return;
                };
                if worker.tx.send(Some(frame)).is_err() {
                    return;
                }
            }
        };
        let pending = worker.pending.lock().unwrap().remove(&id);
        match pending {
            Some(Pending::Call(tx)) => {
                let _ = tx.send(Err(CallError::Body(error)));
            }
            Some(Pending::Stream(tx)) => {
                let _ = tx.try_send(Err(CallError::Body(error).to_string()));
            }
            Some(other) => {
                worker.pending.lock().unwrap().insert(id, other);
                return;
            }
            None => return,
        }
        self.release(&worker, &id);
        let _ = worker.tx.send(Some(Frame::new(Message::Cancel(Cancel {
            id,
            reason: Some(CallError::Body(error).to_string()),
        }))));
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
            // Closing its input asks the worker to exit. The task that
            // reads its output kills it if it hasn't after two seconds.
            let _ = w.tx.send(None);
        }
    }
}

/// Reads each worker's CPU time every [`scale::SAMPLE`], and adds or
/// retires a worker when the [`Scaler`] says so. Ends with the pool.
async fn watch_load(pool: std::sync::Weak<Pool>) {
    let mut scaler = match pool.upgrade() {
        Some(pool) => Scaler::new(pool.runtime.workers.idle),
        None => return,
    };
    // CPU ticks at the last sample, by process, and idle ticks.
    let mut seen: HashMap<u32, u64> = HashMap::new();
    let mut idle_before: Option<u64> = None;
    let mut at = Instant::now();
    let mut calls = 0;
    loop {
        tokio::time::sleep(scale::SAMPLE).await;
        let Some(pool) = pool.upgrade() else { return };
        if pool.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let workers = pool.active();
        // With nothing called and nothing to retire, there's nothing to
        // read either.
        let now_calls = pool.next_id.load(Ordering::Relaxed);
        if now_calls == calls && workers.len() <= pool.min {
            seen.clear();
            idle_before = None;
            at = Instant::now();
            continue;
        }
        calls = now_calls;

        let pids: Vec<Option<u32>> = workers.iter().map(|w| w.pid).collect();
        let (ticks, idle) = tokio::task::spawn_blocking(move || {
            let ticks = pids
                .into_iter()
                .map(|pid| pid.and_then(scale::tree_ticks))
                .collect::<Vec<_>>();
            (ticks, scale::idle_ticks())
        })
        .await
        .unwrap_or_default();
        let now = Instant::now();
        let over = now.duration_since(at);
        at = now;
        let mut next = HashMap::new();
        let loads: Vec<Option<f64>> = workers
            .iter()
            .zip(ticks)
            .map(|(w, ticks)| {
                let (pid, ticks) = (w.pid?, ticks?);
                next.insert(pid, ticks);
                let before = seen.get(&pid)?;
                Some(scale::load(ticks.saturating_sub(*before), over))
            })
            .collect();
        seen = next;
        let idle_cpus = idle
            .zip(idle_before)
            .map(|(now, before)| scale::load(now.saturating_sub(before), over));
        idle_before = idle;
        let sample = Sample {
            loads: &loads,
            idle_cpus,
            waited: pool.waited.swap(false, Ordering::Relaxed),
        };
        match scaler.step(sample, pool.min, pool.max, now) {
            Decision::Grow => pool.grow("its workers were busy"),
            Decision::Shrink => pool.shrink("the others have room for its work"),
            Decision::Hold => {}
        }
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
            self.pool.release(&self.worker, &self.id);
            let _ = self.worker.tx.send(Some(Frame::new(Message::Cancel(Cancel {
                id: self.id.clone(),
                reason: Some("timeout or client disconnected".into()),
            }))));
        }
    }
}

/// A worker's two outputs, which are different types to read from.
enum Output {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

/// A port on this machine that nothing listens on right now.
fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// How soon after starting an `http` runtime's server Unsworn first looks
/// again for it listening, and the longest it then waits between looks.
const LISTEN_FIRST_CHECK: Duration = Duration::from_millis(1);
const LISTEN_CHECK_EVERY: Duration = Duration::from_millis(25);

/// Whether something accepts connections at `target`.
async fn accepts(target: &Target) -> bool {
    match target {
        Target::Unix(path) => tokio::net::UnixStream::connect(&**path).await.is_ok(),
        Target::Origin(origin) => tokio::net::TcpStream::connect(origin.authority.as_str())
            .await
            .is_ok(),
        Target::Url => false,
    }
}

/// Sends a signal to a worker and everything it started.
fn signal_group(pid: Option<u32>, signal: rustix::process::Signal) {
    let Some(pid) = pid.and_then(|p| rustix::process::Pid::from_raw(p as i32)) else {
        return;
    };
    let _ = rustix::process::kill_process_group(pid, signal);
}

/// The shell for an `http` runtime's command: one that, told to stop,
/// tells everything it started.
///
/// The signal sent when Unsworn dies reaches only the process Unsworn
/// started, which is a shell. Some shells become the command they run
/// (bash, for a simple one) and some stay in front of it (dash, always),
/// and a shell that stays ends without a word to the server behind it.
/// This one runs the command in the background and waits, so that it can
/// answer `SIGTERM` by sending it to its whole process group.
///
/// It ignores `SIGTERM` itself from then on. Linux sends the signal for
/// Unsworn's death once for each of Unsworn's threads the shell passes
/// to as they end, so several can arrive together, and a second one that
/// found the shell back at its default would end it before it had told
/// anyone.
///
/// Where there's no such signal (macOS), the shell looks once a second
/// at who its parent is, and stops everything once that isn't Unsworn. A
/// killed Unsworn's children are handed to another process at once,
/// whether or not anyone has collected the dead one, which asking after
/// Unsworn itself would wait for.
fn server_shell(command: &str) -> Command {
    #[cfg(target_os = "linux")]
    const PASS_IT_ON: &str =
        r#"trap 'trap "" TERM; kill -TERM 0; exit 143' TERM; sh -c "$1" & wait $!"#;
    #[cfg(not(target_os = "linux"))]
    const PASS_IT_ON: &str = r#"trap 'trap "" TERM; kill -TERM 0; exit 143' TERM; (while [ "$(ps -o ppid= -p $$ | tr -d ' ')" = "$2" ]; do sleep 1; done; kill -TERM 0) & sh -c "$1" & wait $!"#;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(PASS_IT_ON)
        .arg("sh")
        .arg(command)
        .arg(std::process::id().to_string());
    cmd
}

fn shell(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

/// Logs a worker's stderr (and stdout on Tier 2) line by line. A line that
/// is a JSON object with `level` and `message` (or `msg`) is logged as a
/// structured record, tied to its request when it has an in-flight `id`.
fn spawn_log_reader<R: AsyncRead + Unpin + Send + 'static>(
    stream: R,
    runtime: String,
    worker: String,
    kind: &'static str,
    calls: Calls,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match parse_json_log(&line) {
                Some(mut record) => {
                    let calls = calls.lock().unwrap();
                    let call = match record.fields.get("id") {
                        Some(Value::String(id)) => calls.get(id),
                        _ => None,
                    };
                    if call.is_some() {
                        record.fields.remove("id");
                    }
                    let fields = (!record.fields.is_empty())
                        .then(|| Value::Object(record.fields).to_string());
                    emit_log(
                        record.level,
                        &runtime,
                        &worker,
                        call,
                        &record.message,
                        fields.as_deref(),
                    );
                }
                None => {
                    tracing::info!(target: "worker", runtime = %runtime, worker = %worker, stream = kind, "{line}");
                }
            }
        }
    });
}

struct JsonLog {
    level: LogLevel,
    message: String,
    fields: Map<String, Value>,
}

/// Parses a line written by a JSON logger (pino, structlog, zap, slog...).
fn parse_json_log(line: &str) -> Option<JsonLog> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    let level = match fields.get("level")? {
        Value::String(s) => LogLevel::parse(s)?,
        // pino's numeric levels
        Value::Number(n) => match n.as_u64()? {
            0..=10 => LogLevel::Trace,
            11..=20 => LogLevel::Debug,
            21..=30 => LogLevel::Info,
            31..=40 => LogLevel::Warn,
            _ => LogLevel::Error,
        },
        _ => return None,
    };
    let message = match fields.get("message").or_else(|| fields.get("msg"))? {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    for key in ["level", "message", "msg"] {
        fields.remove(key);
    }
    Some(JsonLog {
        level,
        message,
        fields,
    })
}

fn emit_log(
    level: LogLevel,
    runtime: &str,
    worker: &str,
    call: Option<&CallInfo>,
    message: &str,
    fields: Option<&str>,
) {
    use tracing::field::display;
    let id = call.map(|c| display(&c.request_id));
    let route = call.map(|c| display(&c.route));
    let fields = fields.map(display);
    macro_rules! event {
        ($level:expr) => {
            tracing::event!(target: "worker", $level, runtime = %runtime, worker = %worker, id, route, fields, "{message}")
        };
    }
    match level {
        LogLevel::Error => event!(tracing::Level::ERROR),
        LogLevel::Warn => event!(tracing::Level::WARN),
        LogLevel::Info => event!(tracing::Level::INFO),
        LogLevel::Debug => event!(tracing::Level::DEBUG),
        LogLevel::Trace => event!(tracing::Level::TRACE),
    }
}

/// The most verbose level Unsworn logs, so workers can skip the rest.
fn worker_log_level() -> &'static str {
    use tracing::level_filters::LevelFilter;
    match LevelFilter::current() {
        LevelFilter::OFF => "off",
        LevelFilter::ERROR => "error",
        LevelFilter::WARN => "warn",
        LevelFilter::INFO => "info",
        LevelFilter::DEBUG => "debug",
        _ => "trace",
    }
}

/// Writes a worker's messages to its input, until a `None` says to close
/// it. Dropping the writer is what closes it: the worker reads the end of
/// its input and exits.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Option<Frame>>,
    protocol: Protocol,
) {
    let mut buf = Vec::new();
    while let Some(Some(frame)) = rx.recv().await {
        buf.clear();
        let encoded = match protocol {
            Protocol::Jsonl => proto::jsonl::encode_head(&frame, &mut buf),
            Protocol::Framed | Protocol::Binary => proto::framed::encode_head(&frame, &mut buf),
            Protocol::Http => unreachable!("an http runtime's worker gets no messages"),
        };
        if let Err(e) = encoded {
            tracing::error!("can't encode message: {e}");
            continue;
        }
        // A small body goes out in the same write as its message. A large
        // one is written from where it is, which saves copying it.
        let apart = match &frame.body {
            Some(body) if body.len() >= WRITE_APART => Some(body),
            Some(body) => {
                buf.extend_from_slice(body);
                None
            }
            None => None,
        };
        let mut sent = writer.write_all(&buf).await;
        if let (Ok(()), Some(body)) = (&sent, apart) {
            sent = writer.write_all(body).await;
        }
        if sent.is_err() || writer.flush().await.is_err() {
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
                Protocol::Framed | Protocol::Binary => {
                    Decoder::Framed(proto::framed::Decoder::default())
                }
                Protocol::Http => unreachable!("an http runtime's worker sends no messages"),
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
            // Room for the rest of a frame whose length is known, so a
            // large one is read in a few reads into a buffer that doesn't
            // have to grow (and be copied) on the way.
            let frame_len = match (&self.decoder, self.buf.first_chunk::<4>()) {
                (Decoder::Framed(_), Some(len)) => u32::from_be_bytes(*len) as usize + 4,
                _ => 0,
            };
            let want = frame_len
                .min(proto::DEFAULT_MAX_FRAME)
                .saturating_sub(self.buf.len())
                .max(READ_SPARE);
            if self.buf.capacity() - self.buf.len() < want {
                self.buf.reserve(want);
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
///
/// Whoever connects to a worker's socket first is taken to be the worker,
/// and then sees every request sent to it. So the directory is this
/// process's alone: newly made, under a name nobody could have made in
/// advance, and open to its owner only (`0700`). A worker has to run as
/// the same user as Unsworn to reach its socket.
pub struct SocketDir(tempfile::TempDir);

impl SocketDir {
    pub fn create() -> std::io::Result<SocketDir> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("unsworn-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        Ok(SocketDir(dir))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(test)]
mod tests {
    use super::{CallInfo, CallLog, LOG_GRACE_MAX, SocketDir};
    use std::os::unix::fs::PermissionsExt;

    fn info(n: usize) -> CallInfo {
        CallInfo {
            request_id: format!("req-{n}"),
            route: "/r".into(),
            caller: None,
        }
    }

    #[test]
    fn finished_calls_stay_for_late_logs_but_are_bounded() {
        let mut log = CallLog::default();
        log.start("r-0".into(), info(0));
        log.finish("r-0");
        assert!(
            log.get("r-0").is_some(),
            "kept for log lines that arrive late"
        );

        log.start("live".into(), info(1));
        for n in 1..=LOG_GRACE_MAX + 10 {
            let id = format!("r-{n}");
            log.start(id.clone(), info(n));
            log.finish(&id);
        }
        assert!(
            log.get("r-0").is_none(),
            "the oldest finished calls make way"
        );
        assert!(log.get(&format!("r-{}", LOG_GRACE_MAX + 10)).is_some());
        assert!(
            log.get("live").is_some(),
            "calls in flight are never dropped"
        );
        assert_eq!(log.finished.len(), LOG_GRACE_MAX);
        assert_eq!(log.calls.len(), LOG_GRACE_MAX + 1);
    }

    #[test]
    fn the_socket_directory_is_private_and_removed() {
        let (a, b) = (SocketDir::create().unwrap(), SocketDir::create().unwrap());
        assert_ne!(a.path(), b.path());
        let mode = std::fs::metadata(a.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }
}
