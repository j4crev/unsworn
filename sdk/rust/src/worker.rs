use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::FromRawFd;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_util::{FutureExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use unsworn_proto::{
    Chunk, Continue, DecodeError, End, ErrorMessage, Frame, HeaderValues, Message, MissingHandler,
    PROTOCOL_VERSION, Ready, Response, Verified, framed, jsonl,
};

use crate::handlers::Handlers;
use crate::{Body, Cancel, Reply, Request};

/// Runs the worker until Unsworn closes the connection, then exits the
/// process. Call it from `main`; it starts its own Tokio runtime, with one
/// thread unless `UNSWORN_WORKER_THREADS` asks for more.
pub fn run(handlers: Handlers) -> ! {
    // One thread unless `UNSWORN_WORKER_THREADS` asks for more. Unsworn
    // already runs several workers per runtime, so the cores are used
    // either way, and on several threads every request crosses threads
    // twice (reader to handler task to writer), which costs a small
    // handler more in wakeups than it costs to run.
    let threads = std::env::var("UNSWORN_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let mut builder = match threads {
        Some(threads) if threads > 1 => {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(threads);
            builder
        }
        _ => tokio::runtime::Builder::new_current_thread(),
    };
    let runtime = builder
        .enable_all()
        .build()
        .expect("unsworn: can't start the Tokio runtime");
    let code = match runtime.block_on(serve(handlers)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("unsworn: {e}");
            1
        }
    };
    std::process::exit(code)
}

/// Runs the worker until Unsworn closes the connection, inside an existing
/// Tokio runtime. It reads `UNSWORN_TRANSPORT` to pick Tier 1 (stdio) or
/// Tier 2 (a Unix socket).
///
/// On Tier 1, stdout carries protocol messages, so `serve` points file
/// descriptor 1 at stderr: `println!` output ends up in Unsworn's logs.
pub async fn serve(handlers: Handlers) -> io::Result<()> {
    let transport = std::env::var("UNSWORN_TRANSPORT").unwrap_or_default();
    if let Some(path) = transport.strip_prefix("unix:") {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (read, write) = stream.into_split();
        return run_loop(handlers, read, write, true).await;
    }
    // SAFETY: plain descriptor juggling at startup. The duplicate of fd 1
    // becomes the protocol stream, owned by the File below, and fd 1 then
    // points at stderr.
    let protocol = unsafe {
        let fd = libc::dup(1);
        if fd < 0 || libc::dup2(2, 1) < 0 {
            return Err(io::Error::last_os_error());
        }
        std::fs::File::from_raw_fd(fd)
    };
    // Unsworn connects both with pipes, which can be read and written
    // without blocking. Tokio's `stdin` and `File` hand every read and
    // write to a thread pool instead, a thread hop each way per request,
    // so those are only for a worker started by hand with files.
    use tokio::net::unix::pipe;
    // SAFETY: fd 0 stays open for the life of the process; the duplicate
    // is owned by the File.
    let stdin = unsafe {
        let fd = libc::dup(0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        std::fs::File::from_raw_fd(fd)
    };
    let pipes = (
        stdin.try_clone().and_then(pipe::Receiver::from_file),
        protocol.try_clone().and_then(pipe::Sender::from_file),
    );
    match pipes {
        (Ok(read), Ok(write)) => run_loop(handlers, read, write, false).await,
        _ => {
            run_loop(
                handlers,
                tokio::io::stdin(),
                tokio::fs::File::from_std(protocol),
                false,
            )
            .await
        }
    }
}

#[derive(Clone)]
pub(crate) struct Out {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    framed: bool,
    /// JSON replies go as the raw body (`body_json`), not as `json`.
    raw_json: bool,
}

impl Out {
    pub(crate) fn send(&self, frame: Frame) {
        let mut buf = Vec::new();
        let encoded = if self.framed {
            framed::encode(&frame, &mut buf)
        } else {
            jsonl::encode(&frame, &mut buf)
        };
        match encoded {
            Ok(()) => {
                let _ = self.tx.send(buf);
            }
            Err(e) => eprintln!("unsworn: can't encode a message: {e}"),
        }
    }

    /// Answers a request whose body was sent as JSON and isn't: the `400`
    /// Unsworn gives when it reads such a body itself.
    fn bad_json(&self, id: &str, detail: &str) {
        let mut response = Response::new(id, 400);
        response.headers.insert(
            "content-type".into(),
            HeaderValues::One("application/problem+json".into()),
        );
        response.json = Some(
            serde_json::json!({
                "type": "about:blank",
                "title": "Bad Request",
                "status": 400,
                "detail": detail,
            })
            .into(),
        );
        self.send(Frame::new(Message::Response(response)));
    }

    fn error(&self, id: &str, message: String) {
        self.send(Frame::new(Message::Error(ErrorMessage {
            id: Some(id.to_string()),
            message,
        })));
    }
}

/// Calls to functions that Unsworn hasn't answered yet, and how to make
/// one. Each request holds a copy.
#[derive(Clone)]
pub(crate) struct Calls {
    out: Out,
    /// Set under an Unsworn that answers `call` messages.
    enabled: bool,
    next: Arc<std::sync::atomic::AtomicU64>,
    waiting: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<unsworn_proto::CallResult>>>>,
    /// Set under an Unsworn that answers `fetch` messages.
    can_fetch: bool,
    fetching: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<FetchAnswer>>>>,
}

type FetchAnswer = (unsworn_proto::Fetched, Option<Bytes>);

impl std::fmt::Debug for Calls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Calls")
    }
}

impl Calls {
    pub(crate) async fn fetch(
        &self,
        request: &str,
        service: &str,
        outbound: crate::Outbound,
    ) -> Result<crate::Fetched, crate::FetchError> {
        let fail = |status, message: String| crate::FetchError { status, message };
        if !self.can_fetch {
            return Err(fail(
                501,
                "this Unsworn doesn't make requests for handlers: update it".into(),
            ));
        }
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("f-{n}");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.fetching.lock().unwrap().insert(id.clone(), tx);
        struct Forget<'a>(&'a Calls, &'a str);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.fetching.lock().unwrap().remove(self.1);
            }
        }
        let _forget = Forget(self, &id);
        let message = Message::Fetch(unsworn_proto::Fetch {
            id: id.clone(),
            request: request.to_string(),
            service: service.to_string(),
            method: if outbound.method.is_empty() {
                "GET".into()
            } else {
                outbound.method
            },
            path: outbound.path,
            headers: outbound.headers,
            body_len: None,
        });
        let frame = match outbound.body.filter(|b| !b.is_empty()) {
            Some(body) => Frame::with_body(message, body)
                .map_err(|e| fail(400, format!("the request can't be sent: {e}")))?,
            None => Frame::new(message),
        };
        self.out.send(frame);
        let (fetched, body) = rx
            .await
            .map_err(|_| fail(499, "the request ended before the fetch did".into()))?;
        match fetched.error {
            Some(error) => Err(fail(fetched.status, error)),
            None => Ok(crate::Fetched {
                status: fetched.status,
                headers: fetched.headers,
                body: body.unwrap_or_default(),
            }),
        }
    }

    pub(crate) async fn call(
        &self,
        request: &str,
        function: &str,
        json: serde_json::Value,
    ) -> Result<serde_json::Value, crate::CallError> {
        let fail = |status, message: &str| crate::CallError {
            status,
            message: message.into(),
            json: serde_json::Value::Null,
        };
        if !self.enabled {
            return Err(fail(
                501,
                "this Unsworn doesn't run functions for handlers: update it",
            ));
        }
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("c-{n}");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiting.lock().unwrap().insert(id.clone(), tx);
        // A request that is cancelled drops this future, and with it the
        // place its answer would have gone.
        struct Forget<'a>(&'a Calls, &'a str);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.waiting.lock().unwrap().remove(self.1);
            }
        }
        let _forget = Forget(self, &id);
        self.out.send(Frame::new(Message::Call(unsworn_proto::Call {
            id: id.clone(),
            request: request.to_string(),
            function: function.to_string(),
            json,
        })));
        let result = rx
            .await
            .map_err(|_| fail(499, "the request ended before the call did"))?;
        if result.status >= 400 {
            return Err(crate::CallError {
                status: result.status,
                message: result
                    .error
                    .unwrap_or_else(|| format!("the function answered {}", result.status)),
                json: result.json,
            });
        }
        Ok(result.json)
    }
}

type InFlight = Arc<Mutex<HashMap<String, (AbortHandle, Cancel)>>>;
/// Senders for streamed request bodies, by request id.
type Bodies = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<crate::Result<Bytes>>>>>;

async fn run_loop<R, W>(
    handlers: Handlers,
    mut read: R,
    mut write: W,
    framed: bool,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(buf) = rx.recv().await {
            write.write_all(&buf).await?;
            write.flush().await?;
        }
        io::Result::Ok(())
    });
    // Set by an Unsworn that takes a JSON body as raw bytes, both ways.
    let raw_json = std::env::var("UNSWORN_RAW_JSON").is_ok_and(|v| v == "1");
    let out = Out {
        tx,
        framed,
        raw_json,
    };
    let max_in_flight = std::env::var("UNSWORN_MAX_IN_FLIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(64);
    // Set by an Unsworn that answers `call` messages, and `fetch` ones.
    let can_call = std::env::var("UNSWORN_CALLS").is_ok_and(|v| v == "1");
    let can_fetch = std::env::var("UNSWORN_FETCH").is_ok_and(|v| v == "1");
    out.send(Frame::new(Message::Ready(Ready {
        protocol: PROTOCOL_VERSION,
        max_in_flight: Some(max_in_flight),
        verify: true,
        stream_body: true,
        raw_json,
        raw_body: true,
        calls: can_call,
        fetch: can_fetch,
        sdk: Some(format!("unsworn-rust/{}", crate::VERSION)),
    })));
    crate::logging::install(&out);
    let calls = Calls {
        out: out.clone(),
        enabled: can_call,
        next: Arc::default(),
        waiting: Arc::default(),
        can_fetch,
        fetching: Arc::default(),
    };

    let handlers = Arc::new(handlers);
    let in_flight: InFlight = Arc::default();
    let bodies: Bodies = Arc::default();
    let mut jsonl_decoder = jsonl::Decoder::default();
    let mut framed_decoder = framed::Decoder::default();
    let mut buf = BytesMut::with_capacity(64 * 1024);
    loop {
        let decoded = if framed {
            framed_decoder.decode(&mut buf)
        } else {
            jsonl_decoder.decode(&mut buf)
        };
        let frame = match decoded {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if read.read_buf(&mut buf).await? == 0 {
                    break;
                }
                continue;
            }
            Err(e @ DecodeError::Invalid { .. }) => {
                eprintln!("unsworn: {e}");
                continue;
            }
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        };
        match frame.message {
            Message::Request(req) => start(
                &handlers, &in_flight, &bodies, &out, &calls, req, frame.body,
            ),
            Message::Result(result) => {
                if let Some(tx) = calls.waiting.lock().unwrap().remove(&result.id) {
                    let _ = tx.send(result);
                }
            }
            Message::Fetched(fetched) => {
                if let Some(tx) = calls.fetching.lock().unwrap().remove(&fetched.id) {
                    let _ = tx.send((fetched, frame.body));
                }
            }
            Message::Chunk(chunk) => {
                let data = frame.body.or_else(|| chunk.text.map(Bytes::from));
                if let (Some(tx), Some(data)) = (bodies.lock().unwrap().get(&chunk.id), data) {
                    let _ = tx.send(Ok(data));
                }
            }
            Message::End(end) => {
                bodies.lock().unwrap().remove(&end.id);
            }
            Message::Cancel(cancel) => {
                if let Some(tx) = bodies.lock().unwrap().remove(&cancel.id) {
                    let _ = tx.send(Err(crate::Error::msg("the request was cancelled")));
                }
                if let Some((task, token)) = in_flight.lock().unwrap().remove(&cancel.id) {
                    token.cancel();
                    task.abort();
                }
            }
            Message::Verify(verify) => {
                let missing = verify
                    .handlers
                    .iter()
                    .filter_map(|reference| {
                        handlers
                            .resolve(reference)
                            .err()
                            .map(|reason| MissingHandler {
                                handler: reference.clone(),
                                reason,
                            })
                    })
                    .collect();
                out.send(Frame::new(Message::Verified(Verified {
                    id: verify.id,
                    missing,
                })));
            }
            _ => {}
        }
    }
    crate::logging::uninstall();
    drop(out);
    let _ = writer.await;
    Ok(())
}

fn start(
    handlers: &Arc<Handlers>,
    in_flight: &InFlight,
    bodies: &Bodies,
    out: &Out,
    calls: &Calls,
    msg: unsworn_proto::Request,
    body: Option<Bytes>,
) {
    let cancel = Cancel::default();
    let id = msg.id.clone();
    let stream = msg.stream.then(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        bodies.lock().unwrap().insert(id.clone(), tx);
        crate::BodyStream::new(id.clone(), rx, out.clone())
    });
    // A JSON body sent as the raw bytes after the message is kept as it
    // is, and read if the handler asks for it.
    let (json, body) = match body {
        Some(text) if msg.body_json => (crate::JsonBody::text(text), None),
        body => (
            msg.json.map_or(crate::JsonBody::None, |json| {
                crate::JsonBody::Value(json.into_value())
            }),
            body,
        ),
    };
    let req = Request {
        id: msg.id,
        phase: msg.phase,
        handler: msg.handler,
        route: msg.route,
        method: msg.method,
        path: msg.path,
        params: msg.params,
        query: msg.query,
        raw_query: msg.raw_query,
        headers: msg.headers,
        ctx: msg.context,
        json,
        text: msg.text,
        body,
        stream,
        cancel: cancel.clone(),
        calls: Some(calls.clone()),
    };
    let handlers = handlers.clone();
    let out = out.clone();
    let registry = in_flight.clone();
    let body_registry = bodies.clone();
    let task_id = id.clone();
    let task_cancel = cancel.clone();
    // Holding the lock while spawning means the task can't finish and
    // remove itself before it's been inserted.
    let mut map = in_flight.lock().unwrap();
    let request_id = id.clone();
    let task = tokio::spawn(crate::logging::REQUEST_ID.scope(request_id, async move {
        let id = task_id;
        let result = match handlers.resolve(&req.handler) {
            Ok(handler) => AssertUnwindSafe(handler(req)).catch_unwind().await,
            Err(reason) => Ok(Err(crate::Error::msg(reason))),
        };
        match result {
            Ok(Ok(reply)) => send_reply(&out, &id, reply, &task_cancel).await,
            // Unsworn left the body for the handler to read, so this is
            // the 400 it would have answered with.
            Ok(Err(e)) if e.0.is::<crate::BadJson>() => out.bad_json(&id, &e.to_string()),
            Ok(Err(e)) => {
                eprintln!("unsworn: {e}");
                out.error(&id, e.to_string());
            }
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "handler panicked".into());
                out.error(&id, format!("panic: {message}"));
            }
        }
        registry.lock().unwrap().remove(&id);
        body_registry.lock().unwrap().remove(&id);
    }));
    map.insert(id, (task.abort_handle(), cancel));
}

async fn send_reply(out: &Out, id: &str, reply: Reply, cancel: &Cancel) {
    let status = reply.status;
    let mut headers: BTreeMap<String, HeaderValues> = BTreeMap::new();
    for (name, value) in reply.headers {
        match headers.remove(&name) {
            None => headers.insert(name, HeaderValues::One(value)),
            Some(HeaderValues::One(first)) => {
                headers.insert(name, HeaderValues::Many(vec![first, value]))
            }
            Some(HeaderValues::Many(mut all)) => {
                all.push(value);
                headers.insert(name, HeaderValues::Many(all))
            }
        };
    }
    let mut response = Response::new(id, status);
    response.headers = headers;
    response.mcp_content = reply.mcp_content;
    match reply.body {
        Body::Next(context) => {
            let serde_json::Value::Object(context) = context else {
                out.error(id, "Reply::next takes a JSON object".into());
                return;
            };
            out.send(Frame::new(Message::Continue(Continue {
                id: id.to_string(),
                context,
            })));
        }
        Body::Empty => out.send(Frame::new(Message::Response(response))),
        // Written out here and sent as the raw body, which Unsworn passes
        // on without reading it.
        Body::Json(Ok(value)) if out.raw_json => {
            response.body_json = true;
            let encoded = serde_json::to_vec(&value)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    Frame::with_body(Message::Response(response), text.into())
                        .map_err(|e| e.to_string())
                });
            match encoded {
                Ok(frame) => out.send(frame),
                Err(e) => out.error(id, format!("can't encode the JSON reply: {e}")),
            }
        }
        Body::Json(Ok(value)) => {
            response.json = Some(value.into());
            out.send(Frame::new(Message::Response(response)));
        }
        Body::Json(Err(e)) => out.error(id, format!("can't encode the JSON reply: {e}")),
        Body::Text(text) => {
            response.text = Some(text);
            out.send(Frame::new(Message::Response(response)));
        }
        Body::Bytes(bytes) if bytes.is_empty() => {
            out.send(Frame::new(Message::Response(response)));
        }
        Body::Bytes(bytes) => match Frame::with_body(Message::Response(response), bytes) {
            Ok(frame) => out.send(frame),
            Err(e) => out.error(id, e.to_string()),
        },
        Body::Stream(mut stream) => {
            response.stream = true;
            out.send(Frame::new(Message::Response(response)));
            while let Some(item) = stream.next().await {
                if cancel.is_cancelled() {
                    return;
                }
                match item {
                    Ok(chunk) if chunk.is_empty() => {}
                    Ok(chunk) => {
                        let message = Message::Chunk(Chunk {
                            id: id.to_string(),
                            text: None,
                            body_len: None,
                        });
                        match Frame::with_body(message, chunk) {
                            Ok(frame) => out.send(frame),
                            Err(e) => return out.error(id, e.to_string()),
                        }
                    }
                    Err(e) => return out.error(id, e.to_string()),
                }
            }
            out.send(Frame::new(Message::End(End { id: id.to_string() })));
        }
    }
}
